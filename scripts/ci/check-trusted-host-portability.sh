#!/usr/bin/env bash
# Cross-platform executable contract for the host-backed Agent filesystem view.
set -euo pipefail
cd "$(dirname "$0")/../.."

# Cause/effect matrix: provider={Workdir,sandbox-absolute}, cwd={default,nested},
# separator={POSIX,Windows}, candidate cwd={equal,different}. Effects: one
# process/ACP/prompt view; platform-native relative prompt paths; mismatched
# fallback candidates reject before launch. The Local provider suite adds the
# native Sandbox lifecycle and filesystem boundary on the executing OS.
cargo test -p awaken-session-executor --lib \
  workdir_prompt_paths_follow_the_effective_nested_cwd_cross_platform
cargo test -p awaken-session-executor --lib \
  trusted_host_filesystem_view_maps_workspace_relative_cwd_once
cargo test -p awaken-session-executor --lib \
  configured_working_directory_drives_process_and_acp_from_one_view
cargo test -p awaken-session-executor --lib \
  model_pool_working_directory_is_one_attempt_invariant
cargo test -p awaken-agent-config --lib \
  backend_owned_selection_cannot_encode_an_inexact_backend_or_model
cargo test -p awaken-sandbox-local --lib
