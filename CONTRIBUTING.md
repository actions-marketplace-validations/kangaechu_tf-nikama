# Contributing to tf-nikama

Thank you for your interest in tf-nikama.

## Issues: welcome from everyone

Bug reports, feature requests, questions and compatibility notes are all
welcome as [GitHub Issues](https://github.com/kangaechu/tf-nikama/issues/new).
A good report includes:

- the tf-nikama version (`nikama --version`) and the Terraform / OpenTofu version
- the relevant part of `nikama.yml` and the workflow file, with secrets removed
- what you expected and what happened, including the workflow log around the error

Please note the project scope before filing a feature request: tf-nikama
targets Terraform / OpenTofu on AWS only, and there are currently no plans to
support other IaC tools (Terragrunt, Pulumi, ...) or cloud providers (GCP,
Azure, ...). Requests for those will be closed as out of scope.

## Pull requests: maintainers only

tf-nikama is maintained by a small team and we do **not** accept pull requests
from outside that team at this time. Pull requests opened by non-collaborators
are closed automatically by a workflow with a link back to the issue tracker.

If you would like to see a change, please open an issue describing it. When a
maintainer agrees the change belongs in tf-nikama, they will implement it and
credit the reporter in the commit message or release notes.

This policy keeps the review and release process manageable; it may be
relaxed in the future.
