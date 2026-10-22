# Project validation platform

The user requires all build and test validation executed or dispatched by agents
to use local Windows with the MSVC toolchain (instruction dated 2026-10-22).

- Do not run validation on Linux, remote Linux hosts, WSL, or Linux containers.
- Record checks that cannot run on Windows as unverified; do not count partial
  evidence or cancelled runs as a complete pass.
- Use a short task-owned temp root for native E2E. Preserve the existing Windows
  Git staging-path and cleanup-ownership checks.
- Normal Git fetch, rebase, and push to the configured origin remain allowed;
  keep the existing Git hooks enabled.
