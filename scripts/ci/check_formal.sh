#!/usr/bin/env bash
set -euo pipefail

readonly tlc_version="TLC2 Version 2.19 of 08 August 2024 (rev: 5a47802)"
readonly tlapm_version="763bf3c"
readonly z3_version="Z3 version 4.8.9 - 64 bit"

cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"
source scripts/ci/_deadline.sh
# TLAPS bundles the pinned Z3 outside a non-login PATH. Without this the Z3
# profiles below ran only when the caller's shell happened to carry it.
source scripts/ci/_provider_environment.sh
awaken_append_repo_tools

# Decide this stage's environment before it does anything long, so a missing
# tool is reported as a missing tool. Without this the same facts arrive hours
# later wearing the wrong name: an absent solver as "0 of N failed ...
# VERIFICATION FAILED", and a full filesystem as a CBMC SIGABRT on a GOTO binary
# that a short write truncated. scripts/ci/gate_tools.json declares both, and
# owns the pinned versions this file also states as shell constants above.
# A misspelling here is the worst silent fall-through in this repository:
# `--require-tool` left require_tools=0, so hours of proofs ran in permissive
# mode and the release gate reported a green formal group it had never demanded
# tools for. Admit exactly one spelling; refuse everything else before any
# receipt is written.
require_tools=0
case "${1:-}" in
  "") ;;
  --require-tools) require_tools=1 ;;
  *) echo "usage: scripts/ci/check_formal.sh [--require-tools]" >&2; exit 2 ;;
esac
if [ "$#" -gt 1 ]; then
  echo "usage: scripts/ci/check_formal.sh [--require-tools]" >&2
  echo "       (unexpected extra arguments: ${*:2})" >&2
  exit 2
fi

# One invocation replaces any previous receipt with Running before executing.
# Only successful artifact finalization may publish Passed. The quality workflow
# retains this file beside its existing exact-SHA timing evidence.
formal_run_record="${AWAKEN_FORMAL_RUN_EVIDENCE:-target/test-timings/formal-run.json}"
python3 scripts/ci/formal_run_evidence.py begin \
  --record "$formal_run_record" --strict "$require_tools"

missing=0
formal_tmp_root="$(mktemp -d)"
formal_artifact_dir="$(python3 scripts/ci/formal_run_evidence.py artifacts-begin --kind formal)"
formal_failure_kind=""
formal_failure_stage=""
formal_failure_subject=""
formal_failure_deadline_seconds=""
kani_batch_timeout_seconds="${AWAKEN_KANI_BATCH_TIMEOUT_SECONDS:-1800}"
finish_formal_artifacts() {
  first_result=$?
  result=$first_result
  artifact_result=0
  trap - EXIT
  python3 scripts/ci/formal_run_evidence.py artifacts-finish \
    --directory "$formal_artifact_dir" --exit-code "$first_result" || artifact_result=$?
  # A successful body cannot publish Passed when its diagnostics did not seal.
  # When the body already failed, its first cause remains authoritative.
  if [ "$first_result" -eq 0 ] && [ "$artifact_result" -ne 0 ]; then
    result=$artifact_result
    formal_failure_kind="execution-failed"
    formal_failure_stage="artifact-finalization"
    formal_failure_subject="formal"
    formal_failure_deadline_seconds=""
  fi
  if [ "$result" -ne 0 ]; then
    finish_args=(
      finish --record "$formal_run_record" --missing-tools "$missing"
      --exit-code "$result"
      --failure-kind "${formal_failure_kind:-execution-failed}"
      --failure-stage "${formal_failure_stage:-formal-gate}"
    )
    if [ -n "$formal_failure_subject" ]; then
      finish_args+=(--failure-subject "$formal_failure_subject")
    fi
    if [ -n "$formal_failure_deadline_seconds" ]; then
      finish_args+=(--deadline-seconds "$formal_failure_deadline_seconds")
    fi
    # Not-passed is diagnostic and can never qualify a release. A receipt-write
    # failure must not hide or replace the first gate failure.
    python3 scripts/ci/formal_run_evidence.py "${finish_args[@]}" || \
      echo "failed to finalize non-passing formal receipt" >&2
  elif [ "$result" -eq 0 ] && [ "$require_tools" -eq 1 ]; then
    python3 scripts/ci/formal_run_evidence.py finish \
      --record "$formal_run_record" --missing-tools "$missing" || result=$?
  fi
  if ! rm -rf "$formal_tmp_root"; then
    echo "failed to clean formal temporary directory: $formal_tmp_root" >&2
  fi
  exit "$result"
}
trap finish_formal_artifacts EXIT
# Parse the one package-batch budget after the Running receipt and EXIT owner
# exist, but before any verification command. Invalid configuration therefore
# cannot leave stale green evidence or consume compilation/proof resources.
if ! [[ "$kani_batch_timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
  formal_failure_kind="execution-failed"
  formal_failure_stage="cargo-kani-configuration"
  formal_failure_subject="AWAKEN_KANI_BATCH_TIMEOUT_SECONDS"
  echo "invalid cargo-kani batch deadline: $kani_batch_timeout_seconds" >&2
  exit 2
fi
# The bootstrap is the sole owner of compiler/backend defaults and pins.
# Freeze both physical selections before another bootstrap moves current.
kani_selection="$(scripts/ci/bootstrap_kani.sh --paths)"
mapfile -t kani_paths <<< "$kani_selection"
kani_source_dir="${kani_paths[0]}"
kani_backend_dir="${kani_paths[1]}"
export AWAKEN_KANI_BACKEND_DIR="$kani_backend_dir"
if [ -x "$kani_source_dir/scripts/cargo-kani" ]; then
  export PATH="$kani_source_dir/scripts:$kani_backend_dir:$PATH"
fi
# The driver execs external solvers (kissat) by name; they ship in the released
# bundle, which a non-login PATH does not carry. Appended, never prepended.
kani_solver_tools="$(scripts/ci/bootstrap_kani.sh --solver-tools)"
if [ -d "$kani_solver_tools" ] && [[ ":$PATH:" != *":$kani_solver_tools:"* ]]; then
  export PATH="$PATH:$kani_solver_tools"
fi

# Declared-environment preflight. It runs here, not at the top of the file:
# argument validation above must fail without side effects, and tool selection
# must stay the first physical step inside the receipt/EXIT lifecycle, so a
# preflight failure retires a stale Passed like any other gate failure.
python3 scripts/ci/gate_tools.py require --gate formal
python3 scripts/ci/gate_tools.py disk --stage formal

rust_trace_dir="$formal_artifact_dir/rust-traces"
rendered_trace_dir="$formal_artifact_dir/rendered-traces"
observation_trace_dir="$formal_artifact_dir/session-observations/raw"
observation_rendered_dir="$formal_artifact_dir/session-observations/rendered"

python3 scripts/ci/check_feature_coverage.py --require-complete
python3 scripts/ci/check_formal_coverage.py --require-complete \
  --require-guardrail G42 \
  --require-guardrail G43
python3 scripts/ci/formal_ledger.py --self-test
python3 scripts/ci/formal_plan.py --self-test
python3 scripts/ci/test_formal_profile.py
python3 scripts/ci/test_formal_run_evidence.py
python3 scripts/ci/test_bootstrap_kani.py
python3 scripts/release/verification_manifest.py self-test
python3 scripts/ci/test_formal_surface.py
python3 scripts/ci/check_formal_surface.py --require-complete
python3 scripts/ci/check_formal_web_surface.py
python3 scripts/ci/check_proof_boundaries.py
python3 scripts/ci/check_formal_mutations.py

# Bind the abstract one-owner/waiting-follower theorem to the production
# TaskRegistry phase across timeout, cooperative, and caller-cancellation rows.
# The successful row requires two independent followers; the cancellation row
# proves dropping the initiating Future cannot drop the unique drain owner.
cargo test -p awaken-service-lifecycle --test service_lifecycle \
  concurrent_shutdown_cannot_return_before_the_active_drain_finishes -- --exact
cargo test -p awaken-service-lifecycle --test service_lifecycle \
  concurrent_shutdown_followers_wait_for_successful_active_drain -- --exact
cargo test -p awaken-service-lifecycle --test service_lifecycle \
  cancelled_shutdown_caller_cannot_orphan_the_active_drain -- --exact

# The abstract network algebra is not Rust String/Vec refinement. Exercise the
# actual owner and frozen SQLite root alongside its TLAPS/TLC checks below.
cargo test -p awaken-protocol-managed --lib projection_publish_decision_covers_cache_and_source_fences
cargo test -p awaken-session-configuration-contract --lib network_policy::tests
cargo test -p awaken-session-application --lib profiled_session_creation_enforces_publication_and_upfront_inputs
cargo test -p awaken-session-application --lib realization::publication::collection_tests
# The executable-refresh theorem abstracts async trait dispatch and application
# side effects. Inject failure at independent production consumers and require
# every downstream handler, mutation and launch counter to remain untouched.
cargo test -p awaken-coordinator --lib failed_refresh_never_enters_shared_inventory_or_warmup_handlers
cargo test -p awaken-session-application --lib admission_surfaces_dispatch_failure_and_preserves_retryable_intent
cargo test -p awaken-deployment-application --lib scheduler_refresh_failure_precedes_candidate_mutation_and_launch
cargo test -p awaken-session-application-contract --lib \
  executable_projection::tests::paired_refresh_capability_covers_every_domain_outcome
# ManagementAuditIntent abstracts the repository and Axum middleware boundary.
# Require the canonical protected router to attempt durable intent persistence
# and leave its observable business handler untouched on repository failure.
cargo test -p awaken-control --lib \
  durable_audit_failure_blocks_business_through_canonical_management_router
# Bind successful completion and its failure window to the same public router,
# canonical repository port, and real SQLite durable audit owner.
cargo test -p awaken-control --lib management_audit_completion_
cargo test -p awaken-durable-projection --lib install_reconciliation_covers_cursor_and_authority_outcomes
cargo test -p awaken-durable-projection --lib refresh_domain_permit_is_exclusive
cargo test -p awaken-executable-agent-catalog --lib active_active_peer_refreshes_the_durable_command_projection
cargo test -p awaken-executable-environment-catalog --lib active_active_peer_refreshes_the_durable_environment_projection
cargo test -p awaken-executable-agent-catalog --lib install_error_precedes_agent_target_publication
cargo test -p awaken-executable-environment-catalog --lib install_error_precedes_environment_target_publication
# Bind the theorem's per-domain lock owner to both production registrar types.
# Each test blocks the first source read after two refresh tasks have attempted
# the same registrar, then requires one entered read and one exact installation.
cargo test -p awaken-executable-agent-catalog --lib \
  concurrent_agent_refreshes_are_serialized_by_the_domain_registrar
cargo test -p awaken-executable-environment-catalog --lib \
  concurrent_environment_refreshes_are_serialized_by_the_domain_registrar
# Cross-check the theorem's installed <= authority invariant at the stable-range
# boundary in both production registrar types. A command committed above the
# captured high-water must wait for the next refresh rather than enter this one.
cargo test -p awaken-executable-agent-catalog --lib \
  agent_command_above_captured_high_water_waits_for_next_refresh
cargo test -p awaken-executable-environment-catalog --lib \
  environment_command_above_captured_high_water_waits_for_next_refresh
# Cross the application reaction with the canonical SQLite transaction owner.
# C1 two real schedulers calculate one occurrence: the losing stale cache removes
# its speculative run, refreshes and never launches. C2 two independent
# application clients concurrently create at capacity one: exactly one winner is
# published and the loser receives the capacity error. The shared three-client
# store table proves the rejected capacity transactions leave no row/outbox fact,
# then reuses a rejected stale claim id from a current replica. Configured
# PostgreSQL executes that same store table in the required database gate.
cargo test -p awaken-deployment-application --lib durable_replicas_claim_one_exact_occurrence
cargo test -p awaken-deployment-application --lib \
  scheduled_capacity_is_linearizable_across_replicas
cargo test -p awaken-session-store --features test-support \
  --test session_repo_conformance sqlite_backend_conforms -- --exact
# The WebhookOutbox model treats a committed fact as recoverable across crash.
# Bind that premise to a real post-commit process kill and fresh SQLite reopen;
# delivery/settlement remains covered by the separate notifier kernel below.
cargo test -p awaken-session-store --test process_crash_outbox \
  session_commit_survives_process_kill_before_notification
# The atomic-commit claim also requires the opposite terminal outcome: a real
# outbox INSERT failure must roll back create, replace and delete root writes,
# their receipts, and the outbox itself through the production SQLite adapter.
# Live PostgreSQL executes the same table in the required database gate.
cargo test -p awaken-session-store --features test-support \
  --test session_repo_conformance \
  sqlite_lifecycle_outbox_insert_failure_rolls_back_every_root_path -- --exact

# Explore the production in-memory worker registry's lock interleavings. This
# feature swaps only its mutex for Loom's instrumented mutex; the transition
# kernel and directory methods remain the production code.
cargo test -p awaken-worker-registry --features loom --lib loom_tests
# Explore complete reference WorkQueue commands and its existing LeaseBook.
# Work rows, token digests and lease snapshots use the actual implementation;
# only their mutexes are substituted for the bounded Loom schedules.
cargo test -p awaken-work-store --features loom --lib loom_tests
# The actual ACP batch constructor, admission and publication share the same
# native/Loom case table. Only its mutex is instrumented; a later accepted
# same-revision status update may replace an earlier accepted value.
cargo test -p awaken-run-executor-acp --features loom --lib binding_batch_tests
cargo test -p awaken-acp-application --lib binding_batch_
# Exercise the real A2A executor across both commit/send crash-window tables.
cargo test -p awaken-run-executor-a2a --lib send_windows
# Cross the delegation model's stable identity and durable-cancellation actions
# through the one production registry, restart recovery, and the A2A adapter's
# lost-response retry against the same committed remote task.
cargo test -p awaken-agent-contract --lib \
  agent::delegation::tests::relationship_identity_and_cancel_redelivery_share_exact_durable_binding -- --exact
cargo test -p awaken-agent-contract --lib \
  agent::delegation::tests::child_invocation_admission_consumes_the_exact_open_relationship -- --exact
cargo test -p awaken-runtime --test formal_refinement \
  closed_delegation_relationship_never_reenters_the_child_executor -- --exact
cargo test -p awaken-runtime --test delegation \
  cancelling_an_awaiting_parent_atomically_persists_child_cancel_intent -- --exact
cargo test -p awaken-run-executor-a2a --lib \
  tests::failed_remote_cancel_is_retryable_against_the_same_committed_task -- --exact
# Exercise the production BackgroundTask projection under callback/replacement
# races. Only its single mutex is instrumented; durable commits remain outside
# this bounded lock-interleaving claim.
cargo test -p awaken-ext-background-task --features loom --lib supervisor::loom_tests
# The Kani predicate below is not sufficient evidence of the async wiring.
# Exercise its aggregate projection and real post-concurrency-admission check.
cargo test -p awaken-ext-background-task --test state_machine execution_admission
cargo test -p awaken-runtime --test background_task_state \
  terminal_reclaim_rejects_then_recovers_from_committed_thread_truth -- --exact
cargo test -p awaken-session-executor --lib background_task::ownership::tests
# The model's CommittedAuthority observation source is an async query policy,
# not the process projection. Hold that projection stale while the production
# terminal observer consumes an exact scripted authoritative state command.
cargo test -p awaken-session-executor --lib \
  committed_claim_launches_once_and_records_completion_without_a_second_store
# Async return/cleanup is not covered by the scalar physical-slot proof.
cargo test -p awaken-dispatch --features test-support --test pool_memory
cargo test -p awaken-dispatch --features test-support --test lease_semantics
# Settlement fencing and pending input have one shared cross-adapter table:
# stale Awaiting/Done settlements preserve dispatch, pending input, completion
# and wake eligibility, while current settlements publish every exact effect.
# Run the Memory and SQLite bindings here; the live PostgreSQL gate owns the
# same third binding, while check-reliability retains its normalized history.
cargo test -p awaken-dispatch --features test-support --test dispatch_conformance \
  memory_dispatch_conforms -- --exact
cargo test -p awaken-dispatch --features test-support --test dispatch_conformance \
  sqlite_dispatch_conforms -- --exact
# Cross the abstract crash action with a real child-process kill and two fresh
# opens of the production file-backed SQLite adapter.
cargo test -p awaken-dispatch --features test-support --test crash_before_settle \
  sqlite_process_crash_before_settle_replays_and_consumes_exact_input_once -- --exact
cargo test -p awaken-session-executor --lib durable_interrupt_returns_after_intent
# The epoch kernel assumes the failure CAS actually expires every recoverable
# assignment; cover Idle hot replacement as well as initial retry at that owner.
cargo test -p awaken-session-application --lib failed_nonterminal_realization
cargo test -p awaken-run-executor-acp --lib terminal_business_outcome
cargo test -p awaken-protocol-acp --features real-acp --lib
cargo test -p awaken-sandbox-container --features k8s --lib remote_completion_requires_explicit_process_exit_evidence
cargo test -p awaken-worker --lib lifecycle::tests
cargo test -p awaken-protocol-managed --features test-support --lib rollout

# Credential cleanup depends on complete envelope/pair projections.
# Exercise SQLite and the canonical in-memory fixture without
# making this SQLite-only formal stage silently skip an optional PostgreSQL.
cargo test -p awaken-credential-vault --lib
cargo test -p awaken-credential-store --features test-support,sqlite --test repo_conformance
# The audit scalar proof assumes exact SQL tuple selection and record equality.
# Keep the production projection and SQLite conformance/migration tests beside it;
# live PostgreSQL parity belongs to the existing required database gate.
cargo test -p awaken-agent-config --lib audit_admission_tests
cargo test -p awaken-agent-config --lib effect_receipt_tests
cargo test -p awaken-control --lib management_effect
cargo test -p awaken-config-store --features test-support --test integration management_effect
cargo test -p awaken-config-store --features test-support --lib audit_identity
cargo test -p awaken-config-store --features test-support --test integration audit_identity
# The source-fenced store has no independent expected-revision input. Cross its
# typed durable receipt with the sole ConfigService effect gate and require both
# a stale conflict and a malformed commit receipt to stop before the registrar.
cargo test -p awaken-config-service --lib \
  publish_never_installs_an_artifact_from_a_stale_source_revision
# The scalar admission proofs below rely on the actual adapters and reclaimer
# calling the shared kernels at their transaction and pre-purge boundaries.
cargo test -p awaken-resource-store --lib reclamation_acquisition_has_one_fail_closed_decision_table
cargo test -p awaken-resource-store --lib sqlite_acquire_rechecks_references_added_by_the_fence_transaction
cargo test -p awaken-resource-reclaimer --lib physical_purge_admission_requires_every_safety_fact
cargo test -p awaken-resource-reclaimer --lib a_late_guard_blocker_releases_the_physical_fence_before_deferring

AWAKEN_FORMAL_TRACE_DIR="$rust_trace_dir" \
  cargo test -p awaken-runtime --test formal_refinement
python3 scripts/ci/render_runtime_refinement_traces.py \
  "$rust_trace_dir" "$rendered_trace_dir" \
  --require-complete-transition-coverage

# Actual application/driver/store observations refine the SAME relations used
# by the Session/Store models below. Missing scenarios fail before TLC runs.
AWAKEN_SESSION_EVIDENCE_DIR="$formal_artifact_dir/session-observations" scripts/ci/check-reliability.sh --session-observations

if command -v cargo-kani >/dev/null 2>&1; then
  scripts/ci/bootstrap_kani.sh --verify-source "$kani_source_dir" \
    > "$formal_artifact_dir/kani-source-build.txt"
  # A matching version string cannot establish full-width enum code generation.
  run_with_deadline "$kani_batch_timeout_seconds" kani \
    scripts/ci/fixtures/kani_wide_niche.rs \
    --target-dir "$formal_tmp_root/kani-toolchain" --output-format terse
  # Kani compiles every selected package once and verifies its named harnesses
  # with a bounded worker pool. The old one-process-per-harness path rebuilt the
  # same package 54 times. Two jobs is the conservative default because CBMC is
  # memory-heavy. A single job is also the reproducible default: Kani 0.67 can
  # race while linking cached per-harness GOTO artifacts from one package.
  # CI may raise AWAKEN_KANI_JOBS only after proving its toolchain/cache layout
  # does not share those output paths.
  kani_jobs="${AWAKEN_KANI_JOBS:-1}"
  # Each package batch is one CBMC resource boundary. An explicit environment
  # override wins; otherwise strict and diagnostic runs both use the same finite
  # 30-minute budget. The shared deadline helper owns descendant termination.
  run_kani() {
    local package="$1" result
    shift
    # Kani does not report an absent solver as an absent tool: every check
    # ends in ERROR (z3) or `execvp <solver> failed` and the batch reads
    # "0 of N failed ... VERIFICATION FAILED", indistinguishable from a proof
    # regression. Decide every external solver before running, for explicit
    # selections and profile-expanded ones alike.
    local argument selecting=0
    for argument in "$@"; do
      if [ "$selecting" -eq 1 ]; then
        selecting=0
        case "$argument" in
          # Compiled into CBMC; nothing to find.
          cadical|minisat) ;;
          z3)
            if ! z3_is_pinned; then
              echo "skipped Z3 harnesses of $package: install the pinned solver ($z3_version) on PATH (TLAPS bundles it)"
              missing=1
              return 0
            fi
            ;;
          *)
            if ! command -v "$argument" >/dev/null 2>&1; then
              echo "skipped $argument harnesses of $package: the $argument solver is not on PATH (the released Kani bundle ships it: $kani_solver_tools)"
              missing=1
              return 0
            fi
            ;;
        esac
      elif [ "$argument" = "--solver" ]; then
        selecting=1
      fi
    done
    # The driver's build-directory requirement is owned by
    # `awaken_configure_kani_driver_target`; scripts/ci/kani.sh applies the same
    # one for a person invoking a harness directly.
    awaken_configure_kani_driver_target
    # Tee'd because the deadline alone cannot say *why* a batch did not finish.
    # A harness whose formula does not fit in RAM does not fail fast: CBMC
    # allocates until the machine thrashes, and the batch then dies on the
    # deadline. Reporting that as "timed out" sends the reader after more time
    # or a faster solver, and neither is the fix -- a 54.9 GiB peak on a 60 GiB
    # host needs a smaller proof or a bigger host. CBMC says so in its own
    # output; the gate has to carry the sentence rather than overwrite it.
    local kani_transcript
    kani_transcript="$(mktemp)"
    if run_with_deadline "$kani_batch_timeout_seconds" \
        cargo kani -p "$package" --output-format terse -j "$kani_jobs" "$@" \
        2>&1 | tee "$kani_transcript"; then
      rm -f "$kani_transcript"
      return 0
    else
      result=${PIPESTATUS[0]}
    fi
    formal_failure_kind="execution-failed"
    formal_failure_stage="cargo-kani"
    formal_failure_subject="$package"
    formal_failure_deadline_seconds=""
    if grep -qF "CBMC appears to have run out of memory" "$kani_transcript"; then
      formal_failure_kind="memory-bound"
      echo "cargo-kani batch ran out of memory: $package" >&2
      echo "  the formula did not fit, so more time and a faster solver do not help;" >&2
      echo "  shrink the harness's symbolic state or verify it on a larger host" >&2
    elif [ "$result" -eq 124 ]; then
      formal_failure_kind="resource-bound"
      formal_failure_deadline_seconds="$kani_batch_timeout_seconds"
      echo "cargo-kani batch timed out: $package (${kani_batch_timeout_seconds}s)" >&2
    fi
    rm -f "$kani_transcript"
    return "$result"
  }
  # Present and exactly the pinned version; a different Z3 is a hard error,
  # because its answers are not the ones the profiles were qualified against.
  z3_is_pinned() {
    local actual
    command -v z3 >/dev/null 2>&1 || return 1
    # Assigned apart from `local` and exited explicitly: this runs inside a
    # negated condition, where errexit would not stop an unreadable version.
    actual="$(z3 --version)" || exit $?
    if [ "$actual" != "$z3_version" ]; then
      echo "unexpected Z3 version: $actual" >&2
      exit 1
    fi
  }
  # The batches are derived, not listed: formal_plan.py groups every
  # `#[kani::proof]` harness by its package and by the Kani selection its own
  # `/// @kani` tag states, and selects each by its exact qualified name. The
  # assignment keeps a planning failure fatal before any batch runs.
  kani_plan="$(python3 scripts/ci/formal_plan.py kani --tier full)"
  if [ -z "$kani_plan" ]; then
    echo "the derived Kani plan is empty: no #[kani::proof] harness was found" >&2
    exit 1
  fi
  mapfile -t kani_batches <<< "$kani_plan"
  for kani_batch in "${kani_batches[@]}"; do
    read -r -a kani_argv <<< "$kani_batch"
    run_kani "${kani_argv[@]}"
  done
else
  echo "skipped Kani: run scripts/ci/bootstrap_kani.sh"
  missing=1
fi

tlapm_bin="${TLAPM_BIN:-}"
if [ -z "$tlapm_bin" ] && command -v tlapm >/dev/null 2>&1; then
  tlapm_bin="$(command -v tlapm)"
fi
if [ -n "$tlapm_bin" ] && [ -x "$tlapm_bin" ]; then
  actual_tlapm_version="$("$tlapm_bin" --version)"
  if [ "$actual_tlapm_version" != "$tlapm_version" ]; then
    echo "unexpected TLAPM version: $actual_tlapm_version" >&2
    exit 1
  fi
  "$tlapm_bin" -I formal/tla formal/tla/RuntimeSystemProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/RuntimeImplementationProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/ThreadCommitProjectionProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/WorkQueueProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/SessionActivityProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/SessionRootProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/SessionStartRootProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/SessionOwnershipProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/SessionNetworkPolicyProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/LiveInboxProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/ServiceLifecycleProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/ObservationReconcileProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/ExecutableProjectionRefreshProof.tla
else
  echo "skipped TLAPS: set TLAPM_BIN or install tlapm"
  missing=1
fi

tla_jar="${TLA2TOOLS_JAR:-}"
if [ -z "$tla_jar" ]; then
  user_data_root="${XDG_DATA_HOME:-${HOME}/.local/share}"
  installed_tla_jar="$user_data_root/tlaplus/tla2tools.jar"
  if [ -f "$installed_tla_jar" ]; then
    tla_jar="$installed_tla_jar"
  fi
fi
if command -v java >/dev/null 2>&1 && [ -n "$tla_jar" ] && [ -f "$tla_jar" ]; then
  tlc_version_output="$(java -cp "$tla_jar" tlc2.TLC -version 2>&1 || true)"
  actual_tlc_version="${tlc_version_output%%$'\n'*}"
  if [ "$actual_tlc_version" != "$tlc_version" ]; then
    echo "unexpected TLC version: $actual_tlc_version" >&2
    exit 1
  fi
  tlc_state_root="$formal_tmp_root/tlc-states"
  python3 scripts/ci/check_formal_mutations.py --session-observation-model-mutants "$tla_jar"
  AWAKEN_FORMAL_MUTATION_EVIDENCE_DIR="$formal_artifact_dir/observation-mutations" \
    python3 scripts/ci/check_formal_mutations.py --session-observation-trace-mutants "$tla_jar" --observation-trace-dir "$observation_trace_dir"

  python3 scripts/ci/check_formal_mutations.py --cowork-lifecycle-model-mutants "$tla_jar"
  # The standalone instance loads the canonical transition owner from
  # formal/tla/RunIngressKernel.tla; TLC checks that composition through this
  # one configuration rather than maintaining a second kernel configuration.
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/run-ingress" \
    -config formal/tla/RunIngress.cfg formal/tla/RunIngress.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/commit-epoch-guard" \
    -config formal/tla/CommitEpochGuard.cfg formal/tla/CommitEpochGuard.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/dream-cleanup-boundary" \
    -config formal/tla/DreamCleanupBoundary.cfg \
    formal/tla/DreamCleanupBoundary.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/thread-state" \
    -config formal/tla/ThreadState.cfg formal/tla/ThreadState.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/store-history" \
    -config formal/tla/StoreHistory.cfg formal/tla/StoreHistory.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/work-queue" \
    -config formal/tla/WorkQueue.cfg formal/tla/WorkQueue.tla
  python3 scripts/ci/check_formal_mutations.py --work-queue-stop-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-activity" \
    -config formal/tla/SessionActivity.cfg formal/tla/SessionActivity.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-root" \
    -config formal/tla/SessionRoot.cfg formal/tla/SessionRoot.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-event-protocol" \
    -config formal/tla/SessionEventProtocol.cfg \
    formal/tla/SessionEventProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/delegation" \
    -config formal/tla/Delegation.cfg formal/tla/Delegation.tla
  python3 scripts/ci/check_formal_mutations.py --delegation-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/tool-batch" \
    -config formal/tla/ToolBatch.cfg formal/tla/ToolBatch.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/tool-schedule" \
    -config formal/tla/ToolSchedule.cfg formal/tla/ToolSchedule.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/runtime-system" \
    -config formal/tla/RuntimeSystem.cfg formal/tla/RuntimeSystem.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/runtime-implementation" \
    -config formal/tla/RuntimeImplementation.cfg formal/tla/RuntimeImplementation.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/thread-commit-projection" \
    -config formal/tla/ThreadCommitProjection.cfg formal/tla/ThreadCommitProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/remote-tool" \
    -config formal/tla/RemoteTool.cfg formal/tla/RemoteTool.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/remote-attempt" \
    -config formal/tla/RemoteAttempt.cfg formal/tla/RemoteAttempt.tla
  python3 scripts/ci/check_formal_mutations.py --remote-attempt-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/authz-kernel" \
    -config formal/tla/AuthzKernel.cfg formal/tla/AuthzKernel.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-ownership" \
    -config formal/tla/SessionOwnership.cfg formal/tla/SessionOwnership.tla
  python3 scripts/ci/check_formal_mutations.py --session-ownership-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-deletion" \
    -config formal/tla/SessionDeletion.cfg formal/tla/SessionDeletion.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-deletion-unconfirmed" \
    -config formal/tla/SessionDeletionUnconfirmed.cfg formal/tla/SessionDeletion.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/workspace-path-projection" \
    -config formal/tla/WorkspacePathProjection.cfg \
    formal/tla/WorkspacePathProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-runtime-projection" \
    -config formal/tla/SessionRuntimeProjection.cfg \
    formal/tla/SessionRuntimeProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-runtime-projection-opening" \
    -config formal/tla/SessionRuntimeProjectionOpening.cfg \
    formal/tla/SessionRuntimeProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-environment-projection" \
    -config formal/tla/SessionEnvironmentProjection.cfg \
    formal/tla/SessionEnvironmentProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-environment-convergence" \
    -config formal/tla/SessionEnvironmentConvergence.cfg \
    formal/tla/SessionEnvironmentProjection.tla
  java -Djava.io.tmpdir="$tlc_state_root" -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-environment-terminal-owner" \
    -config formal/tla/SessionEnvironmentTerminalOwner.cfg \
    formal/tla/SessionEnvironmentProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/managed-projection-publish" \
    -config formal/tla/ManagedProjectionPublish.cfg \
    formal/tla/ManagedProjectionPublish.tla
  java -Djava.io.tmpdir="$tlc_state_root" -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/managed-projection-inputs" \
    -config formal/tla/ManagedProjectionPublishInputs.cfg \
    formal/tla/ManagedProjectionPublish.tla
  java -Djava.io.tmpdir="$tlc_state_root" -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/managed-projection-topology" \
    -config formal/tla/ManagedProjectionPublishTopology.cfg \
    formal/tla/ManagedProjectionPublish.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/agent-message-protocol" \
    -config formal/tla/AgentMessageProtocol.cfg \
    formal/tla/AgentMessageProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/agent-message-protocol-early-receipt" \
    -config formal/tla/AgentMessageProtocolEarlyReceipt.cfg \
    formal/tla/AgentMessageProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/agent-message-protocol-late-receipt-recovery" \
    -config formal/tla/AgentMessageProtocolLateReceiptRecovery.cfg \
    formal/tla/AgentMessageProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/tool-permission-policy" \
    -config formal/tla/ToolPermissionPolicy.cfg \
    formal/tla/ToolPermissionPolicy.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-network-policy" \
    -config formal/tla/SessionNetworkPolicy.cfg formal/tla/SessionNetworkPolicy.tla
  python3 scripts/ci/check_formal_mutations.py --session-network-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/circuit-breaker" \
    -config formal/tla/CircuitBreaker.cfg formal/tla/CircuitBreaker.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/config-cas" \
    -config formal/tla/AggregateCAS.cfg formal/tla/AggregateCAS.tla
  # Config family owns the five counterexamples; the shared dispatcher/runner executes them.
  python3 scripts/ci/check_formal_mutations.py --config-cas-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/deployment-cas" \
    -config formal/tla/DeploymentCAS.cfg formal/tla/DeploymentCAS.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/live-inbox" \
    -config formal/tla/LiveInbox.cfg formal/tla/LiveInbox.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/service-lifecycle" \
    -config formal/tla/ServiceLifecycle.cfg formal/tla/ServiceLifecycle.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/observation-reconcile" \
    -config formal/tla/ObservationReconcile.cfg formal/tla/ObservationReconcile.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/executable-projection-refresh" \
    -config formal/tla/ExecutableProjectionRefresh.cfg \
    formal/tla/ExecutableProjectionRefresh.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/checkpoint-recovery" \
    -config formal/tla/CheckpointRecovery.cfg formal/tla/CheckpointRecovery.tla
  python3 scripts/ci/check_formal_mutations.py --checkpoint-recovery-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/background-task" \
    -config formal/tla/BackgroundTask.cfg formal/tla/BackgroundTask.tla
  python3 scripts/ci/check_formal_mutations.py --background-completion-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/sandbox-control-publication" \
    -config formal/tla/SandboxControlPublication.cfg \
    formal/tla/SandboxControlPublication.tla
  python3 scripts/ci/check_formal_mutations.py --sandbox-control-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/memory-invalidation-lifecycle" \
    -config formal/tla/MemoryInvalidationLifecycle.cfg \
    formal/tla/MemoryInvalidationLifecycle.tla
  python3 scripts/ci/check_formal_mutations.py --memory-invalidation-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/memory-participant-ownership" \
    -config formal/tla/MemoryParticipantOwnership.cfg \
    formal/tla/MemoryParticipantOwnership.tla
  python3 scripts/ci/check_formal_mutations.py --memory-participant-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/memory-extraction" \
    -config formal/tla/MemoryExtraction.cfg \
    formal/tla/MemoryExtraction.tla
  python3 scripts/ci/check_formal_mutations.py --memory-extraction-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/eval-evidence" \
    -config formal/tla/EvalEvidence.cfg formal/tla/EvalEvidence.tla
  python3 scripts/ci/check_formal_mutations.py --eval-evidence-model-mutants "$tla_jar"
  cargo test -p awaken-eval --lib \
    outcome_judge::tests::eval_evidence_decision_table_cross_checks_frozen_complete_model --locked -- --exact
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/webhook-outbox" \
    -config formal/tla/WebhookOutbox.cfg formal/tla/WebhookOutbox.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/registration-intent" \
    -config formal/tla/RegistrationIntent.cfg formal/tla/RegistrationIntent.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/erasure-saga" \
    -config formal/tla/ErasureSaga.cfg formal/tla/ErasureSaga.tla
  python3 scripts/ci/check_formal_mutations.py --erasure-checkpoint-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/credential-creation" \
    -config formal/tla/CredentialCreation.cfg formal/tla/CredentialCreation.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/memory-cas" \
    -config formal/tla/MemoryCAS.cfg formal/tla/MemoryCAS.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/memory-cas-copy" \
    -config formal/tla/MemoryCASCopy.cfg formal/tla/MemoryCAS.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/memory-retention" \
    -config formal/tla/MemoryCASRetention.cfg formal/tla/MemoryCAS.tla
  python3 scripts/ci/check_formal_mutations.py --memory-copy-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-process-lifecycle" \
    -config formal/tla/LocalProcessLifecycle.cfg formal/tla/LocalProcessLifecycle.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-process-scope" \
    -config formal/tla/LocalProcessScope.cfg formal/tla/LocalProcessLifecycle.tla
  python3 scripts/ci/check_formal_mutations.py --local-process-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-process-boundary" \
    -config formal/tla/LocalProcessBoundary.cfg formal/tla/LocalProcessLifecycle.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-process-renewal" \
    -config formal/tla/LocalProcessRenewal.cfg formal/tla/LocalProcessLifecycle.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-process-recovery" \
    -config formal/tla/LocalProcessRecovery.cfg formal/tla/LocalProcessRecovery.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-process-refresh" \
    -config formal/tla/LocalProcessRefresh.cfg formal/tla/LocalProcessRefresh.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-repository-effect" \
    -config formal/tla/LocalRepositoryEffect.cfg formal/tla/LocalRepositoryEffect.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/local-process-deadline" \
    -config formal/tla/LocalProcessDeadline.cfg formal/tla/LocalProcessDeadline.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-execution-binding" \
    -config formal/tla/SessionExecutionBinding.cfg formal/tla/SessionExecutionBinding.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-execution-replacement" \
    -config formal/tla/SessionExecutionReplacement.cfg formal/tla/SessionExecutionReplacement.tla
  java -Djava.io.tmpdir="$tlc_state_root" -Xmx2048m -XX:+UseParallelGC -jar "$tla_jar" -workers 4 \
    -metadir "$tlc_state_root/session-execution-replacement-chain" \
    -config formal/tla/SessionExecutionReplacementChain.cfg formal/tla/SessionExecutionReplacement.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/skill-version-pin" \
    -config formal/tla/SkillVersionPin.cfg formal/tla/SkillVersionPin.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/tool-result-protocol" \
    -config formal/tla/ToolResultProtocol.cfg formal/tla/ToolResultProtocol.tla
  python3 scripts/ci/check_formal_mutations.py --event-batch-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-drain" \
    -config formal/tla/WorkerDrain.cfg formal/tla/WorkerDrain.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-authority-evidence" \
    -config formal/tla/WorkerAuthorityEvidence.cfg formal/tla/WorkerAuthorityEvidence.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-credential-liveness" \
    -config formal/tla/WorkerCredentialLiveness.cfg \
    formal/tla/WorkerCredentialLiveness.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/acp-local-binding-batch" \
    -config formal/tla/AcpLocalBindingBatch.cfg formal/tla/AcpLocalBindingBatch.tla
  python3 scripts/ci/check_formal_mutations.py --acp-binding-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/acp-observation-refresh" \
    -config formal/tla/AcpObservationRefresh.cfg formal/tla/AcpObservationRefresh.tla
  python3 scripts/ci/check_formal_mutations.py --acp-observation-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/acp-projected-binding-use" \
    -config formal/tla/AcpProjectedBindingUse.cfg formal/tla/AcpProjectedBindingUse.tla
  python3 scripts/ci/check_formal_mutations.py --acp-projection-model-mutants "$tla_jar"
  python3 scripts/ci/check_formal_mutations.py --managed-projection-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/audit-commit" \
    -config formal/tla/AuditCommit.cfg formal/tla/AuditCommit.tla
  python3 scripts/ci/check_formal_mutations.py --audit-commit-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/config-activation" \
    -config formal/tla/ConfigActivation.cfg formal/tla/ConfigActivation.tla
  python3 scripts/ci/check_formal_mutations.py --config-activation-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/inference-access-publication" \
    -config formal/tla/InferenceAccessPublication.cfg \
    formal/tla/InferenceAccessPublication.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/resource-binding-effect" \
    -config formal/tla/ResourceBindingEffect.cfg formal/tla/ResourceBindingEffect.tla
  python3 scripts/ci/check_formal_mutations.py --effect-retirement-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/agent-input-revision" \
    -config formal/tla/AgentInputRevision.cfg formal/tla/AgentInputRevision.tla
  python3 scripts/ci/check_formal_mutations.py --agent-input-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/outcome-lifecycle" \
    -config formal/tla/OutcomeLifecycle.cfg formal/tla/OutcomeLifecycle.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-resource-activation" \
    -config formal/tla/SessionResourceActivation.cfg \
    formal/tla/SessionResourceActivation.tla
  python3 scripts/ci/check_formal_mutations.py --terminal-resource-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-unmaterialized-cleanup" \
    -config formal/tla/SessionUnmaterializedCleanup.cfg formal/tla/SessionUnmaterializedCleanup.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-unmaterialized-cleanup-target" \
    -config formal/tla/SessionUnmaterializedCleanupTarget.cfg formal/tla/SessionUnmaterializedCleanup.tla
  python3 scripts/ci/check_formal_mutations.py --unmaterialized-cleanup-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/resource-dispatch" \
    -config formal/tla/ResourceDispatch.cfg formal/tla/ResourceDispatch.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/resource-reclamation" \
    -config formal/tla/ResourceReclamation.cfg \
    formal/tla/ResourceReclamation.tla
  python3 scripts/ci/check_formal_mutations.py --file-reclamation-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/management-audit-intent" \
    -config formal/tla/ManagementAuditIntent.cfg formal/tla/ManagementAuditIntent.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/credential-inventory" \
    -config formal/tla/CredentialInventory.cfg formal/tla/CredentialInventory.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/managed-credential-creation" \
    -config formal/tla/ManagedCredentialCreation.cfg \
    formal/tla/ManagedCredentialCreation.tla
  python3 scripts/ci/check_formal_mutations.py --credential-creation-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/verified-profile-admission" \
    -config formal/tla/VerifiedProfileAdmission.cfg formal/tla/VerifiedProfileAdmission.tla
  python3 scripts/ci/check_formal_mutations.py --verified-admission-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/managed-credential-rollout" \
    -config formal/tla/ManagedCredentialRollout.cfg \
    formal/tla/ManagedCredentialRollout.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/rollout-event-identity" \
    -config formal/tla/RolloutEventIdentity.cfg \
    formal/tla/RolloutEventIdentity.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/managed-vault-deletion" \
    -config formal/tla/ManagedVaultDeletion.cfg \
    formal/tla/ManagedVaultDeletion.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/mcp-server" \
    -config formal/tla/McpServer.cfg formal/tla/McpServer.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/mcp-credential-delivery" \
    -config formal/tla/McpCredentialDelivery.cfg \
    formal/tla/McpCredentialDelivery.tla
  python3 scripts/ci/check_formal_mutations.py --mcp-credential-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-replacement" \
    -config formal/tla/WorkerReplacement.cfg formal/tla/WorkerReplacement.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/acp-boundary" \
    -config formal/tla/AcpBoundary.cfg formal/tla/AcpBoundary.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/credential-effect-boundary" \
    -config formal/tla/CredentialEffectBoundary.cfg \
    formal/tla/CredentialEffectBoundary.tla
  python3 scripts/ci/check_formal_mutations.py --credential-effect-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-realization-mutex" \
    -config formal/tla/SessionRealizationMutex.cfg \
    formal/tla/SessionRealizationMutex.tla
  python3 scripts/ci/check_formal_mutations.py --resource-preparation-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-realization-publication" \
    -config formal/tla/SessionRealizationPublication.cfg formal/tla/SessionRealizationMutex.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-realization-renewal" \
    -config formal/tla/SessionRealizationRenewal.cfg formal/tla/SessionRealizationMutex.tla
  python3 scripts/ci/check_formal_mutations.py --realization-renewal-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-realization-authority" \
    -config formal/tla/SessionRealizationAuthority.cfg formal/tla/SessionRealizationAuthority.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-authority-exit" \
    -config formal/tla/WorkerAuthorityExit.cfg formal/tla/WorkerAuthorityExit.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-execution-authority" \
    -config formal/tla/WorkerExecutionAuthority.cfg formal/tla/WorkerExecutionAuthority.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/credential-rotation-workflow" \
    -config formal/tla/CredentialRotationWorkflow.cfg \
    formal/tla/CredentialRotationWorkflow.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/credential-rotation-workflow-reachability" \
    -config formal/tla/CredentialRotationWorkflowReachability.cfg \
    formal/tla/CredentialRotationWorkflow.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/deployment-execution-workflow" \
    -config formal/tla/DeploymentExecutionWorkflow.cfg \
    formal/tla/DeploymentExecutionWorkflow.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/deployment-execution-workflow-reachability" \
    -config formal/tla/DeploymentExecutionWorkflowReachability.cfg \
    formal/tla/DeploymentExecutionWorkflow.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/resource-lifecycle-workflow" \
    -config formal/tla/ResourceLifecycleWorkflow.cfg \
    formal/tla/ResourceLifecycleWorkflow.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/resource-lifecycle-workflow-reachability" \
    -config formal/tla/ResourceLifecycleWorkflowReachability.cfg \
    formal/tla/ResourceLifecycleWorkflow.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/session-run-protocol" \
    -config formal/tla/SessionRunProtocol.cfg formal/tla/SessionRunProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-run-retirement" \
    -config formal/tla/SessionRunProtocolRetirement.cfg formal/tla/SessionRunProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-run-resolution" \
    -config formal/tla/SessionRunProtocolResolution.cfg formal/tla/SessionRunProtocol.tla
  # The mutation family checks formal/tla/SessionRunCommitObservation.cfg over
  # the canonical ObserveCommittedRun action in SessionRunProtocol.tla.
  # The same gate also checks formal/tla/SessionRunStopEvidence.tla with
  # formal/tla/SessionRunStopEvidence.cfg, participant mutants and conditional
  # delivery fairness. It imports the original SessionRunProtocol transitions.
  python3 scripts/ci/check_formal_mutations.py --resolution-failure-model-mutant "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/terminal-reconciliation-scan" \
    -config formal/tla/TerminalReconciliationScan.cfg formal/tla/TerminalReconciliationScan.tla
  python3 scripts/ci/check_formal_mutations.py --terminal-scan-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/dispatch-maintenance-scheduling" \
    -config formal/tla/DispatchMaintenanceScheduling.cfg formal/tla/DispatchMaintenanceScheduling.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/k8s-process-observation" \
    -config formal/tla/K8sProcessObservation.cfg formal/tla/K8sProcessObservation.tla
  python3 scripts/ci/check_formal_mutations.py --k8s-process-observation-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-run-cancellation" \
    -config formal/tla/SessionRunProtocolCancellation.cfg formal/tla/SessionRunProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/session-thread-tool-composition" \
    -config formal/tla/SessionThreadToolComposition.cfg \
    formal/tla/SessionThreadToolComposition.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/session-start-protocol" \
    -config formal/tla/SessionStartProtocol.cfg \
    formal/tla/SessionStartProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/session-start-protocol-reachability" \
    -config formal/tla/SessionStartProtocolReachability.cfg \
    formal/tla/SessionStartProtocol.tla
  python3 scripts/ci/check_formal_mutations.py --session-start-root-model-mutants "$tla_jar"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/remote-worker-protocol" \
    -config formal/tla/RemoteWorkerProtocol.cfg formal/tla/RemoteWorkerProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/remote-worker-registration" \
    -config formal/tla/RemoteWorkerProtocolRegistration.cfg formal/tla/RemoteWorkerProtocol.tla
  python3 scripts/ci/check_formal_mutations.py --worker-registration-model-mutants "$tla_jar"
  for trace_config in "$rendered_trace_dir"/RustTrace*.cfg "$observation_rendered_dir"/RustTrace*.cfg; do
    trace_module="${trace_config%.cfg}.tla"
    trace_name="$(basename "$trace_module" .tla)"
    java -XX:+UseParallelGC \
      -jar "$tla_jar" \
      -metadir "$tlc_state_root/$trace_name" \
      -config "$trace_config" "$trace_module" 2>&1 | tee "$formal_artifact_dir/$trace_name.log"
  done
else
  echo "skipped TLC: set TLA2TOOLS_JAR and install Java 11+"
  missing=1
fi

if [ "$require_tools" -eq 1 ] && [ "$missing" -ne 0 ]; then
  echo "formal verification tools are required but unavailable" >&2
  exit 1
fi
