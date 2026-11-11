//! Discovery of local modules referenced by a project (`autoplan_modules`).
//!
//! The `*.tf` / `*.tofu` files of a project directory are scanned for
//! `module "x" { source = "../path" }` blocks whose `source` is a local path
//! (`./...` or `../...`). Referenced directories are followed transitively, so
//! a module that itself uses another local module is covered too. Registry,
//! git and HTTP sources, `*.tf.json` files and sources that are not string
//! literals are ignored.

use super::impact::normalize_path;
use regex::Regex;
use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

static MODULE_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*module\s+"[^"]*"\s*\{"#).unwrap());
static SOURCE_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*source\s*=\s*"([^"]*)""#).unwrap());
/// `source` attribute on the same line as the block opening
/// (`module "x" { source = "../y" }`).
static SOURCE_INLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[\s{;])source\s*=\s*"([^"]*)""#).unwrap());

const IAC_EXTENSIONS: [&str; 2] = [".tf", ".tofu"];

/// Local module directories (repository-relative, normalised, sorted) that
/// the project at `project_dir` references directly or transitively, not
/// counting directories inside the project itself. `root` is the repository
/// root. Missing directories are skipped.
pub fn local_module_dirs(root: &Path, project_dir: &str) -> Vec<String> {
    let project_dir = normalize_path(project_dir);
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    visited.insert(project_dir.clone());
    queue.push_back(project_dir.clone());
    while let Some(dir) = queue.pop_front() {
        for source in module_sources_in_dir(&root.join(&dir)) {
            let Some(resolved) = resolve_local(&dir, &source) else {
                tracing::debug!(dir, source, "module source escapes the repository; ignored");
                continue;
            };
            if !root.join(&resolved).is_dir() {
                tracing::debug!(
                    dir,
                    source,
                    "module source does not exist in the checkout; ignored"
                );
                continue;
            }
            if visited.insert(resolved.clone()) {
                queue.push_back(resolved);
            }
        }
    }
    visited
        .into_iter()
        .filter(|d| !is_within(d, &project_dir))
        .collect()
}

fn is_within(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || path.starts_with(&format!("{}/", dir))
}

/// Resolve a local module `source` against `dir`; `None` when it is not a
/// local path or would leave the repository.
fn resolve_local(dir: &str, source: &str) -> Option<String> {
    if !(source.starts_with("./") || source.starts_with("../")) {
        return None;
    }
    let source = source.replace('\\', "/");
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in source.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// `source` values of every `module` block in the `*.tf` / `*.tofu` files
/// directly inside `dir` (file order is not significant).
fn module_sources_in_dir(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            IAC_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        match std::fs::read_to_string(&f) {
            Ok(text) => out.extend(module_sources(&text)),
            Err(e) => tracing::warn!(path = %f.display(), error = %e, "could not read file"),
        }
    }
    out
}

/// Extract `source = "..."` string literals from `module` blocks. A small
/// line-based scanner: braces outside string literals track block depth so
/// that `source` attributes of nested blocks or other block types are not
/// picked up. Comments (`#`, `//`, `/* */`) are skipped.
pub fn module_sources(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth: usize = 0;
    let mut module_depth: Option<usize> = None;
    let mut in_block_comment = false;
    for raw_line in text.lines() {
        let line = if in_block_comment {
            match raw_line.find("*/") {
                Some(i) => {
                    in_block_comment = false;
                    &raw_line[i + 2..]
                }
                None => continue,
            }
        } else {
            raw_line
        };
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let line = match trimmed.find("/*") {
            Some(i) if !trimmed.contains("*/") => {
                in_block_comment = true;
                &trimmed[..i]
            }
            _ => trimmed,
        };
        if module_depth.is_none() && MODULE_BLOCK.is_match(line) {
            module_depth = Some(depth);
            let rest = &line[line.find('{').map(|i| i + 1).unwrap_or(line.len())..];
            if let Some(c) = SOURCE_INLINE.captures(rest) {
                out.push(c[1].to_string());
            }
        } else if let Some(d) = module_depth {
            if depth == d + 1 {
                if let Some(c) = SOURCE_ATTR.captures(line) {
                    out.push(c[1].to_string());
                }
            }
        }
        let (opens, closes) = count_braces(line);
        depth = depth.saturating_add(opens).saturating_sub(closes);
        if let Some(d) = module_depth {
            if depth <= d {
                module_depth = None;
            }
        }
    }
    out
}

/// Count `{` and `}` outside double-quoted string literals. Interpolation
/// braces inside strings (`"${...}"`) are ignored with the string.
fn count_braces(line: &str) -> (usize, usize) {
    let mut opens = 0;
    let mut closes = 0;
    let mut in_string = false;
    let mut escaped = false;
    for ch in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => opens += 1,
            '}' => closes += 1,
            _ => {}
        }
    }
    (opens, closes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_sources_from_module_blocks_only() {
        let text = r#"
# module "commented" { source = "../no" }
module "vpc" {
  source = "../../modules/vpc"
  cidr   = "10.0.0.0/16"
  providers = { aws = aws.main }
  tags = { Name = "x" }
}

module "registry" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "5.0.0"
}

resource "archive_file" "zip" {
  source = "./lambda.zip"
}

module "nested" {
  source = "./local"
  config = {
    source = "./not-a-module"
  }
}
/* module "block" {
  source = "../comment"
} */
module "one_line" { source = "../oneline" }
module "interp" {
  name   = "${var.prefix}-{x}"
  source = "../interp"
}
"#;
        assert_eq!(
            module_sources(text),
            vec![
                "../../modules/vpc",
                "terraform-aws-modules/vpc/aws",
                "./local",
                "../oneline",
                "../interp",
            ]
        );
    }

    #[test]
    fn resolves_local_sources() {
        assert_eq!(
            resolve_local("infra/app", "../../modules/vpc"),
            Some("modules/vpc".into())
        );
        assert_eq!(
            resolve_local("infra/app", "./sub"),
            Some("infra/app/sub".into())
        );
        assert_eq!(resolve_local("", "./modules/x"), Some("modules/x".into()));
        assert_eq!(resolve_local("infra", "../../x"), None);
        assert_eq!(resolve_local("infra", "modules/x"), None);
        assert_eq!(resolve_local("infra", "git::https://x/y.git"), None);
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn follows_local_modules_transitively() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            root,
            "infra/app/main.tf",
            "module \"a\" {\n  source = \"../../modules/a\"\n}\nmodule \"sub\" {\n  source = \"./sub\"\n}\nmodule \"gone\" {\n  source = \"../../modules/missing\"\n}\n",
        );
        write(
            root,
            "infra/app/sub/main.tf",
            "module \"c\" {\n  source = \"../../../modules/c\"\n}\n",
        );
        write(
            root,
            "modules/a/main.tofu",
            "module \"b\" {\n  source = \"../b\"\n}\nmodule \"loop\" {\n  source = \"../../infra/app\"\n}\n",
        );
        write(root, "modules/b/main.tf", "");
        write(root, "modules/c/main.tf", "");
        assert_eq!(
            local_module_dirs(root, "./infra/app/"),
            vec!["modules/a", "modules/b", "modules/c"]
        );
        assert!(local_module_dirs(root, "modules/b").is_empty());
        assert!(local_module_dirs(root, "does/not/exist").is_empty());
    }
}
