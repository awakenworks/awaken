#!/usr/bin/env bash
# Cross-platform executable contract for the host-backed Agent filesystem view.
set -euo pipefail
cd "$(dirname "$0")/../.."
# Reuse the same checkout and ambient-secret boundaries as the main CI gate.
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
source scripts/ci/_rust_test_execution.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys
awaken_append_repo_tools

# Gate selection table: default preserves the cross-platform contract;
# --linux-supervision additionally REQUIRES real Linux PID namespaces/pidfd.
# Unknown/multiple arguments or a non-Linux supervision request fail before
# any tests. This is substrate qualification, not a production capability bit.
linux_supervision=false
if [[ "$#" == 1 && "$1" == --linux-supervision ]]; then
  [[ "$(uname -s)" == Linux ]] || { echo "Linux supervision requires Linux" >&2; exit 2; }
  linux_supervision=true
elif [[ "$#" != 0 ]]; then
  echo "usage: $0 [--linux-supervision]" >&2
  exit 2
fi
if [[ "$linux_supervision" == true ]]; then
  cargo test -p awaken-sandbox --test linux_scoped
fi
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    AWAKEN_NATIVE_BASH="$(cygpath -w "$BASH")" python3 scripts/ci/test_deadline.py WindowsDeadlineTests
    # Real original-Job ownership, active-wait control and cancellation. Named
    # floors refuse disappearing/ignored cases; helpers run only via parents.
    run_required_rust_tests --locked -p awaken-process-win32 --lib \
      --require-case=sys::tests::invalid_object_query_preserves_the_native_error_instead_of_empty_job \
      --require-case=sys::tests::poisoned_member_observation_refuses_a_successful_stop \
      --require-case=sys::members::tests::active_members_and_changed_membership_cannot_be_confirmed_as_quiescent
    run_required_rust_tests --locked -p awaken-ext-builtin-tools --lib \
      --require-case=hand::bash::windows_process::tests::cloned_control_terminates_the_original_tree_while_wait_owns_the_child \
      --require-case=hand::bash::windows_process::tests::cancelled_wait_retains_shutdown_control_and_joins_original_quiescence \
      --require-case=hand::bash::windows_process::tests::natural_leader_exit_returns_only_after_its_retained_leaf_has_exited \
      --require-case=hand::bash::windows_process::tests::owner_drop_terminates_its_tree_even_when_a_control_clone_survives \
      --require-case=hand::bash::windows_process::tests::cancelled_session_discard_retains_the_cleanup_owner_until_confirmation \
      -- hand::bash::windows_process::tests:: --skip hand::bash::windows_process::tests::process_tree_child
    run_required_rust_tests --locked -p awaken-run-executor-acp --test windows_descendant \
      --require-case=t10_c02_leader_exit_keeps_identity_and_does_not_hide_a_live_descendant \
      --require-case=t10_c02_reap_after_leader_exit_does_not_claim_stopped_while_descendant_lives \
      -- --skip fixture_descendant_echoes_until_eof --skip fixture_leader_spawns_descendant_then_exits
    ;;
esac

# SQLite crash/replay contracts are native process/filesystem obligations too.
# Every OS matrix row must run them; the PostgreSQL lane independently runs the
# identical domain assertions against its mandatory live network database.
cargo test --locked -p awaken-session-store --test process_crash_outbox \
  -p awaken-config-store --test process_crash_audit \
  -p awaken-credential-store --features sqlite,sealed-aead,test-support \
  --test process_crash_creation --test process_crash_managed_creation -- --test-threads=1

# Cause/effect matrix: provider={Workdir,sandbox-absolute}, cwd={default,nested},
# separator={POSIX,Windows}, candidate cwd={equal,different}. Effects: one
# process/ACP/prompt view; platform-native relative prompt paths; mismatched
# fallback candidates reject before launch. The Local provider suite adds the
# native Sandbox lifecycle and filesystem boundary on the executing OS.
run_required_rust_tests -p awaken-session-realization --lib resource_prompt_tests \
  --require-case=managed_input_layout::resource_prompt::resource_prompt_tests::resource_prompt_variable_decision_table_is_closed_and_provider_relative \
  --require-case=managed_input_layout::resource_prompt::resource_prompt_tests::resource_prompt_templates_preserve_literals_and_never_expand_resource_data
run_required_rust_tests -p awaken-session-executor --lib trusted_host_repository_projection_preserves_the_exact_tier
run_required_rust_tests -p awaken-session-environment --lib trusted_host_live_resource_admission_keeps_readonly_advisory
# Memory replay decision tables cover absent/exact/different installed pins,
# lost acknowledgements and concurrent callers as well as Provider restart.
# Select this existing group once: prompt and terminal rows have their own
# owners immediately above/below and must not run a second time here.
memory_selection=(--skip resource_prompt_tests --skip terminal_)
if [[ "$(uname -s)" != Linux ]]; then
  # This Namespace replay row belongs to the required Linux gate. Do not count
  # its non-Linux ignored result as portable execution evidence.
  memory_selection+=(--skip exact_live_memory_manifest_replay_is_idempotent_but_change_fails_closed)
fi
run_required_rust_tests -p awaken-session-executor --lib memory -- "${memory_selection[@]}"
# Terminal cause/effect tables include hot/cold Memory, cancelled recovery,
# same-generation renewal and Control/Worker clock disagreement. Select the
# existing terminal group once; it includes the former single Memory row and
# the provider-fence regression, without a parallel list of disposal cases.
# These two publication rows execute a real Namespace shell for every assertion.
# Keep them out of portable/default runs, even on Linux without its substrate.
namespace_publication=host::tests::resource_activation::
namespace_publication_ready=${namespace_publication}managed_terminal_repository_publication_pushes_exact_commit_and_replays
namespace_publication_closed=${namespace_publication}terminal_repository_publication_closed_source_converges_without_git
run_required_rust_tests -p awaken-session-executor --lib terminal_ -- \
  --skip "$namespace_publication_ready" --skip "$namespace_publication_closed"
if [[ "$linux_supervision" == true ]]; then
  # These provider process rows require Linux Namespace effects. Their former
  # early returns appeared green on Windows and on Linux without bwrap.
  # Require their exact executions here; absence is a failure, never a skip.
  namespace_workspace=namespace::tests::opaque_processes_and_runtime_projections_share_the_workspace_root
  namespace_spawn=namespace::tests::spawn_agent_launches_an_opaque_process_confined_by_bwrap
  run_required_rust_tests -p awaken-sandbox-local --lib namespace::tests:: \
    --require-case="$namespace_workspace" --require-case="$namespace_spawn" -- --ignored
  # Both case identities are required: an ignored/missing row cannot qualify
  # publication, and an unavailable Namespace must fail inside the fixture.
  run_required_rust_tests -p awaken-session-executor --lib terminal_repository_publication \
    --require-case="$namespace_publication_ready" --require-case="$namespace_publication_closed" -- --ignored
fi
# Current-proof projection and Host adoption waits are platform-neutral. Select
# their existing test module once on every OS; the terminal filter above does
# not include these source/ABA rows, and provider unit tests cannot prove wiring.
run_required_rust_tests -p awaken-session-executor --lib fence_source_tests
run_required_rust_tests -p awaken-session-environment --lib \
  trusted_host_filesystem_view_maps_workspace_relative_cwd_once
run_required_rust_tests -p awaken-session-environment --lib trusted_host_installed_hand
run_required_rust_tests -p awaken-session-environment --lib workdir_unknown_hand_reap
# The launch adapter moved to execution-composition; the former Host filter
# selected zero tests. Bind this obligation to its current exact test identity.
launch_selection=sandbox_source::tests::configured_working_directory_drives_process_and_acp_from_one_view
run_required_rust_tests -p awaken-execution-composition --lib \
  "$launch_selection" --require-case="$launch_selection" -- --exact
run_required_rust_tests -p awaken-session-executor --lib \
  model_pool_working_directory_is_one_attempt_invariant
shared_environment=realization_composed_tests::session_environment::tests::shared_environment::native_process_and_agent_channel_share_one_live_environment
run_required_rust_tests -p awaken-session-executor --lib \
  "$shared_environment" --require-case="$shared_environment" -- --exact
run_required_rust_tests -p awaken-agent-config --lib \
  backend_owned_selection_cannot_encode_an_inexact_backend_or_model
cargo test -p awaken-run-executor-acp --lib
cargo test -p awaken-acp-application --lib
run_required_rust_tests -p awaken-worker --lib bootstrap::
cargo test -p awaken-worker-contract --lib
run_required_rust_tests -p awaken-cli --lib acp_local_credentials
# Publication lifecycle uses the same portable task/slot kernel. Include both
# adapter races on every matrix runner; Namespace's Unix-socket rows remain
# Linux-gated inside the Local suite. No container daemon is needed here.
cargo test -p awaken-sandbox-control --lib
cargo test -p awaken-sandbox-control-transport --lib
# Include the neutral provider/Memory preparation decision tables and actual
# credential-response wait faults, not only control publication. This expands
# the existing provider invocation; it does not run the control group twice.
cargo test -p awaken-provisioning-contract --lib
cargo test -p awaken-sandbox-container --lib
cargo test -p awaken-sandbox-local --lib
cargo test -p awaken-session-environment --lib
# The authority clock's Linux BOOTTIME and non-Linux Instant paths both need
# every matrix runner; the local-process scoped cutoff reads the same Linux clock.
cargo test -p awaken-authority-clock --lib
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    run_required_rust_tests --locked -p awaken-local-process --lib \
      --require-case=windows_job::tests::job_cancellation_and_drop_close_owned_descendants \
      --require-case=windows_job::tests::natural_leader_exit_requests_owned_descendant_teardown \
      --require-case=windows_job::tests::kernel_job_closes_when_owner_is_terminated_without_drop \
      -- --skip tests::lifecycle_child --skip windows_job::tests::tree_child
    ;;
  *) cargo test -p awaken-local-process --lib ;;
esac
# Run the actual Hand binary with an explicit resource root and a nested cwd,
# not just a renderer/unit fixture. Non-Linux builds exercise native path parsing.
cargo test -p awaken-sandbox --features hand --lib --test hand_stdio_role
# Artifact rule: Cargo may separate intermediates from final executables (and
# Windows adds .exe). Reuse the one JSON resolver, never infer a profile sibling
# of the test binary or copy a stale companion into Cargo's build cache.
AWAKEN_TEST_SCOPED_HELPER="$(node --input-type=module -e '
  import { cargoExecutable } from "./e2e/cargo_binary.mjs";
  process.stdout.write(cargoExecutable({ cwd: process.cwd(), packageName: "awaken-sandbox", targetName: "awaken-sandbox", features: ["hand"] }));
')"
export AWAKEN_TEST_SCOPED_HELPER
if [[ "$linux_supervision" == true ]]; then
  # The Session lifecycle rows use real scope processes and the exact just-built
  # helper. Missing binaries/substrate fail; ignored merely isolates this
  # mandatory Linux gate from portable unit builds without an installed helper.
  AWAKEN_TEST_SCOPED_LAUNCHER="$(command -v bwrap)" \
    run_required_rust_tests -p awaken-session-executor --lib trusted_host_scoped_session -- --ignored
  AWAKEN_TEST_SCOPED_LAUNCHER="$(command -v bwrap)" \
    run_required_rust_tests -p awaken-session-environment --lib trusted_host_scoped_session -- --ignored
fi
# These startup rows require the just-built release companion. The thin scoped
# qualification above intentionally does not provide Hand, and must not race
# companion validation or turn an absent executable into a successful skip.
run_required_rust_tests -p awaken-session-executor --lib acp_backend::tests -- --include-ignored
run_required_rust_tests -p awaken-execution-composition --lib local:: -- --include-ignored
cargo test -p awaken-fs-durability --features native-fs --lib
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    run_required_rust_tests -p awaken-fs-durability --features native-fs --test windows_long_paths \
      --require-case=windows_long_sibling_publication_keeps_exact_identity_and_noreplace
    ;;
esac
cargo test -p awaken-sandbox-memoryd --lib
cargo test -p awaken-sandbox-memoryd --test copy_lifecycle
cargo test -p awaken-sandbox-memoryd --test writeback_faults
cargo test -p awaken-memory-store --features sqlite,test-support
cargo test -p awaken-resource-worker-http --lib
cargo test -p awaken-resource-worker-http --test memory_transport
cargo test -p awaken-protocol-managed --test memory_store_api_http

# Composition rules: each installed fixture is a real native executable, and
# each runtime must perform File/Memory/Repository I/O through the production
# API -> Worker -> cwd -> prompt path. No "runtime missing" success skip.
for runtime in codex claude; do
  # Both topologies share one mission. The split row additionally requires
  # signed transport, exact-reference revocation and ONLY Worker replacement.
  for profile in host-fixture worker-host-fixture; do
    CODEX_ACP_PROFILE="$profile" AWAKEN_ACP_RUNTIME="$runtime" \
      node e2e/codex_acp_live_e2e.mjs
  done
done
CODEX_ACP_PROFILE=native-host-fixture AWAKEN_ACP_RUNTIME=native \
  node e2e/codex_acp_live_e2e.mjs

# Native Namespace is an explicit Linux substrate gate, not an ACP cwd option
# and not a successful skip on hosts lacking Namespace support. Other OS rows
# still run Native Local contracts above; no cross-OS Namespace claim is made.
if [[ "$(uname -s)" == "Linux" ]]; then
  # Both kernel owner-death and real Hand path/tool behavior are mandatory;
  # selecting only the crash test would omit the Native resource path contract.
  cargo test -p awaken-sandbox --features hand --test namespace_hand_stdio
  CODEX_ACP_PROFILE=native-fixture AWAKEN_ACP_RUNTIME=native \
    node e2e/codex_acp_live_e2e.mjs
elif [[ "$(uname -s)" == "Darwin" ]]; then
  # The real Seatbelt row is required on macOS, not an unavailable-success skip.
  cargo test -p awaken-sandbox --features hand --test seatbelt_hand_stdio
fi
