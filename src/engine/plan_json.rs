//! Interpretation of `terraform show -json` plans and apply output.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanSummary {
    pub create: u32,
    pub update: u32,
    pub delete: u32,
    pub replace: u32,
    /// Resources brought under management by `import` blocks. An import may
    /// also carry an update, which is counted in `update` as well.
    pub import: u32,
    pub output_changes: u32,
    /// Per-resource rows: (address, action label).
    pub rows: Vec<(String, String)>,
}

impl PlanSummary {
    pub fn is_empty(&self) -> bool {
        self.create == 0
            && self.update == 0
            && self.delete == 0
            && self.replace == 0
            && self.import == 0
            && self.output_changes == 0
    }
    pub fn short(&self) -> String {
        let mut s = format!(
            "+{} ~{} -{} ±{}",
            self.create, self.update, self.delete, self.replace
        );
        if self.import > 0 {
            s.push_str(&format!(" ↓{}", self.import));
        }
        s
    }
}

/// `(imported, added, changed, destroyed)` parsed from apply or destroy output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplySummary {
    pub imported: u32,
    pub added: u32,
    pub changed: u32,
    pub destroyed: u32,
}

#[derive(Debug, Deserialize)]
struct TfPlan {
    #[serde(default)]
    resource_changes: Vec<ResourceChange>,
    #[serde(default)]
    output_changes: BTreeMap<String, Change>,
}

#[derive(Debug, Deserialize)]
struct ResourceChange {
    address: String,
    change: Change,
}

#[derive(Debug, Deserialize)]
struct Change {
    #[serde(default)]
    actions: Vec<String>,
    /// Present when the resource is being imported (`import` block).
    #[serde(default)]
    importing: Option<serde_json::Value>,
}

fn classify(actions: &[String]) -> Option<&'static str> {
    let a: Vec<&str> = actions.iter().map(|s| s.as_str()).collect();
    match a.as_slice() {
        [] | ["no-op"] => None,
        ["read"] => None,
        ["create"] => Some("create"),
        ["update"] => Some("update"),
        ["delete"] => Some("delete"),
        ["delete", "create"] | ["create", "delete"] => Some("replace"),
        _ => Some("update"),
    }
}

/// Parse a plan JSON document into a summary.
pub fn plan_summary_from_json(json: &str) -> Result<PlanSummary, serde_json::Error> {
    let plan: TfPlan = serde_json::from_str(json)?;
    let mut s = PlanSummary::default();
    for rc in &plan.resource_changes {
        let kind = classify(&rc.change.actions);
        if let Some(kind) = kind {
            match kind {
                "create" => s.create += 1,
                "update" => s.update += 1,
                "delete" => s.delete += 1,
                "replace" => s.replace += 1,
                _ => {}
            }
        }
        let importing = rc.change.importing.is_some();
        if importing {
            s.import += 1;
        }
        let label = match (importing, kind) {
            (true, None) => "import".to_string(),
            (true, Some(k)) => format!("import+{}", k),
            (false, Some(k)) => k.to_string(),
            (false, None) => continue,
        };
        s.rows.push((rc.address.clone(), label));
    }
    for c in plan.output_changes.values() {
        if classify(&c.actions).is_some() {
            s.output_changes += 1;
        }
    }
    Ok(s)
}

/// Markdown table of the resource changes.
pub fn plan_summary_table(summary: &PlanSummary) -> String {
    if summary.rows.is_empty() {
        return String::new();
    }
    let mut out = String::from("| Resource | Action |\n|---|---|\n");
    for (addr, action) in &summary.rows {
        let sym: Vec<&str> = action
            .split('+')
            .map(|a| match a {
                "create" => "➕ create",
                "update" => "🔄 update",
                "delete" => "❌ delete",
                "replace" => "♻️ replace",
                "import" => "📥 import",
                other => other,
            })
            .collect();
        out.push_str(&format!("| `{}` | {} |\n", addr, sym.join(", ")));
    }
    if summary.output_changes > 0 {
        out.push_str(&format!(
            "| _outputs_ | {} changed |\n",
            summary.output_changes
        ));
    }
    out
}

/// Summary parsed from apply or destroy output; the last match wins. Handles
/// `Resources: 1 added, 2 changed, 3 destroyed`, the import form
/// `Resources: 4 imported, 1 added, 2 changed, 3 destroyed` and the destroy
/// form `Resources: 3 destroyed`.
pub fn apply_summary_from_output(output: &str) -> Option<ApplySummary> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"Resources: (?:(\d+) imported, )?(?:(\d+) added, (\d+) changed, )?(\d+) destroyed",
        )
        .unwrap()
    });
    let mut last = None;
    for cap in re.captures_iter(output) {
        let num = |i: usize| {
            cap.get(i)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0)
        };
        last = Some(ApplySummary {
            imported: num(1),
            added: num(2),
            changed: num(3),
            destroyed: num(4),
        });
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_plan() {
        let json = r#"{
          "resource_changes": [
            {"address": "aws_s3_bucket.a", "change": {"actions": ["create"]}},
            {"address": "aws_s3_bucket.b", "change": {"actions": ["no-op"]}},
            {"address": "aws_instance.c", "change": {"actions": ["delete", "create"]}},
            {"address": "aws_instance.d", "change": {"actions": ["update"]}},
            {"address": "aws_instance.e", "change": {"actions": ["delete"]}}
          ],
          "output_changes": {"x": {"actions": ["update"]}, "y": {"actions": ["no-op"]}}
        }"#;
        let s = plan_summary_from_json(json).unwrap();
        assert_eq!(
            (s.create, s.update, s.delete, s.replace, s.output_changes),
            (1, 1, 1, 1, 1)
        );
        assert!(!s.is_empty());
        assert!(plan_summary_table(&s).contains("aws_instance.c"));
    }

    #[test]
    fn counts_imports() {
        let json = r#"{
          "resource_changes": [
            {"address": "aws_iam_role.a", "change": {"actions": ["no-op"], "importing": {"id": "a"}}},
            {"address": "aws_iam_role.b", "change": {"actions": ["update"], "importing": {"id": "b"}}},
            {"address": "aws_iam_role.c", "change": {"actions": ["no-op"]}}
          ]
        }"#;
        let s = plan_summary_from_json(json).unwrap();
        assert_eq!((s.import, s.update, s.create), (2, 1, 0));
        assert!(!s.is_empty());
        assert_eq!(s.short(), "+0 ~1 -0 ±0 ↓2");
        let table = plan_summary_table(&s);
        assert!(table.contains("| `aws_iam_role.a` | 📥 import |"));
        assert!(table.contains("| `aws_iam_role.b` | 📥 import, 🔄 update |"));
        assert!(!table.contains("aws_iam_role.c"));
    }

    #[test]
    fn empty_plan() {
        let s = plan_summary_from_json(
            r#"{"resource_changes": [{"address":"a","change":{"actions":["no-op"]}}]}"#,
        )
        .unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn apply_summary() {
        assert_eq!(
            apply_summary_from_output(
                "Apply complete! Resources: 1 added, 2 changed, 3 destroyed."
            ),
            Some(ApplySummary {
                imported: 0,
                added: 1,
                changed: 2,
                destroyed: 3
            })
        );
        assert_eq!(apply_summary_from_output("nothing"), None);
        assert_eq!(
            apply_summary_from_output("Destroy complete! Resources: 4 destroyed."),
            Some(ApplySummary {
                destroyed: 4,
                ..Default::default()
            })
        );
        assert_eq!(
            apply_summary_from_output(
                "Apply complete! Resources: 67 imported, 0 added, 0 changed, 0 destroyed."
            ),
            Some(ApplySummary {
                imported: 67,
                ..Default::default()
            })
        );
    }
}
