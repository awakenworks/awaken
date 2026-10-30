# Project validation

Build and test validation uses the available local environment.

- Record checks that could not run as unverified; do not count partial evidence
  or cancelled runs as a complete pass.
- Keep temporary E2E files within a task-owned root and preserve cleanup
  ownership boundaries.
- Normal Git fetch, rebase, and push to the configured origin remain allowed;
  keep the existing Git hooks enabled.
