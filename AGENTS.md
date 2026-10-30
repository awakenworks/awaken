# Project validation platform

Build and test validation runs on the local Linux host (user decision
2026-10-31, replacing the Windows/MSVC-only rule of 2026-10-22). Native
Windows/MSVC checks run where a Windows host is available and stay part of the
gates that require them.

- Record checks that could not run on the available platform as unverified; do
  not count partial evidence or cancelled runs as a complete pass.
- Use a short task-owned temp root for native E2E. Preserve the existing Windows
  Git staging-path and cleanup-ownership checks.
- Normal Git fetch, rebase, and push to the configured origin remain allowed;
  keep the existing Git hooks enabled.
