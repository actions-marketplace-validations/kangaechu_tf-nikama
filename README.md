# tf-nikama

<p align="center">
  <img src="docs/images/tf-nikama.png" alt="tf-nikama logo: a steaming stainless pot of boiling tofu" width="400">
</p>

Backendless Terraform / OpenTofu automation for GitHub pull requests, as a single
small binary that runs inside GitHub Actions.

`nikama` is a Rust re-implementation of the *backendless mode* of
[Digger](https://github.com/diggerhq/digger) (`no-backend: true`). It keeps the
parts that work without an orchestrator backend, drops everything else, and
fixes the pain points of the original:

| | Digger backendless | tf-nikama |
|---|---|---|
| Binary | ~115 MB Go binary with 3 cloud SDKs, OPA, terragrunt | small Rust binary, AWS SDK only |
| Plans | strictly sequential | parallel per project (`terraform init` serialised for plugin-cache safety) |
| PR comments | several comments per project, no size handling | one comment per run, updated in place, split at the 65,536-char limit |
| Config | `digger.yml` | `nikama.yml` (same schema; unsupported keys fail fast) |

## Scope

Supported in the first release:

- **GitHub** `pull_request` and `issue_comment` events, `workflow_dispatch` (manual) and `schedule` (drift)
- **Terraform** and **OpenTofu**
- **AWS**: DynamoDB PR locks (schema-compatible with Digger), S3 plan storage,
  GitHub OIDC → STS role assumption, Cognito Identity
- **Google Cloud**: Cloud Storage PR locks and plan storage (compatible with Digger's `gcp` lock and plan providers)
- Comment commands: `digger plan|apply|lock|unlock|help` (also `nikama ...`), with `-p/--project`, `-d/--directory` and `plan --no-lock`; on a closed or merged PR only `unlock` is accepted
- Commit statuses `digger/plan` and `digger/apply`, optional auto-merge
- Drift detection with Slack webhook or GitHub Issue notifications

Not supported (and rejected at config load): Terragrunt, Pulumi, Azure
lock or plan storage, GitHub Artifacts plan storage, OPA policies, Digger
backend features.

There are currently no plans to support anything beyond Terraform / OpenTofu
with AWS or Google Cloud. Other IaC tools and cloud providers are out of
scope for the foreseeable future.

## Quick start

`.github/workflows/nikama.yml`:

```yaml
name: nikama
on:
  pull_request:
    types: [opened, synchronize, reopened, closed, ready_for_review, converted_to_draft]
  issue_comment:
    types: [created]

permissions:
  contents: read
  pull-requests: write
  statuses: write
  id-token: write

concurrency:
  group: nikama-${{ github.event.pull_request.number || github.event.issue.number }}
  cancel-in-progress: false

jobs:
  nikama:
    runs-on: ubuntu-latest
    # Skip the runner for comments from outside the repository; nikama checks
    # the commenter's write access itself (see "Who can run commands").
    if: >-
      github.event_name != 'issue_comment'
      || contains(fromJSON('["OWNER", "MEMBER", "COLLABORATOR"]'), github.event.comment.author_association)
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event_name == 'issue_comment' && format('refs/pull/{0}/merge', github.event.issue.number) || '' }}
      - uses: hashicorp/setup-terraform@v3
        with:
          terraform_wrapper: false
      - uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-assume: arn:aws:iam::123456789012:role/nikama
          aws-region: ap-northeast-1
      - uses: kangaechu/tf-nikama@v0.2.0
        with:
          upload-plan-destination: aws
          upload-plan-destination-s3-bucket: my-plan-bucket
```

On Google Cloud, replace the AWS credentials step with
`google-github-actions/auth` and point the action at Cloud Storage buckets
(the service account needs `roles/storage.objectUser` on both):

```yaml
      - uses: google-github-actions/auth@v2
        with:
          workload_identity_provider: projects/123456789012/locations/global/workloadIdentityPools/github/providers/github
          service_account: nikama@my-project.iam.gserviceaccount.com
      - uses: kangaechu/tf-nikama@v0.2.0
        with:
          lock-provider: gcp
          google-lock-bucket: my-lock-bucket
          upload-plan-destination: gcp
          upload-plan-destination-gcp-bucket: my-plan-bucket
```

`nikama.yml` at the repository root:

```yaml
projects:
  - name: network
    dir: infra/network
  - name: app
    dir: infra/app
    depends_on: [network]

concurrency:
  max_parallel: 4
```

See [`examples/nikama.yml`](examples/nikama.yml) for the full schema.

Coming from Digger? See [`docs/MIGRATION.md`](docs/MIGRATION.md).

## How a run works

1. The event is mapped to commands using each project's
   `workflow_configuration` (defaults: push → `digger plan`, close / merge → `digger unlock`).
   With `allow_draft_prs: false` (the default) a draft pull request runs
   nothing and takes no locks: pushes to it are ignored, converting an open
   pull request to a draft releases its locks (`converted_to_draft`), and
   comment commands other than `digger unlock` are refused until it is marked
   ready for review.
2. Impacted projects are computed from the PR's changed files
   (`dir/**`, `include_patterns`, `exclude_patterns`, plus downstream
   `depends_on` projects in `hard` mode).
3. One run comment is posted immediately with every project as *pending*, and
   the commit status is set to *pending*.
4. PR locks are checked for all projects up front and taken in configuration
   order. If any project is held by another open PR, no new lock is taken:
   the held projects are reported as *locked by #N*, the others as skipped,
   and only projects this PR already holds still run. On close / merge and
   on a bare `digger unlock`, every lock the PR still holds is released, not
   only those of the currently impacted projects.
   A plan can skip locking altogether: `digger plan --no-lock` on a comment,
   or a pull request carrying the label named in `no_lock_label` (set it to
   the label your Renovate / Dependabot configuration adds). Such a plan runs
   even for projects held by another PR and releases any lock this PR holds
   for the planned projects, so a pull request that only needs a look at its
   plan, for example a Terraform or provider version bump across every
   directory, blocks nobody. `digger apply` takes the locks as usual; a
   stored plan is still bound to the commit it was produced from.
5. Before an apply, each project's `apply_requirements` are checked against
   the PR: `mergeable` (GitHub reports the PR as mergeable; when branch
   protection reports it as *blocked*, the required reviews must be in
   place and every required check run or status other than nikama's own
   must have passed, so that a required `digger/apply` check does not block
   itself), `approved` (at least one approving review from someone other
   than the author and no outstanding "changes requested"), `undiverged`
   (the branch is not behind its base). The default is `[mergeable]`; a
   closed, unmerged PR is always rejected.
6. Plans run in parallel (`concurrency.max_parallel`); applies run level by
   level in dependency order. The comment is edited as projects finish.
   An apply always uses a plan file: the stored one when plan storage is
   configured (a missing stored plan fails the job instead of applying
   blind), otherwise a plan produced immediately before the apply, whose
   summary is shown in the comment. Stored plans record the PR head commit
   they were produced from; a plan from a different commit is refused.
7. The commit status is set to success / failure; `auto_merge` merges the PR
   when every apply succeeded. A push that impacts no project sets both
   `digger/plan` and `digger/apply` to success, and a plan run in which no
   project has changes also sets `digger/apply` to success, so either can be
   used as a required check without blocking PRs that have nothing to apply.

### Who can run commands

A run executes the pull request's `nikama.yml` and Terraform code (providers
included) with the workflow's credentials, so starting one is equivalent to
running code in the repository. Comment commands (`nikama plan`, `nikama
apply`, ...) are therefore only accepted from users whose repository role is
`write`, `maintain` or `admin`, checked through the collaborator permission
API at run time. A command from anyone else is answered with a short comment
and the workflow exits 0. `nikama help` is answered for everyone.

The `if:` guard in the quick start is only an optimisation that avoids
starting a runner for comments from outside the repository: `COLLABORATOR`
and `MEMBER` include read-only users, so the permission check inside nikama
is the one that counts.

`pull_request` events need no such check: GitHub gives workflows triggered by
pull requests from forks a read-only token and no OIDC token, and branches in
the repository itself can only be pushed by users with write access.

### Trusting the configuration

By default `nikama.yml` is read from the checkout, which on pull request runs
is the pull request's own version. Two inputs tighten that:

- `config-ref: default` reads the configuration from the repository's default
  branch (or any other git ref) through the GitHub API, so a pull request runs
  with the reviewed configuration: `run:` steps, workflows, `apply_requirements`
  and `aws_role_to_assume` cannot be changed by the PR itself. Changes to
  `nikama.yml` take effect once merged; a PR that adds a new project is
  planned after it lands. `generate_projects` still discovers directories in
  the checkout. The input only applies to pull request runs; manual and drift
  runs read the checkout.
- `allow-run-steps: false` refuses any configuration that contains a `run:`
  step, from whichever source it was read.

```yaml
      - uses: kangaechu/tf-nikama@v0.2.0
        with:
          config-ref: default
          allow-run-steps: "false"
```

### Restricting providers

`terraform plan` runs provider binaries, so a pull request that adds a
provider (`hashicorp/external`, anything from another registry or namespace)
runs code of its choosing with the job's credentials. `allowed-providers`
limits what can be installed:

```yaml
      - uses: kangaechu/tf-nikama@v0.2.0
        with:
          allowed-providers: "hashicorp/aws, hashicorp/random, app.terraform.io/myorg/*"
```

Entries are provider source addresses, `namespace/type` meaning
`registry.terraform.io/namespace/type`, and `*` may replace any whole
segment. nikama turns the list into a Terraform CLI configuration
(`provider_installation` with a `direct` method and that `include` list) and
passes it to every `terraform` / `tofu` invocation through
`TF_CLI_CONFIG_FILE`. `init` then refuses any provider outside the list,
including providers required by child modules, before anything is
downloaded or executed. Built-in providers (`terraform_data`) are unaffected.

- `nikama.yml` may also carry a top-level `allowed_providers:` list. It can
  only narrow the workflow input: entries not covered by `allowed-providers`
  are ignored with a warning, and a list with nothing left is an error.
  Without the workflow input the `nikama.yml` list applies on its own, which
  is only meaningful together with `config-ref`.
- An existing CLI configuration (`TF_CLI_CONFIG_FILE`, `~/.terraformrc`,
  `~/.tofurc`) is kept and extended. If it already has a
  `provider_installation` block (a filesystem or network mirror), put the
  `include` list there instead and leave `allowed-providers` unset.
  `TF_PLUGIN_CACHE_DIR` keeps working.

### What protects against whom

| Trigger | PR author | What runs their code | Protection |
|---|---|---|---|
| `issue_comment` | anyone (fork, read-only) | nothing: commands need write access | write-access check (built in) |
| `issue_comment` by a maintainer on an untrusted PR | fork, read-only | the PR's Terraform code; its `nikama.yml` unless `config-ref` | `config-ref`, `allow-run-steps`, `allowed-providers` |
| `pull_request` | fork | nothing useful: read-only token, no OIDC token, no secrets | GitHub |
| `pull_request` | write access, same repository | the PR's workflow file, so everything | not solvable in nikama: trust write access, or drop `on.pull_request` and use comment commands with branch protection, or gate the cloud role on a protected GitHub Environment |

With comment-only triggering, `config-ref: default` and `allowed-providers`
hold against users with write access too: changing either requires a merge
to the default branch.

## Configuration file

`nikama.yml` (or `nikama.yaml`). A `digger.yml` is still read as a fallback
with a warning. Unknown keys and keys for unsupported features are errors.

Top-level keys: `projects`, `workflows`, `generate_projects`,
`dependency_configuration`, `traverse_to_nested_projects`, `auto_merge`,
`auto_merge_strategy`, `pr_locks`, `allow_draft_prs`, `no_lock_label`,
`drift_include_patterns`, `drift_exclude_patterns`, `concurrency`, `comment`
(plus `git_timeout` and `telemetry`, accepted for compatibility).

## Environment variables

The action sets these for you; when running the binary directly:

| Variable | Purpose |
|---|---|
| `GITHUB_TOKEN`, `GITHUB_CONTEXT`, `GITHUB_REPOSITORY` | required for PR mode |
| `INPUT_DIGGER_MODE` | `` / `manual` / `drift-detection` |
| `INPUT_DIGGER_COMMAND`, `INPUT_DIGGER_PROJECT` | manual mode |
| `LOCK_PROVIDER` (`aws`/`gcp`/`noop`), `NIKAMA_LOCK_TABLE`, `GOOGLE_STORAGE_LOCK_BUCKET`, `DISABLE_LOCKING` | PR locks |
| `PLAN_UPLOAD_DESTINATION` (`aws`/`gcp`), `AWS_S3_BUCKET`, `PLAN_UPLOAD_S3_*`, `GOOGLE_STORAGE_PLAN_ARTEFACT_BUCKET` | plan storage |
| `REPORTING_STRATEGY` | `comments_per_run` / `latest_run_comment` / `per_project` |
| `INPUT_DRIFT_DETECTION_SLACK_NOTIFICATION_URL`, `INPUT_DRIFT_GITHUB_ISSUES` | drift notifications |
| `NIKAMA_CONFIG`, `NIKAMA_MAX_PARALLEL`, `NIKAMA_LOG_LEVEL` | CLI options |
| `NIKAMA_CONFIG_REF`, `NIKAMA_ALLOW_RUN_STEPS` | trusted configuration (see "Trusting the configuration") |
| `NIKAMA_ALLOWED_PROVIDERS` | provider allowlist (see "Restricting providers") |
| `TF_PLUGIN_CACHE_DIR` | when set, `init` steps run one at a time |

Validate a configuration locally:

```sh
nikama --validate --local
```

## Exit codes

`0` ok / nothing to do, `1` configuration error, `2` lock provider error,
`3` context parse error, `4` initialisation error, `6` event error,
`7` command error, `8` at least one job failed, `10` not in GitHub Actions.
Events the action does not handle (for example `push` or `workflow_run`
reaching the PR mode) are ignored with exit `0`.

## Contributing

Bug reports and feature requests are welcome as
[GitHub Issues](https://github.com/kangaechu/tf-nikama/issues/new). Pull
requests are accepted from repository collaborators only; PRs from other
accounts are closed automatically. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

tf-nikama is an independent re-implementation of the backendless mode of
[Digger](https://github.com/diggerhq/digger) by Digger HQ, licensed under
Apache-2.0. Configuration keys, comment commands, the DynamoDB and Cloud Storage lock
schemas and the plan object key format are kept compatible so existing setups
can migrate.

## License

Apache License 2.0. See [LICENSE](LICENSE).
