//! PR event mode: `pull_request` and `issue_comment` events.

use super::exit;
use super::run::{run_by_levels, run_parallel, JobResult, Outcome, RunContext};
use crate::cli::Env;
use crate::config::{CommentMode, Config};
use crate::engine::{Command, Job};
use crate::github::comment_cmd::HELP_TEXT;
use crate::github::events::{
    classify_pr_event, jobs_for_comment, jobs_for_pull_request, select_projects, PrSelector,
};
use crate::github::{parse_comment, CommentVerb, GithubClient, GithubContext};
use crate::lock::{HolderStatus, LockOutcome, PrLocker};
use crate::report::{ReportWriter, RowStatus, RunReport};
use std::sync::Arc;

pub(super) struct Trigger {
    pub(super) pr_number: u64,
    pub(super) jobs: Vec<Job>,
    /// Does the job set cover every impacted project? (needed for auto-merge)
    pub(super) covers_all_impacted: bool,
    pub(super) notes: Vec<String>,
    pub(super) comment_id: Option<u64>,
    /// Release every lock this PR holds, not only those of impacted projects
    /// (PR closed / merged, or a bare `digger unlock`).
    pub(super) release_all_locks: bool,
    /// Head commit of the PR at the time of the event.
    pub(super) head_sha: String,
    /// Number of projects impacted by the PR.
    pub(super) impacted: usize,
    /// Would this event normally plan or apply? When it does and there is
    /// nothing to do, the commit statuses are still set (DESIGN.md §13).
    pub(super) expects_plan_or_apply: bool,
}

pub async fn run(env: &Env, config: Arc<Config>, ctx: &GithubContext, github: GithubClient) -> i32 {
    let trigger = match build_trigger(&config, ctx, &github).await {
        Ok(Some(t)) => t,
        Ok(None) => return exit::OK,
        Err(code) => return code,
    };
    if trigger.jobs.is_empty() {
        tracing::info!("no jobs to run");
        if trigger.expects_plan_or_apply {
            // A required `digger/plan` or `digger/apply` check must not stay
            // missing (or keep an older failure) just because this commit
            // changes no infrastructure.
            let description = if trigger.impacted == 0 {
                "no impacted projects"
            } else {
                "nothing to run for this event"
            };
            set_noop_statuses(&github, &trigger.head_sha, description, &ctx.run_url()).await;
        }
        let mut notes = trigger.notes.clone();
        if trigger.release_all_locks {
            // A PR that no longer impacts any project may still hold locks
            // from earlier commits; release them on close / unlock.
            match sweep_locks_from_env(&config, &github, trigger.pr_number, &ctx.repository, &[])
                .await
            {
                Ok(released) => notes.extend(released_notes(&released)),
                Err(e) => notes.push(format!("could not release remaining locks: {}", e)),
            }
        }
        if !notes.is_empty() {
            let body = notes
                .iter()
                .map(|n| format!("- {}", n))
                .collect::<Vec<_>>()
                .join("\n");
            let _ = github
                .create_comment(trigger.pr_number, &format!("**nikama**\n\n{}", body))
                .await;
        }
        return exit::OK;
    }

    if let Some(id) = trigger.comment_id {
        let _ = github.add_reaction(id, "eyes").await;
    }

    // All jobs in one run share a command (PR events may mix e.g. plan+lock;
    // the status context follows the "heaviest" command).
    let main_command = trigger
        .jobs
        .iter()
        .map(|j| j.command)
        .max_by_key(|c| match c {
            Command::Destroy => 4,
            Command::Apply => 3,
            Command::Plan => 2,
            Command::Lock => 1,
            Command::Unlock => 0,
        })
        .unwrap_or(Command::Plan);

    let pr = match github.get_pr(trigger.pr_number).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "could not fetch pull request");
            return exit::INIT;
        }
    };

    // Report + status.
    let marker = match config.comment.mode {
        CommentMode::Latest => "<!-- nikama:latest -->".to_string(),
        _ => format!(
            "<!-- nikama:run={}:attempt={} -->",
            ctx.run_id, ctx.run_attempt
        ),
    };
    let mut report = RunReport::new(
        &marker,
        main_command,
        &ctx.run_url(),
        config.comment.show_empty_plans,
    );
    for j in &trigger.jobs {
        report.row_mut(&j.project.name);
    }
    report.notes = trigger.notes.clone();
    let (writer, writer_task) = ReportWriter::start(
        github.clone(),
        trigger.pr_number,
        report,
        config.comment.mode,
    );

    let status_context = main_command.status_context();
    let _ = github
        .set_status(
            &pr.head_sha,
            "pending",
            status_context,
            &format!("running {} project(s)", trigger.jobs.len()),
            Some(&ctx.run_url()),
        )
        .await;

    // Locking (two-phase).
    let locker = match crate::lock::from_env(config.pr_locks).await {
        Ok(backend) => Arc::new(PrLocker::new(backend, github.clone())),
        Err(e) => {
            tracing::error!(error = %e, "could not initialise lock provider");
            writer.update(move |r| r.notes.push(format!("lock provider error: {}", e)));
            writer.finish().await;
            writer_task.join().await;
            let _ = github
                .set_status(
                    &pr.head_sha,
                    "failure",
                    status_context,
                    "lock provider error",
                    Some(&ctx.run_url()),
                )
                .await;
            return exit::LOCK_INIT;
        }
    };
    let keep_locks: Vec<String> = trigger.jobs.iter().map(|j| j.lock_id()).collect();
    let plan = acquire_locks(&locker, trigger.pr_number, trigger.jobs).await;
    let lock_failed = plan.failed;
    for (name, status, detail) in plan.rows {
        writer.update(move |r| {
            let row = r.row_mut(&name);
            row.status = status;
            if let Some(d) = detail {
                row.detail_kind = Some(crate::report::DetailKind::Error);
                row.detail = Some(d);
            }
        });
    }
    let runnable = plan.runnable;

    // Apply preconditions.
    let has_apply = runnable.iter().any(|j| matches!(j.command, Command::Apply));
    if has_apply {
        if let Err(reason) = apply_allowed(&github, &pr, &runnable, status_context).await {
            tracing::error!(%reason, "apply not allowed");
            let r2 = reason.clone();
            writer.update(move |r| {
                r.notes.push(format!("apply not allowed: {}", r2));
                for row in r.rows.values_mut() {
                    if row.status == RowStatus::Pending {
                        row.status = RowStatus::Skipped("apply not allowed".into());
                    }
                }
            });
            writer.finish().await;
            writer_task.join().await;
            let _ = github
                .set_status(
                    &pr.head_sha,
                    "failure",
                    status_context,
                    &reason,
                    Some(&ctx.run_url()),
                )
                .await;
            return exit::JOB_FAILED;
        }
    }

    let storage = match crate::storage::from_env().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "could not initialise plan storage");
            writer.update(move |r| r.notes.push(format!("plan storage error: {}", e)));
            writer.finish().await;
            writer_task.join().await;
            let _ = github
                .set_status(
                    &pr.head_sha,
                    "failure",
                    status_context,
                    "plan storage error",
                    Some(&ctx.run_url()),
                )
                .await;
            return exit::INIT;
        }
    };

    let run_ctx = Arc::new(RunContext {
        config: config.clone(),
        repo_root: env.repo_root.clone(),
        work_root: env.work_root.clone(),
        init_lock: env.init_lock(),
        storage,
        locker: Some(locker.clone()),
        writer: Some(writer.clone()),
        max_parallel: env.max_parallel.unwrap_or(config.concurrency.max_parallel),
        head_sha: Some(pr.head_sha.clone()),
    });

    let (serial, parallel): (Vec<Job>, Vec<Job>) = runnable
        .into_iter()
        .partition(|j| matches!(j.command, Command::Apply | Command::Destroy));
    let mut results: Vec<JobResult> = run_parallel(run_ctx.clone(), parallel).await;
    if !serial.is_empty() {
        results.extend(
            run_by_levels(
                run_ctx.clone(),
                serial,
                config.concurrency.apply_parallelism,
            )
            .await,
        );
    }

    if trigger.release_all_locks {
        match sweep_locks(
            &locker,
            run_ctx.storage.as_ref(),
            trigger.pr_number,
            &ctx.repository,
            &keep_locks,
        )
        .await
        {
            Ok(released) => {
                let notes = released_notes(&released);
                if !notes.is_empty() {
                    writer.update(move |r| r.notes.extend(notes));
                }
            }
            Err(e) => {
                let msg = format!("could not release remaining locks: {}", e);
                writer.update(move |r| r.notes.push(msg));
            }
        }
    }

    writer.finish().await;
    writer_task.join().await;

    let failed = lock_failed || results.iter().any(|r| !r.is_ok());
    let applied = results
        .iter()
        .filter(|r| matches!(r.outcome, Ok(Outcome::Apply(_))))
        .count();
    let description = summarize(&results, lock_failed);
    let _ = github
        .set_status(
            &pr.head_sha,
            if failed { "failure" } else { "success" },
            status_context,
            &description,
            Some(&ctx.run_url()),
        )
        .await;

    // Nothing to apply: let a required `digger/apply` check pass. When any
    // project has changes the apply context is left untouched, so it stays
    // missing on this commit until an apply runs.
    if main_command == Command::Plan && trigger.covers_all_impacted && all_plans_empty(&results) {
        let _ = github
            .set_status(
                &pr.head_sha,
                "success",
                Command::Apply.status_context(),
                "no changes to apply",
                Some(&ctx.run_url()),
            )
            .await;
    }

    write_outputs(trigger.pr_number);

    if !failed && applied > 0 && config.auto_merge && trigger.covers_all_impacted {
        auto_merge(&github, &config, trigger.pr_number).await;
    }

    if failed {
        exit::JOB_FAILED
    } else {
        exit::OK
    }
}

/// `true` when every job succeeded, at least one was a plan, and no plan has
/// changes (DESIGN.md §13: `digger/apply` may be set to success).
pub(super) fn all_plans_empty(results: &[JobResult]) -> bool {
    let mut plans = 0;
    for r in results {
        match &r.outcome {
            Ok(Outcome::Plan(p)) if p.is_empty() => plans += 1,
            Ok(Outcome::Plan(_)) => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    plans > 0
}

/// Set both `digger/plan` and `digger/apply` to success on `sha` for a run
/// that has nothing to do.
pub(super) async fn set_noop_statuses(
    github: &GithubClient,
    sha: &str,
    description: &str,
    run_url: &str,
) {
    for context in [
        Command::Plan.status_context(),
        Command::Apply.status_context(),
    ] {
        if let Err(e) = github
            .set_status(sha, "success", context, description, Some(run_url))
            .await
        {
            tracing::warn!(context, error = %e, "could not set commit status");
        }
    }
}

pub(super) fn summarize(results: &[JobResult], lock_failed: bool) -> String {
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let failed = results.iter().filter(|r| !r.is_ok()).count();
    let changes = results
        .iter()
        .filter(|r| matches!(&r.outcome, Ok(Outcome::Plan(p)) if !p.is_empty()))
        .count();
    let mut parts = vec![format!("{} succeeded", ok)];
    if changes > 0 {
        parts.push(format!("{} with changes", changes));
    }
    if failed > 0 {
        parts.push(format!("{} failed", failed));
    }
    if lock_failed {
        parts.push("some projects locked by other PRs".into());
    }
    parts.join(", ")
}

pub(super) async fn build_trigger(
    config: &Config,
    ctx: &GithubContext,
    github: &GithubClient,
) -> Result<Option<Trigger>, i32> {
    match ctx.event_name.as_str() {
        "pull_request" | "pull_request_target" => {
            let event = ctx.pull_request_event().map_err(|e| {
                tracing::error!(error = %e, "could not parse pull_request event");
                exit::CONTEXT
            })?;
            let selector = classify_pr_event(&event, config);
            if let PrSelector::Ignore(reason) = &selector {
                tracing::info!(%reason, "ignoring event");
                return Ok(None);
            }
            let pr_number = event.pull_request.number;
            let files = github.changed_files(pr_number).await.map_err(|e| {
                tracing::error!(error = %e, "could not list changed files");
                exit::EVENT
            })?;
            let impacted = crate::config::impacted_projects(config, &files);
            log_impacted(&impacted);
            let jobs = jobs_for_pull_request(&event, config, &impacted, &selector);
            let release_all_locks =
                matches!(selector, PrSelector::Closed | PrSelector::CommitToDefault);
            Ok(Some(Trigger {
                pr_number,
                jobs,
                covers_all_impacted: true,
                notes: vec![],
                comment_id: None,
                release_all_locks,
                head_sha: event.pull_request.head.sha.clone(),
                impacted: impacted.len(),
                expects_plan_or_apply: selector == PrSelector::Pushed,
            }))
        }
        "issue_comment" => {
            let event = ctx.issue_comment_event().map_err(|e| {
                tracing::error!(error = %e, "could not parse issue_comment event");
                exit::CONTEXT
            })?;
            if event.action != "created" {
                tracing::info!(action = %event.action, "ignoring issue_comment action");
                return Ok(None);
            }
            if event.issue.pull_request.is_none() {
                tracing::info!("comment is on an issue, not a pull request; ignoring");
                return Ok(None);
            }
            let Some(parsed) = parse_comment(&event.comment.body) else {
                tracing::info!("comment is not a nikama command; ignoring");
                return Ok(None);
            };
            let pr_number = event.issue.number;
            let cmd = match parsed {
                Ok(c) => c,
                Err(e) => {
                    let _ = github
                        .create_comment(pr_number, &format!("**nikama**: {}\n\n{}", e, HELP_TEXT))
                        .await;
                    return Err(exit::COMMAND);
                }
            };
            let command = match cmd.verb {
                CommentVerb::Help => {
                    let _ = github.create_comment(pr_number, HELP_TEXT).await;
                    return Ok(None);
                }
                CommentVerb::Run(Command::Destroy) => {
                    let _ = github
                        .create_comment(
                            pr_number,
                            "**nikama**: `destroy` cannot be run from a PR comment. Use a manually dispatched workflow with `mode: manual` and `command: nikama destroy`.",
                        )
                        .await;
                    return Err(exit::COMMAND);
                }
                CommentVerb::Run(c) => c,
            };
            let pr = github.get_pr(pr_number).await.map_err(|e| {
                tracing::error!(error = %e, "could not fetch pull request");
                exit::EVENT
            })?;
            let files = github.changed_files(pr_number).await.map_err(|e| {
                tracing::error!(error = %e, "could not list changed files");
                exit::EVENT
            })?;
            let impacted = crate::config::impacted_projects(config, &files);
            log_impacted(&impacted);
            let (selected, unmatched) = select_projects(&impacted, &cmd.projects, &cmd.dirs);
            let mut notes = Vec::new();
            if !unmatched.is_empty() {
                notes.push(format!(
                    "selector(s) matched no impacted project: {}",
                    unmatched.join(", ")
                ));
            }
            if impacted.is_empty() {
                notes.push("no projects are impacted by this pull request".into());
            }
            let covers_all = selected.len() == impacted.len();
            let jobs = jobs_for_comment(&event, config, &selected, command, &pr.head_ref);
            // A bare `digger unlock` releases everything this PR holds; with
            // `-p` / `-d` only the selected projects are touched.
            let release_all_locks =
                command == Command::Unlock && cmd.projects.is_empty() && cmd.dirs.is_empty();
            Ok(Some(Trigger {
                pr_number,
                jobs,
                covers_all_impacted: covers_all,
                notes,
                comment_id: Some(event.comment.id),
                release_all_locks,
                head_sha: pr.head_sha.clone(),
                impacted: impacted.len(),
                expects_plan_or_apply: matches!(command, Command::Plan | Command::Apply),
            }))
        }
        "merge_group" => {
            tracing::info!("ignoring merge_group event");
            Ok(None)
        }
        // Events this action does not handle are a no-op, not a failure: a
        // workflow with broader triggers (`push`, `workflow_run`, ...) must
        // not go red because of us. Dispatch / schedule without a mode is a
        // configuration mistake, so say what is missing, but still exit 0.
        "workflow_dispatch" | "schedule" => {
            tracing::warn!(
                event = %ctx.event_name,
                "ignoring {} event in PR mode; set `mode: manual` (workflow_dispatch) or `mode: drift-detection` (schedule) to run it",
                ctx.event_name
            );
            Ok(None)
        }
        other => {
            tracing::warn!(event = other, "ignoring event not handled in PR mode");
            Ok(None)
        }
    }
}

/// Result of the lock phase: which jobs may run and what to show for the rest.
pub(super) struct LockPlan {
    pub runnable: Vec<Job>,
    /// (project, status, error detail) for jobs that will not run.
    pub rows: Vec<(String, RowStatus, Option<String>)>,
    pub failed: bool,
}

fn lockable(job: &Job) -> bool {
    matches!(
        job.command,
        Command::Plan | Command::Apply | Command::Destroy
    )
}

/// Take the locks for `jobs` (DESIGN.md §9.3).
///
/// Locks are checked first and then acquired in job order, which follows the
/// configuration order, so two PRs contending for the same projects always
/// try them in the same sequence. When any project is held by another open
/// PR, nothing new is acquired and anything acquired in this run is released
/// again; only jobs whose lock this PR already held from an earlier run keep
/// running. Jobs that need no lock (`lock` / `unlock`) always run.
pub(super) async fn acquire_locks(locker: &PrLocker, pr: u64, jobs: Vec<Job>) -> LockPlan {
    /// What phase 1 learned about one job.
    enum Pre {
        /// Needs no lock (`lock` / `unlock` jobs).
        NoLock,
        Ours,
        Free,
        Other(u64),
        /// Backend error; the row is already recorded.
        Error,
    }

    let mut plan = LockPlan {
        runnable: Vec::new(),
        rows: Vec::new(),
        failed: false,
    };

    // Phase 1: look, do not touch.
    let mut pre: Vec<Pre> = Vec::with_capacity(jobs.len());
    let mut blocked: Vec<(String, u64)> = Vec::new();
    for job in &jobs {
        if !lockable(job) {
            pre.push(Pre::NoLock);
            continue;
        }
        match locker.status(pr, &job.lock_id()).await {
            Ok(HolderStatus::Ours) => pre.push(Pre::Ours),
            Ok(HolderStatus::Free) => pre.push(Pre::Free),
            Ok(HolderStatus::Other(other)) => {
                blocked.push((job.project.name.clone(), other));
                pre.push(Pre::Other(other));
            }
            Err(e) => {
                plan.failed = true;
                plan.rows.push((
                    job.project.name.clone(),
                    RowStatus::Failed,
                    Some(format!("lock error: {}", e)),
                ));
                pre.push(Pre::Error);
            }
        }
    }

    if !blocked.is_empty() {
        plan.failed = true;
        let why = blocked
            .iter()
            .map(|(p, other)| format!("{} is locked by #{}", p, other))
            .collect::<Vec<_>>()
            .join(", ");
        for (job, st) in jobs.into_iter().zip(pre) {
            match st {
                Pre::NoLock | Pre::Ours => plan.runnable.push(job),
                Pre::Error => {}
                Pre::Other(other) => {
                    tracing::warn!(project = %job.project.name, other, "project locked by another PR");
                    plan.rows.push((
                        job.project.name.clone(),
                        RowStatus::LockedByOther(other),
                        None,
                    ));
                }
                Pre::Free => plan.rows.push((
                    job.project.name.clone(),
                    RowStatus::Skipped(format!("lock not taken: {}", why)),
                    None,
                )),
            }
        }
        return plan;
    }

    // Phase 2: acquire in order; a lost race rolls back this run's locks.
    let mut acquired_now: Vec<(usize, String)> = Vec::new(); // (index in runnable, resource)
    let mut lost: Option<(String, u64)> = None;
    let mut it = jobs.into_iter().zip(pre);
    for (job, st) in it.by_ref() {
        match st {
            Pre::NoLock | Pre::Ours => plan.runnable.push(job),
            Pre::Error | Pre::Other(_) => {}
            Pre::Free => match locker.lock(pr, &job.lock_id()).await {
                Ok(LockOutcome::Acquired) => {
                    acquired_now.push((plan.runnable.len(), job.lock_id()));
                    plan.runnable.push(job);
                }
                Ok(LockOutcome::AlreadyHeld) => plan.runnable.push(job),
                Ok(LockOutcome::HeldByOther(other)) => {
                    tracing::warn!(project = %job.project.name, other, "lost the race for a project lock");
                    lost = Some((job.project.name.clone(), other));
                    plan.rows.push((
                        job.project.name.clone(),
                        RowStatus::LockedByOther(other),
                        None,
                    ));
                    break;
                }
                Err(e) => {
                    plan.failed = true;
                    plan.rows.push((
                        job.project.name.clone(),
                        RowStatus::Failed,
                        Some(format!("lock error: {}", e)),
                    ));
                }
            },
        }
    }
    if let Some((name, other)) = lost {
        plan.failed = true;
        let why = format!("{} is locked by #{}", name, other);
        // Roll back what this run took.
        let mut released = std::collections::BTreeSet::new();
        for (idx, resource) in &acquired_now {
            if let Err(e) = locker.unlock(pr, resource).await {
                tracing::warn!(resource, error = %e, "could not release a lock taken in this run");
            }
            released.insert(*idx);
        }
        let kept: Vec<Job> = std::mem::take(&mut plan.runnable);
        for (idx, job) in kept.into_iter().enumerate() {
            if released.contains(&idx) {
                plan.rows.push((
                    job.project.name.clone(),
                    RowStatus::Skipped(format!("lock released: {}", why)),
                    None,
                ));
            } else {
                plan.runnable.push(job);
            }
        }
        // Not attempted.
        for (job, st) in it {
            match st {
                Pre::NoLock | Pre::Ours => plan.runnable.push(job),
                _ => plan.rows.push((
                    job.project.name.clone(),
                    RowStatus::Skipped(format!("lock not taken: {}", why)),
                    None,
                )),
            }
        }
    }
    plan
}

/// Release every lock under `namespace` held by `pr` except `keep`
/// (DESIGN.md §9.2, dangling locks). Returns the released project names.
pub(super) async fn sweep_locks(
    locker: &PrLocker,
    storage: &dyn crate::storage::PlanStorage,
    pr: u64,
    namespace: &str,
    keep: &[String],
) -> anyhow::Result<Vec<String>> {
    let prefix = format!("{}#", namespace);
    let mut released = Vec::new();
    for resource in locker.held_by(pr, &prefix).await? {
        if keep.contains(&resource) {
            continue;
        }
        let project = resource
            .strip_prefix(&prefix)
            .unwrap_or(&resource)
            .to_string();
        if locker.unlock(pr, &resource).await? {
            tracing::info!(
                project,
                pr,
                "released lock of a project that is no longer impacted"
            );
            if let Err(e) = storage
                .delete(&crate::engine::plan_file_name(
                    namespace,
                    Some(pr),
                    &project,
                ))
                .await
            {
                tracing::warn!(project, error = %e, "failed to delete stored plan");
            }
            released.push(project);
        }
    }
    Ok(released)
}

/// `sweep_locks` for the path where no jobs run (lock backend and storage
/// are created here).
async fn sweep_locks_from_env(
    config: &Config,
    github: &GithubClient,
    pr: u64,
    namespace: &str,
    keep: &[String],
) -> anyhow::Result<Vec<String>> {
    let backend = crate::lock::from_env(config.pr_locks).await?;
    let locker = PrLocker::new(backend, github.clone());
    let storage = crate::storage::from_env().await?;
    sweep_locks(&locker, storage.as_ref(), pr, namespace, keep).await
}

fn released_notes(released: &[String]) -> Vec<String> {
    released
        .iter()
        .map(|p| {
            format!(
                "released lock of `{}` (no longer impacted by this pull request)",
                p
            )
        })
        .collect()
}

fn log_impacted(impacted: &[crate::config::Project]) {
    if impacted.is_empty() {
        tracing::info!("no impacted projects");
    } else {
        tracing::info!(
            projects = %impacted.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
            "impacted projects"
        );
    }
}

/// Result of the network-free part of the apply precondition check.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ApplyGate {
    Allowed,
    Denied(String),
    /// Branch protection reports `blocked`; the commit statuses decide.
    CheckStatuses,
}

/// Union of `apply_requirements` over the apply jobs. A job whose workflow
/// sets `skip_merge_check` contributes no `mergeable` requirement.
pub(super) fn required_checks(jobs: &[Job]) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for j in jobs.iter().filter(|j| j.command == Command::Apply) {
        for r in &j.project.apply_requirements {
            if r == "mergeable" && j.skip_merge_check {
                continue;
            }
            out.insert(r.clone());
        }
    }
    out
}

/// The `mergeable` part of the apply preconditions (network-free).
///
/// A closed, unmerged PR is always denied. A merged PR is always allowed
/// (apply after merge). Otherwise the GitHub mergeability of the PR decides,
/// unless no apply job requires `mergeable`.
pub(super) fn apply_gate(pr: &crate::github::PrInfo, jobs: &[Job]) -> ApplyGate {
    if pr.state == "closed" && !pr.merged {
        return ApplyGate::Denied("pull request is closed".into());
    }
    if pr.merged {
        return ApplyGate::Allowed;
    }
    if !required_checks(jobs).contains("mergeable") {
        return ApplyGate::Allowed;
    }
    match (pr.mergeable, pr.mergeable_state.as_deref()) {
        (Some(true), Some("clean"))
        | (Some(true), Some("unstable"))
        | (Some(true), Some("has_hooks")) => ApplyGate::Allowed,
        (_, Some("blocked")) => ApplyGate::CheckStatuses,
        (Some(false), state) => ApplyGate::Denied(format!(
            "pull request is not mergeable (state: {})",
            state.unwrap_or("unknown")
        )),
        (None, state) => ApplyGate::Denied(format!(
            "GitHub has not computed mergeability yet (state: {}); retry in a moment",
            state.unwrap_or("unknown")
        )),
        (Some(true), state) => ApplyGate::Denied(format!(
            "pull request is not in a mergeable state ({})",
            state.unwrap_or("unknown")
        )),
    }
}

/// When branch protection blocks the PR, accept if every *other* commit
/// status is green (our own contexts are ignored).
pub(super) fn check_required_statuses(
    combined: &crate::github::client::CombinedStatus,
    own_context: &str,
) -> Result<(), String> {
    let bad: Vec<String> = combined
        .statuses
        .iter()
        .filter(|s| {
            s.context != own_context
                && !s.context.starts_with("digger/")
                && !s.context.starts_with("nikama/")
        })
        .filter(|s| s.state != "success")
        .map(|s| format!("{} ({})", s.context, s.state))
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "pull request is blocked by required checks: {}",
            bad.join(", ")
        ))
    }
}

/// `approved` requirement: the latest non-dismissed review of every reviewer
/// other than the PR author is considered; at least one must be `APPROVED`
/// and none may be `CHANGES_REQUESTED`. `COMMENTED` / `PENDING` reviews do not
/// change a reviewer's verdict.
pub(super) fn check_approved(
    reviews: &[crate::github::client::ReviewInfo],
    author: &str,
) -> Result<(), String> {
    let mut latest: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for r in reviews {
        if r.user == author {
            continue;
        }
        match r.state.as_str() {
            "APPROVED" | "CHANGES_REQUESTED" => {
                latest.insert(r.user.as_str(), r.state.as_str());
            }
            // A dismissed review no longer counts for that reviewer.
            "DISMISSED" => {
                latest.remove(r.user.as_str());
            }
            _ => {}
        }
    }
    let blocking: Vec<&str> = latest
        .iter()
        .filter(|(_, s)| **s == "CHANGES_REQUESTED")
        .map(|(u, _)| *u)
        .collect();
    if !blocking.is_empty() {
        return Err(format!(
            "changes requested by {}",
            blocking
                .iter()
                .map(|u| format!("@{}", u))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if latest.values().any(|s| *s == "APPROVED") {
        Ok(())
    } else {
        Err("pull request has no approving review".into())
    }
}

/// `undiverged` requirement: the PR head must not be behind its base branch.
pub(super) fn check_undiverged(cmp: &crate::github::client::CompareInfo) -> Result<(), String> {
    if cmp.behind_by == 0 {
        Ok(())
    } else {
        Err(format!(
            "pull request is {} commit(s) behind the base branch; update the branch first",
            cmp.behind_by
        ))
    }
}

async fn apply_allowed(
    github: &GithubClient,
    pr: &crate::github::PrInfo,
    jobs: &[Job],
    own_context: &str,
) -> Result<(), String> {
    let mut reasons: Vec<String> = Vec::new();
    match apply_gate(pr, jobs) {
        ApplyGate::Allowed => {}
        // A closed PR fails every other check as well; stop here.
        ApplyGate::Denied(reason) if pr.state == "closed" => return Err(reason),
        ApplyGate::Denied(reason) => reasons.push(reason),
        ApplyGate::CheckStatuses => {
            let combined = github
                .combined_status(&pr.head_sha)
                .await
                .map_err(|e| format!("could not read commit status: {}", e))?;
            if let Err(reason) = check_required_statuses(&combined, own_context) {
                reasons.push(reason);
            }
        }
    }
    if !pr.merged {
        let required = required_checks(jobs);
        if required.contains("approved") {
            let reviews = github
                .list_reviews(pr.number)
                .await
                .map_err(|e| format!("could not list reviews: {}", e))?;
            if let Err(reason) = check_approved(&reviews, &pr.author) {
                reasons.push(reason);
            }
        }
        if required.contains("undiverged") {
            let cmp = github
                .compare(&pr.base_ref, &pr.head_sha)
                .await
                .map_err(|e| format!("could not compare with the base branch: {}", e))?;
            if let Err(reason) = check_undiverged(&cmp) {
                reasons.push(reason);
            }
        }
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons.join("; "))
    }
}

async fn auto_merge(github: &GithubClient, config: &Config, pr_number: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let pr = match github.get_pr(pr_number).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "auto-merge: could not fetch PR");
            return;
        }
    };
    if pr.merged {
        tracing::info!("auto-merge: PR already merged");
        return;
    }
    match github.combined_status(&pr.head_sha).await {
        Ok(c) if c.state == "success" => {}
        Ok(c) => {
            tracing::warn!(state = %c.state, "auto-merge: combined status is not success; skipping");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "auto-merge: could not read combined status");
            return;
        }
    }
    if pr.mergeable != Some(true) {
        tracing::warn!("auto-merge: PR is not mergeable; skipping");
        return;
    }
    match github
        .merge_pr(
            pr_number,
            config.auto_merge_strategy.as_github_method(),
            &pr.head_sha,
            "auto-merge",
        )
        .await
    {
        Ok(()) => tracing::info!("auto-merge: PR merged"),
        Err(e) => tracing::error!(error = %e, "auto-merge: merge failed"),
    }
}

fn write_outputs(pr_number: u64) {
    use std::io::Write;
    if let Some(path) = crate::util::env_opt("GITHUB_OUTPUT") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            let _ = writeln!(f, "pr_number={}", pr_number);
        }
    }
    // Digger compatibility: it exported DIGGER_PR_NUMBER via GITHUB_ENV.
    if let Some(path) = crate::util::env_opt("GITHUB_ENV") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            let _ = writeln!(f, "DIGGER_PR_NUMBER={}", pr_number);
        }
    }
}
