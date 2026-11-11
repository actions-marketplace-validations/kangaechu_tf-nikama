# tf-nikama

<p align="center">
  <img src="docs/images/tf-nikama.png" alt="tf-nikama logo: a steaming stainless pot of boiling tofu" width="400">
</p>

[日本語版 README](README.ja.md)

Backendless Terraform / OpenTofu automation for GitHub pull requests, as a single
small binary that runs inside GitHub Actions.

`nikama` is a Rust re-implementation of the *backendless mode* of
[Digger](https://github.com/diggerhq/digger) (`no-backend: true`).
Configuration keys, comment commands, the lock schemas and the plan storage
format are compatible with Digger, so it can be used as a drop-in replacement
for an existing Digger setup.

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
    # the commenter's write access itself (see docs/CONFIGURATION.md).
    if: >-
      github.event_name != 'issue_comment'
      || contains(fromJSON('["OWNER", "MEMBER", "COLLABORATOR"]'), github.event.comment.author_association)
    steps:
      - uses: actions/checkout@v7
        with:
          ref: ${{ github.event_name == 'issue_comment' && format('refs/pull/{0}/merge', github.event.issue.number) || '' }}
      - uses: hashicorp/setup-terraform@v4
        with:
          terraform_wrapper: false
      - uses: aws-actions/configure-aws-credentials@v6
        with:
          role-to-assume: arn:aws:iam::123456789012:role/nikama
          aws-region: ap-northeast-1
      - uses: kangaechu/tf-nikama@v0.3.0
        with:
          upload-plan-destination: aws
          upload-plan-destination-s3-bucket: my-plan-bucket
```

PR locks default to a DynamoDB table (`DiggerDynamoDBLockTable`, created on
first use). To keep everything in S3 instead, set `lock-provider: s3` and
`aws-lock-bucket`; the role then needs `s3:PutObject`, `s3:GetObject`,
`s3:DeleteObject` and `s3:ListBucket` on that bucket and no DynamoDB
permissions. Lock objects are created with `If-None-Match: *`, so two runs
cannot both take the same project. The lock bucket can be the plan bucket.

```yaml
      - uses: kangaechu/tf-nikama@v0.3.0
        with:
          lock-provider: s3
          aws-lock-bucket: my-lock-bucket
          upload-plan-destination: aws
          upload-plan-destination-s3-bucket: my-plan-bucket
```

On Google Cloud, replace the AWS credentials step with
`google-github-actions/auth` and point the action at Cloud Storage buckets
(the service account needs `roles/storage.objectUser` on both):

```yaml
      - uses: google-github-actions/auth@v3
        with:
          workload_identity_provider: projects/123456789012/locations/global/workloadIdentityPools/github/providers/github
          service_account: nikama@my-project.iam.gserviceaccount.com
      - uses: kangaechu/tf-nikama@v0.3.0
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

[`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) describes how a run works
(locks, apply requirements, auto-merge), who can start a run and what it may
execute; [`docs/PARAMETERS.md`](docs/PARAMETERS.md) lists every action input,
`nikama.yml` key and environment variable; [`examples/nikama.yml`](examples/nikama.yml)
shows the full schema.

Coming from Digger? See [`docs/MIGRATION.md`](docs/MIGRATION.md).

## What nikama adds

Everything Digger's backendless mode does, plus:

- **Faster feedback on large repositories**: projects plan in parallel instead of one after another.
- **Readable plan / apply results**: one comment per run, updated in place as projects finish.
- **Fewer cloud resources**: on AWS, locks can also live in an S3 bucket, so no DynamoDB table is needed.
- **Planning never blocks a colleague**: with `pr_locks: on_apply`, several PRs can plan the same project and only the apply takes the lock.
- **Dependency bumps block nobody**: Renovate / Dependabot PRs plan every directory without holding a single lock.
- **Targeted plans from a comment**: pass `-target` / `-replace` with `nikama plan -- ...`, within an allowlist you define, without editing the workflow.
- **Module changes are not missed**: a change to a local module plans every project that uses it.
- **One directory, several environments**: a project's `branch` regex ties staging and production workspaces to their base branches.
- **Drafts stay quiet**: a draft PR runs nothing and holds no locks until it is marked ready for review.
- **Never apply a stale plan**: a stored plan is tied to the commit it came from and refused for any other.
- **Outsiders cannot run your Terraform**: comment commands and approvals only count from users with write access, and `config-ref` / `allowed-providers` stop a PR from changing what runs with the job's credentials.

Not supported, and rejected at config load: Terragrunt, Pulumi, Azure lock or
plan storage, GitHub Artifacts plan storage, OPA policies and Digger backend
features. There are no plans to go beyond Terraform / OpenTofu on AWS and
Google Cloud.

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
