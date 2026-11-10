//! Loading the configuration from a git ref through the GitHub API instead
//! of the working tree, so that a pull request cannot change the
//! configuration it is run with.

use super::{load_from_str, ConfigError, Loaded, CONFIG_FILE_NAMES, LEGACY_CONFIG_FILE_NAMES};
use crate::github::GithubClient;
use std::path::{Path, PathBuf};

/// Fetch the configuration file from `git_ref` and parse it. `explicit` is
/// the repository-relative path given by `--config` / `NIKAMA_CONFIG`;
/// without it the usual names are tried in order. `dir` is still the local
/// repository root, used for `generate_projects` directory discovery.
pub async fn load_from_ref(
    github: &GithubClient,
    git_ref: &str,
    explicit: Option<&Path>,
    dir: &Path,
) -> Result<Loaded, ConfigError> {
    let candidates: Vec<(String, bool)> = match explicit {
        Some(p) => {
            if p.is_absolute() {
                return Err(ConfigError::Remote(format!(
                    "the configuration path must be relative to the repository when config-ref is set, got {}",
                    p.display()
                )));
            }
            vec![(p.to_string_lossy().replace('\\', "/"), false)]
        }
        None => CONFIG_FILE_NAMES
            .iter()
            .map(|n| (n.to_string(), false))
            .chain(
                LEGACY_CONFIG_FILE_NAMES
                    .iter()
                    .map(|n| (n.to_string(), true)),
            )
            .collect(),
    };
    for (name, legacy_name) in &candidates {
        let text = github
            .get_file(name, git_ref)
            .await
            .map_err(|e| ConfigError::Remote(format!("{}@{}: {}", name, git_ref, e)))?;
        let Some(text) = text else {
            continue;
        };
        let label = PathBuf::from(format!("{}@{}", name, git_ref));
        let config = load_from_str(&text, dir).map_err(|e| match e {
            ConfigError::Yaml { source, .. } => ConfigError::Yaml {
                path: label.clone(),
                source,
            },
            other => other,
        })?;
        return Ok(Loaded {
            config,
            path: label,
            legacy_name: *legacy_name,
        });
    }
    Err(ConfigError::Remote(format!(
        "no configuration file found on {} (looked for {})",
        git_ref,
        candidates
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn tries_names_in_order_and_parses_the_first_hit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/nikama.yml"))
            .and(query_param("ref", "main"))
            .respond_with(ResponseTemplate::new(404).set_body_string("{\"message\":\"Not Found\"}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/nikama.yaml"))
            .and(query_param("ref", "main"))
            .and(header("accept", "application/vnd.github.raw+json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("projects:\n  - name: a\n    dir: a\n"),
            )
            .mount(&server)
            .await;
        let github = GithubClient::new("tok", "o", "r", Some(&server.uri()));
        let tmp = tempfile::tempdir().unwrap();
        let loaded = load_from_ref(&github, "main", None, tmp.path())
            .await
            .unwrap();
        assert_eq!(loaded.config.projects[0].name, "a");
        assert_eq!(loaded.path.to_string_lossy(), "nikama.yaml@main");
        assert!(!loaded.legacy_name);
    }

    #[tokio::test]
    async fn legacy_name_and_not_found() {
        let server = MockServer::start().await;
        for name in ["nikama.yml", "nikama.yaml", "digger.yml"] {
            Mock::given(method("GET"))
                .and(path(format!("/repos/o/r/contents/{}", name)))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/digger.yaml"))
            .and(query_param("ref", "v1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("projects:\n  - name: b\n    dir: b\n"),
            )
            .mount(&server)
            .await;
        let github = GithubClient::new("tok", "o", "r", Some(&server.uri()));
        let tmp = tempfile::tempdir().unwrap();
        let loaded = load_from_ref(&github, "v1", None, tmp.path())
            .await
            .unwrap();
        assert!(loaded.legacy_name);

        let err = load_from_ref(&github, "other", None, tmp.path())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ConfigError::Remote(ref m) if m.contains("no configuration file found on other")),
            "{}",
            err
        );
    }

    #[tokio::test]
    async fn explicit_path_is_used_verbatim() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/infra/custom.yml"))
            .and(query_param("ref", "main"))
            .respond_with(ResponseTemplate::new(200).set_body_string("projects: []\n"))
            .expect(1)
            .mount(&server)
            .await;
        let github = GithubClient::new("tok", "o", "r", Some(&server.uri()));
        let tmp = tempfile::tempdir().unwrap();
        let loaded = load_from_ref(
            &github,
            "main",
            Some(Path::new("infra/custom.yml")),
            tmp.path(),
        )
        .await
        .unwrap();
        assert!(loaded.config.projects.is_empty());

        let err = load_from_ref(&github, "main", Some(Path::new("/abs.yml")), tmp.path())
            .await
            .unwrap_err();
        assert!(matches!(err, ConfigError::Remote(_)), "{}", err);
    }
}
