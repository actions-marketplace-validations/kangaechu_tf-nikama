//! Orchestration tests: apply gating, summaries, dependency-level execution.

use super::pr_event::{
    acquire_locks, all_plans_empty, apply_gate, check_approved, check_required_statuses,
    check_undiverged, closed_pr_rejection, required_checks, set_noop_statuses, summarize,
    sweep_locks, ApplyGate,
};
use super::run::{run_by_levels, JobResult, Outcome, RunContext};
use crate::config::load_from_str;
use crate::engine::{Command, Job, PlanResult, PlanSummary};
use crate::github::client::{CombinedStatus, CompareInfo, ReviewInfo, StatusEntry};
use crate::github::PrInfo;
use crate::storage::{NoopPlanStorage, PlanStorage, StoredPlan};
use std::sync::Arc;

fn pr(merged: bool, state: &str, mergeable: Option<bool>, mergeable_state: Option<&str>) -> PrInfo {
    PrInfo {
        number: 1,
        head_ref: "f".into(),
        head_sha: "s".into(),
        base_ref: "main".into(),
        merged,
        state: state.into(),
        draft: false,
        mergeable,
        mergeable_state: mergeable_state.map(|s| s.to_string()),
        author: "author".into(),
    }
}

fn apply_jobs(skip: bool) -> Vec<Job> {
    apply_jobs_with(skip, None)
}

/// Apply job for one project; `requirements` is the raw `apply_requirements`
/// list (`None` keeps the default).
fn apply_jobs_with(skip: bool, requirements: Option<&str>) -> Vec<Job> {
    let tmp = tempfile::tempdir().unwrap();
    let req = requirements
        .map(|r| format!("    apply_requirements: {}\n", r))
        .unwrap_or_default();
    let c = load_from_str(
        &format!(
            "projects:\n  - name: a\n    dir: a\n    workflow: w\n{}workflows:\n  w:\n    workflow_configuration:\n      skip_merge_check: {}\n",
            req, skip
        ),
        tmp.path(),
    )
    .unwrap();
    let p = &c.projects[0];
    vec![Job::new(
        p,
        c.workflow_for(p),
        Command::Apply,
        Some(1),
        "x",
        "me",
        "o/r",
        "",
        "",
    )]
}

#[test]
fn apply_gate_decisions() {
    let jobs = apply_jobs(false);
    assert_eq!(
        apply_gate(&pr(true, "closed", None, None), &jobs),
        ApplyGate::Allowed
    );
    assert_eq!(
        apply_gate(&pr(false, "open", Some(true), Some("clean")), &jobs),
        ApplyGate::Allowed
    );
    assert_eq!(
        apply_gate(&pr(false, "open", Some(true), Some("unstable")), &jobs),
        ApplyGate::Allowed
    );
    assert_eq!(
        apply_gate(&pr(false, "open", Some(true), Some("has_hooks")), &jobs),
        ApplyGate::Allowed
    );
    assert_eq!(
        apply_gate(&pr(false, "open", Some(false), Some("blocked")), &jobs),
        ApplyGate::CheckStatuses
    );
    assert_eq!(
        apply_gate(&pr(false, "closed", Some(true), Some("clean")), &jobs),
        ApplyGate::Denied("pull request is closed".into())
    );
    assert!(
        matches!(apply_gate(&pr(false, "open", Some(false), Some("dirty")), &jobs), ApplyGate::Denied(m) if m.contains("not mergeable"))
    );
    assert!(
        matches!(apply_gate(&pr(false, "open", None, None), &jobs), ApplyGate::Denied(m) if m.contains("not computed"))
    );
    assert!(
        matches!(apply_gate(&pr(false, "open", Some(true), Some("behind")), &jobs), ApplyGate::Denied(m) if m.contains("behind"))
    );
    // skip_merge_check bypasses the mergeable check on an open PR ...
    assert_eq!(
        apply_gate(
            &pr(false, "open", Some(false), Some("dirty")),
            &apply_jobs(true)
        ),
        ApplyGate::Allowed
    );
    // ... but never the closed-PR check.
    assert_eq!(
        apply_gate(
            &pr(false, "closed", Some(true), Some("clean")),
            &apply_jobs(true)
        ),
        ApplyGate::Denied("pull request is closed".into())
    );
}

#[test]
fn apply_gate_without_mergeable_requirement() {
    // `approved` alone no longer disables gating: mergeable is not checked,
    // but the closed-PR check stays and `approved` is reported as required.
    let jobs = apply_jobs_with(false, Some("[approved]"));
    assert_eq!(
        apply_gate(&pr(false, "open", Some(false), Some("dirty")), &jobs),
        ApplyGate::Allowed
    );
    assert_eq!(
        apply_gate(&pr(false, "closed", None, None), &jobs),
        ApplyGate::Denied("pull request is closed".into())
    );
    let req = required_checks(&jobs);
    assert!(req.contains("approved"));
    assert!(!req.contains("mergeable"));
}

#[test]
fn required_checks_collects_requirements() {
    let req = required_checks(&apply_jobs_with(
        false,
        Some("[approved, mergeable, undiverged]"),
    ));
    assert_eq!(
        req.into_iter().collect::<Vec<_>>(),
        vec!["approved", "mergeable", "undiverged"]
    );
    // skip_merge_check removes only `mergeable`.
    let req = required_checks(&apply_jobs_with(true, Some("[mergeable, undiverged]")));
    assert_eq!(req.into_iter().collect::<Vec<_>>(), vec!["undiverged"]);
    // Non-apply jobs contribute nothing.
    let mut jobs = apply_jobs(false);
    jobs[0].command = Command::Plan;
    assert!(required_checks(&jobs).is_empty());
}

fn review(user: &str, state: &str) -> ReviewInfo {
    ReviewInfo {
        user: user.into(),
        state: state.into(),
    }
}

#[test]
fn approved_requirement() {
    // No reviews at all.
    assert!(check_approved(&[], "author").is_err());
    // Comments only.
    assert!(check_approved(&[review("bob", "COMMENTED")], "author").is_err());
    // One approval.
    assert!(check_approved(&[review("bob", "APPROVED")], "author").is_ok());
    // The author's own approval does not count.
    assert!(check_approved(&[review("author", "APPROVED")], "author").is_err());
    // Changes requested after an approval by the same reviewer block.
    let err = check_approved(
        &[
            review("bob", "APPROVED"),
            review("bob", "CHANGES_REQUESTED"),
        ],
        "author",
    )
    .unwrap_err();
    assert!(err.contains("changes requested by @bob"), "{}", err);
    // Approval after changes requested by the same reviewer resolves it.
    assert!(check_approved(
        &[
            review("bob", "CHANGES_REQUESTED"),
            review("bob", "APPROVED")
        ],
        "author"
    )
    .is_ok());
    // Another reviewer's outstanding request blocks even with an approval.
    assert!(check_approved(
        &[
            review("bob", "APPROVED"),
            review("carol", "CHANGES_REQUESTED")
        ],
        "author"
    )
    .is_err());
    // A dismissed approval no longer counts.
    assert!(check_approved(
        &[review("bob", "APPROVED"), review("bob", "DISMISSED")],
        "author"
    )
    .is_err());
    // A later comment does not revoke an approval.
    assert!(check_approved(
        &[review("bob", "APPROVED"), review("bob", "COMMENTED")],
        "author"
    )
    .is_ok());
}

#[test]
fn undiverged_requirement() {
    assert!(check_undiverged(&CompareInfo {
        ahead_by: 3,
        behind_by: 0
    })
    .is_ok());
    let err = check_undiverged(&CompareInfo {
        ahead_by: 3,
        behind_by: 2,
    })
    .unwrap_err();
    assert!(err.contains("2 commit(s) behind"), "{}", err);
}

#[test]
fn required_statuses_ignore_own_contexts() {
    let combined = CombinedStatus {
        state: "pending".into(),
        statuses: vec![
            StatusEntry {
                context: "digger/apply".into(),
                state: "pending".into(),
            },
            StatusEntry {
                context: "digger/plan".into(),
                state: "failure".into(),
            },
            StatusEntry {
                context: "nikama/plan/x".into(),
                state: "pending".into(),
            },
            StatusEntry {
                context: "ci/test".into(),
                state: "success".into(),
            },
        ],
    };
    assert!(check_required_statuses(&combined, "digger/apply").is_ok());
    let blocked = CombinedStatus {
        state: "pending".into(),
        statuses: vec![StatusEntry {
            context: "ci/lint".into(),
            state: "pending".into(),
        }],
    };
    let err = check_required_statuses(&blocked, "digger/apply").unwrap_err();
    assert!(err.contains("ci/lint (pending)"), "{}", err);
}

fn plan_result(empty: bool) -> JobResult {
    JobResult {
        project: "p".into(),
        command: Command::Plan,
        outcome: Ok(Outcome::Plan(PlanResult {
            summary: PlanSummary {
                create: if empty { 0 } else { 1 },
                ..Default::default()
            },
            plan_text: String::new(),
            plan_json: String::new(),
            plan_file: std::path::PathBuf::new(),
            additional_output: None,
            steps: vec![],
        })),
    }
}

#[test]
fn summarize_counts() {
    let results = vec![
        plan_result(true),
        plan_result(false),
        JobResult {
            project: "q".into(),
            command: Command::Plan,
            outcome: Err("x".into()),
        },
    ];
    assert_eq!(
        summarize(&results, false),
        "2 succeeded, 1 with changes, 1 failed"
    );
    assert_eq!(
        summarize(&results[..1], true),
        "1 succeeded, some projects locked by other PRs"
    );
}

#[tokio::test]
async fn run_by_levels_skips_dependents_of_failed_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Arc::new(
        load_from_str(
            "projects:\n  - name: a\n    dir: missing/a\n  - name: b\n    dir: missing/b\n    depends_on: [a]\n  - name: c\n    dir: missing/c\n",
            tmp.path(),
        )
        .unwrap(),
    );
    let jobs: Vec<Job> = config
        .projects
        .iter()
        .map(|p| {
            Job::new(
                p,
                config.workflow_for(p),
                Command::Apply,
                Some(1),
                "x",
                "me",
                "o/r",
                "",
                "",
            )
        })
        .collect();
    let ctx = Arc::new(RunContext {
        config: config.clone(),
        repo_root: tmp.path().to_path_buf(),
        work_root: tmp.path().join("work"),
        init_lock: None,
        storage: Arc::new(NoopPlanStorage),
        locker: None,
        writer: None,
        max_parallel: 2,
        head_sha: None,
    });
    let mut results = run_by_levels(ctx, jobs, 2).await;
    results.sort_by(|x, y| x.project.cmp(&y.project));
    assert_eq!(results.len(), 3);
    // `a` and `c` fail because their directories do not exist; `b` is skipped.
    assert!(
        results[0].outcome.is_err(),
        "a should fail: {:?}",
        results[0]
    );
    assert!(
        matches!(&results[1].outcome, Ok(Outcome::Skipped(reason)) if reason.contains("dependency a failed"))
    );
    assert!(results[2].outcome.is_err());
}

/// In-memory plan storage that is configured (not noop). Empty by default.
#[derive(Default)]
struct MemoryStorage {
    plans: std::sync::Mutex<std::collections::HashMap<String, StoredPlan>>,
}

impl MemoryStorage {
    fn with(key: &str, head_sha: Option<&str>) -> Self {
        let s = Self::default();
        s.plans.lock().unwrap().insert(
            key.to_string(),
            StoredPlan {
                bytes: b"not-a-real-plan".to_vec(),
                head_sha: head_sha.map(|h| h.to_string()),
            },
        );
        s
    }
}

#[async_trait::async_trait]
impl PlanStorage for MemoryStorage {
    async fn store(&self, key: &str, bytes: &[u8], head_sha: Option<&str>) -> anyhow::Result<()> {
        self.plans.lock().unwrap().insert(
            key.to_string(),
            StoredPlan {
                bytes: bytes.to_vec(),
                head_sha: head_sha.map(|h| h.to_string()),
            },
        );
        Ok(())
    }
    async fn retrieve(&self, key: &str) -> anyhow::Result<Option<StoredPlan>> {
        Ok(self.plans.lock().unwrap().get(key).cloned())
    }
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.plans.lock().unwrap().remove(key);
        Ok(())
    }
    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.plans.lock().unwrap().contains_key(key))
    }
    fn name(&self) -> &'static str {
        "memory"
    }
}

fn run_ctx(
    tmp: &tempfile::TempDir,
    config: &Arc<crate::config::Config>,
    storage: Arc<dyn PlanStorage>,
    head_sha: Option<&str>,
) -> RunContext {
    RunContext {
        config: config.clone(),
        repo_root: tmp.path().to_path_buf(),
        work_root: tmp.path().join("work"),
        init_lock: None,
        storage,
        locker: None,
        writer: None,
        max_parallel: 1,
        head_sha: head_sha.map(|h| h.to_string()),
    }
}

fn demo_fixture(dir_exists: bool) -> (tempfile::TempDir, Arc<crate::config::Config>) {
    let tmp = tempfile::tempdir().unwrap();
    if dir_exists {
        std::fs::create_dir_all(tmp.path().join("infra/demo")).unwrap();
        std::fs::write(
            tmp.path().join("infra/demo/main.tf"),
            "resource \"terraform_data\" \"demo\" {\n  input = \"hello\"\n}\n",
        )
        .unwrap();
    }
    let config = Arc::new(
        load_from_str(
            "projects:\n  - name: demo\n    dir: infra/demo\n",
            tmp.path(),
        )
        .unwrap(),
    );
    (tmp, config)
}

fn demo_apply_job(config: &crate::config::Config) -> Job {
    let p = &config.projects[0];
    Job::new(
        p,
        config.workflow_for(p),
        Command::Apply,
        Some(1),
        "x",
        "me",
        "o/r",
        "",
        "",
    )
}

#[tokio::test]
async fn apply_fails_when_storage_has_no_plan() {
    let (tmp, config) = demo_fixture(false);
    let ctx = run_ctx(
        &tmp,
        &config,
        Arc::new(MemoryStorage::default()),
        Some("abc"),
    );
    let result = super::run::run_job(&ctx, demo_apply_job(&config)).await;
    let err = result.outcome.unwrap_err();
    assert!(err.contains("no stored plan found"), "{}", err);
    assert!(err.contains("o-r-1-demo.tfplan"), "{}", err);
    assert!(err.contains("run `nikama plan` first"), "{}", err);
}

#[tokio::test]
async fn apply_without_storage_plans_first_and_reports_it() {
    if std::process::Command::new("terraform")
        .arg("version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: terraform not found on PATH");
        return;
    }
    let (tmp, config) = demo_fixture(true);
    let ctx = run_ctx(&tmp, &config, Arc::new(NoopPlanStorage), None);
    let result = super::run::run_job(&ctx, demo_apply_job(&config)).await;
    let Ok(Outcome::Apply(a)) = result.outcome else {
        panic!("expected apply outcome: {:?}", result.outcome);
    };
    let pre = a.pre_plan.expect("plan produced before apply");
    assert_eq!(pre.summary.create, 1);
    assert!(pre.plan_file.exists());
    assert!(a.output.contains("Apply complete!"), "{}", a.output);
    assert_eq!(a.summary.map(|s| s.added), Some(1));
}

#[test]
fn stored_plan_commit_check() {
    use super::run::check_stored_plan_commit;
    // Same commit: fine.
    assert!(check_stored_plan_commit("p", Some("abc"), Some("abc")).is_ok());
    // Different commit: refused, with shortened SHAs.
    let err = check_stored_plan_commit(
        "p",
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("fedcba9876543210fedcba9876543210fedcba98"),
    )
    .unwrap_err();
    assert!(err.contains("from commit 0123456789ab"), "{}", err);
    assert!(err.contains("head is fedcba987654"), "{}", err);
    assert!(err.contains("run `nikama plan` first"), "{}", err);
    // Legacy plan without metadata: accepted.
    assert!(check_stored_plan_commit("p", None, Some("abc")).is_ok());
    // Outside PR mode nothing to compare against.
    assert!(check_stored_plan_commit("p", Some("abc"), None).is_ok());
}

#[tokio::test]
async fn apply_refuses_stored_plan_from_another_commit() {
    let (tmp, config) = demo_fixture(false);
    let storage = Arc::new(MemoryStorage::with("o-r-1-demo.tfplan", Some("old-sha")));
    let ctx = run_ctx(&tmp, &config, storage.clone(), Some("new-sha"));
    let result = super::run::run_job(&ctx, demo_apply_job(&config)).await;
    let err = result.outcome.unwrap_err();
    assert!(
        err.contains("stored plan is from commit old-sha"),
        "{}",
        err
    );
    assert!(err.contains("head is new-sha"), "{}", err);
    // The stale plan is kept so the user can inspect it; nothing was applied.
    assert!(storage.exists("o-r-1-demo.tfplan").await.unwrap());
}

#[tokio::test]
async fn apply_accepts_stored_plan_without_commit_metadata() {
    // No terraform in the fixture dir, so the job fails later at `init`;
    // what matters is that the commit check did not reject it.
    let (tmp, config) = demo_fixture(false);
    let storage = Arc::new(MemoryStorage::with("o-r-1-demo.tfplan", None));
    let ctx = run_ctx(&tmp, &config, storage, Some("new-sha"));
    let result = super::run::run_job(&ctx, demo_apply_job(&config)).await;
    let err = result.outcome.unwrap_err();
    assert!(!err.contains("stored plan is from commit"), "{}", err);
    assert!(!err.contains("no stored plan found"), "{}", err);
}

#[tokio::test]
async fn plan_stores_head_sha_with_the_plan_file() {
    if std::process::Command::new("terraform")
        .arg("version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: terraform not found on PATH");
        return;
    }
    let (tmp, config) = demo_fixture(true);
    let storage = Arc::new(MemoryStorage::default());
    let ctx = run_ctx(&tmp, &config, storage.clone(), Some("head-123"));
    let mut job = demo_apply_job(&config);
    job.command = Command::Plan;
    let result = super::run::run_job(&ctx, job).await;
    assert!(
        matches!(result.outcome, Ok(Outcome::Plan(_))),
        "{:?}",
        result.outcome
    );
    let stored = storage
        .retrieve("o-r-1-demo.tfplan")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.head_sha.as_deref(), Some("head-123"));
    assert!(!stored.bytes.is_empty());
}

#[tokio::test]
async fn unhandled_events_are_a_noop() {
    use super::pr_event::build_trigger;
    use crate::github::{GithubClient, GithubContext};
    let tmp = tempfile::tempdir().unwrap();
    let config = load_from_str("projects:\n  - name: a\n    dir: a\n", tmp.path()).unwrap();
    // Never reached for these events; no server behind it.
    let github = GithubClient::new("tok", "o", "r", Some("http://127.0.0.1:9"));
    for event in [
        "push",
        "workflow_run",
        "check_suite",
        "workflow_dispatch",
        "schedule",
    ] {
        let ctx = GithubContext::parse(&format!(
            r#"{{"event_name":"{}","actor":"me","repository":"o/r","run_id":"1","run_attempt":"1","event":{{}}}}"#,
            event
        ))
        .unwrap();
        let result = build_trigger(&config, &ctx, &github).await;
        assert!(
            matches!(result, Ok(None)),
            "{} should be ignored, got {:?}",
            event,
            result.map(|t| t.is_some())
        );
    }
}

// ---------------------------------------------------------------------------
// Lock acquisition (DESIGN.md §9.3) and the dangling-lock sweep (§9.2).

use crate::lock::testing::MemLock;
use crate::lock::{Lock, PrLocker};
use crate::report::RowStatus;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mock_pr_state(server: &MockServer, number: u64, state: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/o/r/pulls/{}", number)))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": {"ref": "x", "sha": "s"}, "base": {"ref": "main", "sha": "b"},
            "merged": false, "state": state, "draft": false
        })))
        .mount(server)
        .await;
}

fn pr_locker(server: &MockServer, backend: Arc<dyn Lock>) -> PrLocker {
    PrLocker::new(
        backend,
        crate::github::GithubClient::new("t", "o", "r", Some(&server.uri())),
    )
}

/// Jobs for projects a, b, c (configuration order) of PR 1 in `o/r`.
fn abc_jobs(command: Command) -> Vec<Job> {
    let tmp = tempfile::tempdir().unwrap();
    let c = load_from_str(
        "projects:\n  - name: a\n    dir: a\n  - name: b\n    dir: b\n  - name: c\n    dir: c\n",
        tmp.path(),
    )
    .unwrap();
    c.projects
        .iter()
        .map(|p| {
            Job::new(
                p,
                c.workflow_for(p),
                command,
                Some(1),
                "x",
                "me",
                "o/r",
                "",
                "",
            )
        })
        .collect()
}

fn names(jobs: &[Job]) -> Vec<&str> {
    jobs.iter().map(|j| j.project.name.as_str()).collect()
}

#[tokio::test]
async fn locks_all_free_are_all_taken() {
    let server = MockServer::start().await;
    let mem = Arc::new(MemLock::default());
    let locker = pr_locker(&server, mem.clone());
    let plan = acquire_locks(&locker, 1, abc_jobs(Command::Plan)).await;
    assert!(!plan.failed);
    assert_eq!(names(&plan.runnable), vec!["a", "b", "c"]);
    assert!(plan.rows.is_empty());
    for r in ["o/r#a", "o/r#b", "o/r#c"] {
        assert_eq!(mem.holder(r).await.unwrap(), Some(1));
    }
}

#[tokio::test]
async fn locks_held_by_another_pr_block_new_acquisition() {
    let server = MockServer::start().await;
    mock_pr_state(&server, 2, "open").await;
    let mem = Arc::new(MemLock::holding(&[("o/r#b", 2)]));
    let locker = pr_locker(&server, mem.clone());
    let plan = acquire_locks(&locker, 1, abc_jobs(Command::Plan)).await;
    assert!(plan.failed);
    assert!(plan.runnable.is_empty(), "{:?}", names(&plan.runnable));
    let rows: Vec<(&str, &RowStatus)> = plan.rows.iter().map(|(n, s, _)| (n.as_str(), s)).collect();
    assert_eq!(rows[0].0, "a");
    assert!(matches!(rows[0].1, RowStatus::Skipped(r) if r == "lock not taken: b is locked by #2"));
    assert_eq!(rows[1], ("b", &RowStatus::LockedByOther(2)));
    assert!(matches!(rows[2].1, RowStatus::Skipped(_)));
    // Nothing was taken for PR 1; PR 2 keeps its lock.
    assert_eq!(mem.holder("o/r#a").await.unwrap(), None);
    assert_eq!(mem.holder("o/r#b").await.unwrap(), Some(2));
    assert_eq!(mem.holder("o/r#c").await.unwrap(), None);
}

#[tokio::test]
async fn locks_already_ours_still_run_when_others_are_blocked() {
    let server = MockServer::start().await;
    mock_pr_state(&server, 2, "open").await;
    let mem = Arc::new(MemLock::holding(&[("o/r#a", 1), ("o/r#b", 2)]));
    let locker = pr_locker(&server, mem.clone());
    let mut jobs = abc_jobs(Command::Plan);
    jobs.push({
        let mut j = jobs[2].clone();
        j.command = Command::Unlock;
        j
    });
    let plan = acquire_locks(&locker, 1, jobs).await;
    assert!(plan.failed);
    // `a` is held from an earlier run, the unlock job needs no lock.
    assert_eq!(names(&plan.runnable), vec!["a", "c"]);
    assert_eq!(plan.runnable[1].command, Command::Unlock);
    assert_eq!(mem.holder("o/r#a").await.unwrap(), Some(1));
    assert_eq!(mem.holder("o/r#c").await.unwrap(), None);
}

/// Backend that looks free but refuses to hand out one resource, simulating
/// another PR taking it between the check and the acquisition.
struct RaceLock {
    inner: MemLock,
    race_on: String,
    other: u64,
    tripped: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl Lock for RaceLock {
    async fn lock(&self, pr: u64, resource: &str) -> anyhow::Result<bool> {
        if resource == self.race_on {
            self.tripped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            return Ok(false);
        }
        self.inner.lock(pr, resource).await
    }
    async fn unlock(&self, resource: &str) -> anyhow::Result<bool> {
        self.inner.unlock(resource).await
    }
    async fn holder(&self, resource: &str) -> anyhow::Result<Option<u64>> {
        if resource == self.race_on && self.tripped.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(Some(self.other));
        }
        self.inner.holder(resource).await
    }
    async fn held_by(&self, pr: u64, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.held_by(pr, prefix).await
    }
    fn name(&self) -> &'static str {
        "race"
    }
}

#[tokio::test]
async fn lost_race_releases_locks_taken_in_this_run() {
    let server = MockServer::start().await;
    mock_pr_state(&server, 2, "open").await;
    let race = Arc::new(RaceLock {
        inner: MemLock::default(),
        race_on: "o/r#b".into(),
        other: 2,
        tripped: Default::default(),
    });
    let locker = pr_locker(&server, race.clone());
    let plan = acquire_locks(&locker, 1, abc_jobs(Command::Plan)).await;
    assert!(plan.failed);
    assert!(plan.runnable.is_empty(), "{:?}", names(&plan.runnable));
    let rows: Vec<(&str, &RowStatus)> = plan.rows.iter().map(|(n, s, _)| (n.as_str(), s)).collect();
    assert_eq!(rows[0], ("b", &RowStatus::LockedByOther(2)));
    assert!(
        matches!(rows[1], ("a", RowStatus::Skipped(r)) if r == "lock released: b is locked by #2")
    );
    assert!(
        matches!(rows[2], ("c", RowStatus::Skipped(r)) if r == "lock not taken: b is locked by #2")
    );
    // `a` was taken and given back; `c` was never attempted.
    assert_eq!(race.inner.holder("o/r#a").await.unwrap(), None);
    assert_eq!(race.inner.holder("o/r#c").await.unwrap(), None);
}

#[tokio::test]
async fn sweep_releases_only_this_prs_other_locks() {
    let server = MockServer::start().await;
    let mem = Arc::new(MemLock::holding(&[
        ("o/r#a", 1),
        ("o/r#eks", 1),
        ("o/r#z", 2),
        ("x/y#a", 1),
    ]));
    let locker = pr_locker(&server, mem.clone());
    let storage = MemoryStorage::with("o-r-1-eks.tfplan", Some("sha"));
    let released = sweep_locks(&locker, &storage, 1, "o/r", &["o/r#a".to_string()])
        .await
        .unwrap();
    assert_eq!(released, vec!["eks".to_string()]);
    assert_eq!(mem.holder("o/r#a").await.unwrap(), Some(1));
    assert_eq!(mem.holder("o/r#eks").await.unwrap(), None);
    assert_eq!(mem.holder("o/r#z").await.unwrap(), Some(2));
    assert_eq!(mem.holder("x/y#a").await.unwrap(), Some(1));
    assert!(!storage.exists("o-r-1-eks.tfplan").await.unwrap());
}

// ---------------------------------------------------------------------------
// Commit statuses when there is nothing to do (DESIGN.md §13).

#[test]
fn all_plans_empty_decisions() {
    assert!(all_plans_empty(&[plan_result(true), plan_result(true)]));
    assert!(!all_plans_empty(&[plan_result(true), plan_result(false)]));
    // A failure anywhere disqualifies.
    assert!(!all_plans_empty(&[
        plan_result(true),
        JobResult {
            project: "b".into(),
            command: Command::Plan,
            outcome: Err("boom".into()),
        }
    ]));
    // Lock / unlock jobs alongside empty plans are fine, but there must be
    // at least one plan.
    let unlocked = JobResult {
        project: "c".into(),
        command: Command::Unlock,
        outcome: Ok(Outcome::Unlocked(true)),
    };
    assert!(all_plans_empty(&[plan_result(true), unlocked]));
    assert!(!all_plans_empty(&[JobResult {
        project: "c".into(),
        command: Command::Unlock,
        outcome: Ok(Outcome::Unlocked(true)),
    }]));
    assert!(!all_plans_empty(&[]));
}

#[tokio::test]
async fn noop_statuses_set_plan_and_apply_to_success() {
    use wiremock::matchers::body_partial_json;
    let server = MockServer::start().await;
    for context in ["digger/plan", "digger/apply"] {
        Mock::given(method("POST"))
            .and(path("/repos/o/r/statuses/abc"))
            .and(body_partial_json(serde_json::json!({
                "state": "success",
                "context": context,
                "description": "no impacted projects",
                "target_url": "https://ci/run/1"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
    }
    let github = crate::github::GithubClient::new("t", "o", "r", Some(&server.uri()));
    set_noop_statuses(&github, "abc", "no impacted projects", "https://ci/run/1").await;
}

#[tokio::test]
async fn push_without_impacted_projects_expects_statuses() {
    use super::pr_event::build_trigger;
    use crate::github::{GithubClient, GithubContext};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/7/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"filename": "README.md"}
        ])))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = load_from_str("projects:\n  - name: a\n    dir: infra/a\n", tmp.path()).unwrap();
    let github = GithubClient::new("tok", "o", "r", Some(&server.uri()));
    let ctx = |action: &str| {
        GithubContext::parse(&format!(
            r#"{{"event_name":"pull_request","actor":"me","repository":"o/r","run_id":"1","run_attempt":"1",
                "event":{{"action":"{}","pull_request":{{"number":7,"draft":false,"merged":false,
                "head":{{"ref":"feat","sha":"abc"}},"base":{{"ref":"main","sha":"def"}}}},
                "repository":{{"default_branch":"main","full_name":"o/r"}},"sender":{{"login":"me"}}}}}}"#,
            action
        ))
        .unwrap()
    };
    let t = build_trigger(&config, &ctx("synchronize"), &github)
        .await
        .unwrap()
        .expect("trigger");
    assert!(t.jobs.is_empty());
    assert_eq!(t.impacted, 0);
    assert_eq!(t.head_sha, "abc");
    assert!(t.expects_plan_or_apply);
    // Closing a PR is not a plan / apply event.
    let t = build_trigger(&config, &ctx("closed"), &github)
        .await
        .unwrap()
        .expect("trigger");
    assert!(!t.expects_plan_or_apply);
    assert!(t.release_all_locks);
}

#[test]
fn comment_commands_on_closed_prs() {
    let open = pr(false, "open", Some(true), Some("clean"));
    let closed = pr(false, "closed", None, None);
    let merged = pr(true, "closed", None, None);
    for c in [
        Command::Plan,
        Command::Apply,
        Command::Lock,
        Command::Unlock,
    ] {
        assert!(closed_pr_rejection(&open, c).is_none(), "{:?}", c);
    }
    let r = closed_pr_rejection(&closed, Command::Plan).unwrap();
    assert!(r.contains("is closed"), "{}", r);
    assert!(r.contains("`plan`"), "{}", r);
    assert!(r.contains("`nikama unlock`"), "{}", r);
    let r = closed_pr_rejection(&merged, Command::Apply).unwrap();
    assert!(r.contains("is merged"), "{}", r);
    assert!(closed_pr_rejection(&merged, Command::Lock).is_some());
    // Unlock is always accepted.
    assert!(closed_pr_rejection(&closed, Command::Unlock).is_none());
    assert!(closed_pr_rejection(&merged, Command::Unlock).is_none());
}

#[tokio::test]
async fn plan_comment_on_merged_pr_is_rejected_with_a_comment() {
    use super::pr_event::build_trigger;
    use crate::github::{GithubClient, GithubContext};
    use wiremock::matchers::body_string_contains;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": {"ref": "feat", "sha": "abc"}, "base": {"ref": "main", "sha": "def"},
            "user": {"login": "alice"},
            "merged": true, "state": "closed", "draft": false
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/7/comments"))
        .and(body_string_contains("this pull request is merged"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = load_from_str("projects:\n  - name: a\n    dir: a\n", tmp.path()).unwrap();
    let github = GithubClient::new("tok", "o", "r", Some(&server.uri()));
    let ctx = GithubContext::parse(
        r#"{"event_name":"issue_comment","actor":"me","repository":"o/r","run_id":"1","run_attempt":"1",
            "event":{"action":"created","issue":{"number":7,"pull_request":{}},
            "comment":{"id":99,"body":"digger plan"},
            "repository":{"default_branch":"main","full_name":"o/r"},"sender":{"login":"me"}}}"#,
    )
    .unwrap();
    let result = build_trigger(&config, &ctx, &github).await;
    assert!(
        matches!(result, Ok(None)),
        "{:?}",
        result.map(|t| t.is_some())
    );
    // The changed-files endpoint was never called: rejection happens first.
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().ends_with("/files")));
}
