#!/usr/bin/env bash
# Cross-platform executable contract for the host-backed Agent filesystem view.
set -euo pipefail
cd "$(dirname "$0")/../.."
# Reuse the same checkout and ambient-secret boundaries as the main CI gate.
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys

# Cause/effect matrix: provider={Workdir,sandbox-absolute}, cwd={default,nested},
# separator={POSIX,Windows}, candidate cwd={equal,different}. Effects: one
# process/ACP/prompt view; platform-native relative prompt paths; mismatched
# fallback candidates reject before launch. The Local provider suite adds the
# native Sandbox lifecycle and filesystem boundary on the executing OS.
cargo test -p awaken-session-executor --lib resource_prompt_tests
cargo test -p awaken-session-executor --lib trusted_host_repository_projection_preserves_the_exact_tier
cargo test -p awaken-session-executor --lib trusted_host_live_resource_admission_keeps_readonly_advisory
cargo test -p awaken-session-executor --lib local_and_namespace_restart_replay_exact_memory_participants_before_late_mounts
cargo test -p awaken-session-executor --lib \
  trusted_host_filesystem_view_maps_workspace_relative_cwd_once
cargo test -p awaken-session-executor --lib \
  configured_working_directory_drives_process_and_acp_from_one_view
cargo test -p awaken-session-executor --lib \
  model_pool_working_directory_is_one_attempt_invariant
cargo test -p awaken-session-executor --lib \
  session_environment::tests::native_process_and_agent_channel_share_one_live_environment -- --exact
cargo test -p awaken-agent-config --lib \
  backend_owned_selection_cannot_encode_an_inexact_backend_or_model
cargo test -p awaken-run-executor-acp --lib
cargo test -p awaken-worker-contract --lib
cargo test -p awaken-cli --lib acp_local_credentials
cargo test -p awaken-sandbox-local --lib
cargo test -p awaken-local-process --lib
cargo test -p awaken-sandbox-fs --lib
cargo test -p awaken-sandbox-memoryd --lib
cargo test -p awaken-sandbox-memoryd --test copy_lifecycle
cargo test -p awaken-sandbox-memoryd --test writeback_faults

# Composition rules: each installed fixture is a real native executable, and
# each runtime must perform File/Memory/Repository I/O through the production
# API -> Worker -> cwd -> prompt path. No "runtime missing" success skip.
for runtime in codex claude; do
  CODEX_ACP_PROFILE=host-fixture AWAKEN_ACP_RUNTIME="$runtime" \
    node e2e/codex_acp_live_e2e.mjs
done

# Native Namespace is an explicit Linux substrate gate, not an ACP cwd option
# and not a successful skip on hosts lacking Namespace support. Other OS rows
# still run Native Local contracts above; no cross-OS Namespace claim is made.
if [[ "$(uname -s)" == "Linux" ]]; then
  CODEX_ACP_PROFILE=native-fixture AWAKEN_ACP_RUNTIME=native \
    node e2e/codex_acp_live_e2e.mjs
fi
