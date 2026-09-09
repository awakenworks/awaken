#!/usr/bin/env bash
set -euo pipefail

readonly tlc_version="TLC2 Version 2.19 of 08 August 2024 (rev: 5a47802)"
readonly tlapm_version="763bf3c"
readonly z3_version="Z3 version 4.8.9 - 64 bit"

cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"
source scripts/ci/_deadline.sh

# Prefer the repository-pinned source build when bootstrapped. The published
# Kani 0.67 bundle embeds Rust 1.93 and cannot compile the workspace's Rust 1.96
# IAM dependencies. Its setup still owns the CBMC/GOTO backend binaries.
kani_cache_root="${AWAKEN_KANI_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/awaken-kani}"
kani_source_dir="${AWAKEN_KANI_SOURCE_DIR:-$kani_cache_root/current}"
if [ -x "$kani_source_dir/scripts/cargo-kani" ]; then
  # Freeze this invocation's selection before another bootstrap moves current.
  kani_source_dir="$(cd "$kani_source_dir" && pwd -P)"
  kani_backend_dir="${AWAKEN_KANI_BACKEND_DIR:-}"
  if [ -z "$kani_backend_dir" ]; then
    kani_backend_dir="$HOME/.kani/kani-0.67.0/bin"
  fi
  export PATH="$kani_source_dir/scripts${kani_backend_dir:+:$kani_backend_dir}:$PATH"
fi

require_tools=0
if [ "${1:-}" = "--require-tools" ]; then
  require_tools=1
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
rust_trace_dir="$formal_artifact_dir/rust-traces"
rendered_trace_dir="$formal_artifact_dir/rendered-traces"
observation_trace_dir="$formal_artifact_dir/session-observations/raw"
observation_rendered_dir="$formal_artifact_dir/session-observations/rendered"

python3 scripts/ci/check_feature_coverage.py --require-complete
python3 scripts/ci/check_formal_coverage.py --require-complete \
  --require-guardrail G42 \
  --require-guardrail G43
python3 scripts/ci/test_formal_profile.py
python3 scripts/ci/test_formal_run_evidence.py
python3 scripts/ci/test_bootstrap_kani.py
python3 scripts/release/verification_manifest.py self-test
python3 scripts/ci/test_formal_surface.py
python3 scripts/ci/check_formal_surface.py --require-complete
python3 scripts/ci/check_formal_web_surface.py
python3 scripts/ci/check_proof_boundaries.py
python3 scripts/ci/check_formal_mutations.py

# The abstract network algebra is not Rust String/Vec refinement. Exercise the
# actual owner and frozen SQLite root alongside its TLAPS/TLC checks below.
cargo test -p awaken-protocol-managed --lib projection_publish_decision_covers_cache_and_source_fences
cargo test -p awaken-session-contract --lib network_policy::tests
cargo test -p awaken-session-application --lib profiled_session_creation_enforces_publication_and_upfront_inputs
cargo test -p awaken-session-application --lib realization::publication::collection_tests

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
# Exercise the production BackgroundTask projection under callback/replacement
# races. Only its single mutex is instrumented; durable commits remain outside
# this bounded lock-interleaving claim.
cargo test -p awaken-ext-background-task --features loom --lib supervisor::loom_tests
# The Kani predicate below is not sufficient evidence of the async wiring.
# Exercise its aggregate projection and real post-concurrency-admission check.
cargo test -p awaken-ext-background-task --test state_machine execution_admission
cargo test -p awaken-session-executor --lib background_task::ownership::tests
# Async return/cleanup is not covered by the scalar physical-slot proof.
cargo test -p awaken-dispatch --features test-support --test pool_memory
cargo test -p awaken-dispatch --features test-support --test lease_semantics
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
    local package="$1" result expanded
    local -a checked_options
    shift
    # Profile expansion is fail-closed: command substitution status is checked
    # before readarray, so an invalid binding never falls back to default flags.
    if [[ " $* " == *" --checked-loop-profile "* ]]; then
      if expanded=$(python3 scripts/ci/formal_kani_options.py "$package" "$@"); then
        readarray -t checked_options <<< "$expanded"
        set -- "${checked_options[@]}"
      else
        result=$?
        return "$result"
      fi
    fi
    # C1 Cargo has an independent build-dir (user/CI configuration); E1 Kani's
    # driver still reads target/kani/debug artifacts. Keep compiler and driver
    # in that one authored tree; otherwise valid compilation ends in ENOENT.
    if CARGO_BUILD_BUILD_DIR="${CARGO_TARGET_DIR%/}/kani" \
      run_with_deadline "$kani_batch_timeout_seconds" \
        cargo kani -p "$package" --output-format terse -j "$kani_jobs" "$@"; then
      return 0
    else
      result=$?
    fi
    formal_failure_kind="execution-failed"
    formal_failure_stage="cargo-kani"
    formal_failure_subject="$package"
    formal_failure_deadline_seconds=""
    if [ "$result" -eq 124 ]; then
      formal_failure_kind="resource-bound"
      formal_failure_deadline_seconds="$kani_batch_timeout_seconds"
      echo "cargo-kani batch timed out: $package (${kani_batch_timeout_seconds}s)" >&2
    fi
    return "$result"
  }
  run_kani awaken-agent-contract \
    --harness ended_is_absorbing_for_every_next_state \
    --harness wire_accepts_exactly_the_legal_dispositions \
    --harness run_identity_binding_is_exactly_run_scoped \
    --harness exclusive_state_conflict_requires_every_exact_precondition \
    --harness state_materialization_effect_is_total_and_exact \
    --harness only_unsettled_relationships_occupy_a_parallel_slot \
    --harness every_relationship_effect_has_one_documented_precondition \
    --harness delegation_admission_requires_every_budget_and_lineage_guard \
    --harness cancellation_delivery_is_enabled_only_by_durable_intent \
    --harness operation_identity_replays_only_the_same_run_and_payload \
    --harness operation_append_requires_exact_prefix_and_nonwrapping_version \
    --harness applied_receipt_is_exact_and_names_one_successor_record \
    --harness tool_policy_selector_fails_closed_for_unregistered_widened_or_mismatched_calls \
    --harness stream_terminal_projection_is_exact_fail_closed_and_absorbing
  run_kani awaken-authorization-contract \
    --harness run_backed_route_policy_uses_exact_run_actions \
    --harness application_route_policy_never_enters_the_service_guard \
    --harness credential_ingress_role_contains_only_workspace_apikey_authority \
    --harness agent_publisher_role_contains_only_workspace_model_read_and_skill_authority \
    --harness hosted_admin_role_contains_exactly_its_seven_workspace_authorities \
    --harness hosted_builder_role_has_writes_without_apikey_or_model_administration \
    --harness workspace_member_role_contains_exactly_read_only_workspace_authorities \
    --harness runtime_member_role_contains_exactly_run_read \
    --harness legacy_workspace_binding_migration_is_idempotent_and_authority_exact
  run_kani awaken-acp-contract \
    --harness acp_capability_is_detected_exactly_after_a_verified_observation
  run_kani awaken-authz-enforce \
    --harness session_decision_collapse_allows_only_explicit_allow \
    --harness session_method_mapping_is_total_and_read_only_for_exact_safe_methods
  run_kani awaken-environment-realization-contract \
    --harness environment_image_build_claim_is_exact_non_wrapping_and_blocked_is_inert \
    --harness environment_image_build_mutation_requires_exact_claim_and_admissible_command
  run_kani awaken-environment-contract \
    --harness environment_registration_intent_is_exact_idempotent_and_recoverable
  # The actual deleted resolver covers empty-inventory replay and one legacy
  # error/frame/Drop call. Legacy replay remains open.
  run_kani awaken-session-contract -Z unstable-options \
    --harness deleted_admission_resolver_preserves_empty \
    --harness deleted_admission_resolver_preserves_legacy_error_and_frame \
    --cbmc-args --unwindset memcmp.0:64
  # All 24 None/empty/one-ID cases plus two-ID Absent 24/28/30 and Closed31;
  # full source frame and two-live-output replay/Drop (two-ID output stays open).
  run_kani awaken-session-contract -Z unstable-options \
    --harness deleted_admission_one_retained_absent_none_00 \
    --harness deleted_admission_one_retained_absent_none_02 \
    --harness deleted_admission_one_retained_accepted_none_03 \
    --harness deleted_admission_one_retained_absent_none_04 \
    --harness deleted_admission_one_retained_absent_none_06 \
    --harness deleted_admission_one_retained_closed_none_07 \
    --harness deleted_admission_one_retained_absent_empty_08 \
    --harness deleted_admission_one_retained_absent_empty_10 \
    --harness deleted_admission_one_retained_accepted_empty_11 \
    --harness deleted_admission_one_retained_absent_empty_12 \
    --harness deleted_admission_one_retained_absent_empty_14 \
    --harness deleted_admission_one_retained_closed_empty_15 \
    --harness deleted_admission_one_retained_absent_one_16 \
    --harness deleted_admission_one_retained_absent_one_18 \
    --harness deleted_admission_one_retained_accepted_one_19 \
    --harness deleted_admission_one_retained_absent_one_20 \
    --harness deleted_admission_one_retained_absent_one_22 \
    --harness deleted_admission_one_retained_closed_one_23 \
    --harness deleted_admission_one_retained_absent_two_24 \
    --harness deleted_admission_one_retained_absent_two_28 \
    --harness deleted_admission_one_retained_absent_two_30 \
    --harness deleted_admission_one_retained_closed_two_31 \
    --cbmc-args --unwindset memcmp.0:2
  # The complete 64-byte conflict message also needs its loop-exit check.
  run_kani awaken-session-contract -Z unstable-options \
    --harness deleted_admission_one_retained_conflict_none_01 \
    --harness deleted_admission_one_retained_conflict_none_05 \
    --harness deleted_admission_one_retained_conflict_empty_09 \
    --harness deleted_admission_one_retained_conflict_empty_13 \
    --harness deleted_admission_one_retained_conflict_one_17 \
    --harness deleted_admission_one_retained_conflict_one_21 \
    --cbmc-args --unwindset memcmp.0:65
  run_kani awaken-session-contract \
    --harness session_event_batch_admission_resolution_is_total_closed_and_append_once \
    --harness session_agent_boundary_requires_a_visible_committed_target \
    --harness user_event_processing_requires_exact_applied_dispatch_and_activity_settlement \
    --harness terminal_resource_projection_retains_the_exact_desired_generation \
    --harness initial_event_plan_is_installed_only_on_the_complete_fresh_root \
    --harness session_run_reservation_binds_exactly_one_nonzero_activity_receipt \
    --harness session_network_intersection_two_by_two_ascii_hosts_partition \
    --harness session_network_intersection_none_by_none_partition \
    --harness session_network_intersection_none_by_unrestricted_partition \
    --harness session_network_intersection_none_by_empty_partition \
    --harness session_network_intersection_none_by_one_ascii_host_partition \
    --harness session_network_intersection_none_by_two_ascii_hosts_partition \
    --harness session_network_intersection_unrestricted_by_none_partition \
    --harness session_network_intersection_unrestricted_by_unrestricted_partition \
    --harness session_network_intersection_unrestricted_by_empty_partition \
    --harness session_network_intersection_unrestricted_by_one_ascii_host_partition \
    --harness session_network_intersection_unrestricted_by_two_ascii_hosts_partition \
    --harness session_network_intersection_empty_by_none_partition \
    --harness session_network_intersection_empty_by_unrestricted_partition \
    --harness session_network_intersection_empty_by_empty_partition \
    --harness session_network_intersection_empty_by_one_ascii_host_partition \
    --harness session_network_intersection_empty_by_two_ascii_hosts_partition \
    --harness session_network_intersection_one_ascii_host_by_none_partition \
    --harness session_network_intersection_one_ascii_host_by_unrestricted_partition \
    --harness session_network_intersection_one_ascii_host_by_empty_partition \
    --harness session_network_intersection_one_ascii_host_by_one_ascii_host_partition \
    --harness session_network_intersection_one_ascii_host_by_two_ascii_hosts_partition \
    --harness session_network_intersection_two_ascii_hosts_by_none_partition \
    --harness session_network_intersection_two_ascii_hosts_by_unrestricted_partition \
    --harness session_network_intersection_two_ascii_hosts_by_empty_partition \
    --harness session_network_intersection_two_ascii_hosts_by_one_ascii_host_partition \
    --harness session_network_normalization_none_partition \
    --harness session_network_normalization_unrestricted_partition \
    --harness session_network_normalization_empty_partition \
    --harness session_network_normalization_one_ascii_host_partition \
    --harness session_network_normalization_two_ascii_hosts_partition \
    --harness session_root_replacement_preserves_creation_and_monotonic_update_observations \
    --harness public_millisecond_projection_is_exact_bounded_and_nonwrapping \
    --harness session_utc_projection_domain_is_total_exact_and_idempotent \
    --harness realization_generation_authority_requires_every_axis \
    --harness processed_event_time_is_first_observation_or_atomic_rejection \
    --harness session_realization_control_failure_disposition_is_total_exact_and_fail_closed \
    --harness realization_ownership_loss_proof_is_total_and_exact \
    --harness session_model_override_reuses_only_the_same_identity_and_resolves_every_mismatch \
    --harness awaiting_constructor_cannot_create_a_terminal_or_failed_outcome \
    --harness ended_constructor_carries_the_only_failure_authority_and_no_pending_tool \
    --harness settled_step_has_no_parallel_observation_authority \
    --harness only_queued_work_is_claimable \
    --harness only_active_work_accepts_lease_extension \
    --harness stop_is_absorbing_for_every_work_state \
    --harness work_lease_mutation_requires_present_exact_authority_and_matching_condition \
    --harness session_work_release_requires_exact_owner_and_epoch \
    --harness first_heartbeat_is_authorized_exactly_once \
    --harness matching_heartbeat_rejects_every_other_receipt \
    --harness resolved_environment_snapshot_accepts_only_exact_positive_identity_and_revision \
    --harness stopped_session_work_is_revived_only_by_a_claimed_nonterminal_run \
    --harness accepted_managed_budget_cost_never_wraps \
    --harness source_preparation_and_disposal_require_distinct_durable_phases \
    --harness resource_materialization_selects_only_active_or_pending_root_generation \
    --harness failed_resource_replacement_never_promotes_the_pending_manifest \
    --harness resource_retry_reuses_exact_pending_revision \
    --harness terminal_execution_never_reopens \
    --harness realization_renewal_never_widens_owner_epoch_or_expiry_authority \
    --harness session_realization_assignment_never_shares_a_live_foreign_owner_or_reuses_an_epoch \
    --harness runtime_intervals_open_once_and_never_close_before_start \
    --harness bounded_runtime_capability_iterator_uses_exact_member_projection \
    --harness runtime_capability_member_projection_is_total_exact_and_bounded \
    --harness terminal_environment_receipt_is_admitted_only_during_the_fenced_join \
    --harness initial_realization_recovery_is_exact_for_every_execution_state \
    --harness skill_execution_pin_requires_exact_workspace_revision_and_hash \
    --harness every_skill_execution_pin_axis_is_binding \
    --harness frozen_worker_agent_publication_is_exact_or_fails_closed \
    --harness terminal_dream_statuses_are_absorbing \
    --harness dream_public_terminal_requires_committed_cleanup \
    --harness dream_cleanup_admission_is_total_and_retries_every_pending_terminal \
    --harness dream_success_and_failure_require_a_running_process \
    --harness dream_recovery_and_cancel_never_widen_terminal_authority \
    --harness dream_transition_table_is_total_exact_and_closed \
    --harness quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects \
    --harness checkpoint_receipt_requires_every_immutable_generation_axis \
    --harness source_disposal_receipt_requires_exact_binding_and_termination \
    --harness restore_receipt_requires_exact_checkpoint_and_nonempty_binding \
    --harness session_cleanup_completion_requires_every_identity_axis \
    --harness session_cleanup_phase_advances_only_not_requested_fenced_requested_completed \
    --harness session_delete_request_plan_is_exact_hidden_terminal_and_idempotent \
    --harness session_tombstone_requires_hidden_disposition_terminal_execution_and_verified_cleanup \
    --harness terminal_batch_resolution_empty
  run_kani awaken-session-contract --checked-loop-profile activity-admission-height-v1 \
    --harness activity_next_epoch_inserts_exact_checked_successor \
    --harness activity_exact_epoch_inserts_only_fresh_positive_member \
    --harness activity_next_next_admissions_preserve_exact_members \
    --harness activity_next_exact_admissions_preserve_exact_members \
    --harness activity_exact_next_admissions_preserve_exact_members \
    --harness activity_exact_exact_admissions_preserve_exact_members
  run_kani awaken-session-contract --checked-loop-profile activity-terminal-height-v1 \
    --harness activity_terminal_transition_clears_real_members
  run_kani awaken-session-store \
    --harness session_identity_and_create_shapes_require_one_owned_live_row
  run_kani awaken-service-auth-contract \
    --harness service_token_retry_is_enabled_only_for_an_exact_changed_token
  run_kani awaken-session-application \
    --harness session_activity_open_and_settlement_plans_are_total_exact_and_fail_closed \
    --harness realization_publication_selects_only_unacknowledged_active_generation \
    --harness session_root_observation_is_total_monotonic_and_idempotent \
    --harness session_root_receipt_selects_candidate_only_for_applied_revision \
    --harness environment_owner_topology_is_exact_and_frozen \
    --harness work_projection_is_derived_only_from_durable_session_truth \
    --harness only_an_applied_awaited_create_may_enter_post_create_effects \
    --harness create_replay_conflict_and_terminal_occupation_are_exact_and_closed \
    --harness nonterminal_control_failures_only_replay_the_durable_create_root \
    --harness session_recovery_backoff_is_bounded_monotonic_and_saturating \
    --harness terminal_session_never_enters_ordinary_resource_reconciliation \
    --harness dependency_wake_never_postpones_the_single_existing_deadline
  run_kani awaken-tenancy \
    --harness successful_scope_resolution_never_widens_authority \
    --harness any_uncovered_selector_fails_closed \
    --harness selector_order_cannot_change_an_authorized_result
  run_kani awaken-provisioning-contract \
    --harness credential_expiry_never_exceeds_lease_or_own_ttl \
    --harness revoked_or_expired_lease_always_denies_egress \
    --harness lease_loss_obeys_fixed_fail_closed_fence_priority \
    --harness valid_lease_timing_always_has_three_renewal_opportunities \
    --harness valid_recovery_grace_covers_lease_and_reconciliation \
    --harness sandbox_admission_never_weakens_the_isolation_floor \
    --harness sandbox_admission_requires_every_requested_capability \
    --harness fail_closed_sandbox_policy_never_authorizes_a_downgrade
  run_kani awaken-deployment-contract \
    --harness minute_timestamp_projection_is_exact_and_bounded \
    --harness minute_timestamp_projection_has_an_exact_input_domain
  run_kani awaken-data-subject-application \
    --harness any_withdrawal_vetoes_full_content_capture \
    --harness consent_upsert_leaves_exactly_one_row_for_the_incoming_purpose \
    --harness erasure_withdrawal_is_absorbing_and_idempotent \
    --harness revision_advance_is_strict_or_explicitly_exhausted
  run_kani awaken-credential-vault \
    --harness credential_material_phase_transitions_preserve_exact_writer_fence \
    --harness credential_material_recovery_action_is_total_and_expiry_exact \
    --harness credential_material_takeover_preserves_attempt_and_cannot_restore_original_writer \
    --harness credential_cleanup_requires_exact_authority_and_phase_truth \
    --harness credential_reclamation_never_deletes_shared_or_live_predecessor_material \
    --harness injection_location_represents_exactly_the_three_usable_states \
    --harness injection_location_partial_update_is_exact_and_never_allows_both_disabled \
    --harness provider_scope_never_widens_endpoint_scope \
    --harness disabled_credential_pool_members_are_never_eligible \
    --harness credential_cooldown_boundary_is_exact_and_inclusive \
    --harness exhausted_credentials_are_unavailable_at_every_time \
    --harness a_pool_with_no_enabled_available_member_fails_closed \
    --harness managed_vault_workspace_admission_is_exact_and_non_widening \
    --harness managed_vault_replacement_requires_exact_revision_successor \
    --harness managed_vault_delete_is_monotonic_identity_bound_and_absorbing \
    --harness managed_vault_delete_fence_allows_only_absorbing_child_delete \
    --harness managed_credential_insert_checks_actual_workspace_and_parent \
    --harness managed_credential_insert_requires_every_aggregate_invariant \
    --harness managed_credential_replacement_requires_both_fences_and_exact_successor \
    --harness deleted_managed_credential_is_absorbing \
    --harness managed_creation_pair_requires_every_binding_axis \
    --harness managed_creation_begin_rejects_every_published_identity \
    --harness managed_mutation_operation_shape_is_closed_and_delete_is_absorbing \
    --harness credential_material_attempt_requires_owner_and_exact_physical_namespace \
    --harness credential_inventory_never_orphans_committed_pending_or_foreign_material \
    --harness source_mutation_publication_is_exact_revision_cas \
    --harness managed_rollout_projection_preserves_committed_identity \
    --harness managed_rollout_ack_requires_converged_progress \
    --harness managed_rollout_delivery_acknowledges_only_exact_convergence
  # Two-slot material collections do not need the string comparison's loop
  # budget. Keep every unwinding assertion; only builtin memcmp gets 16.
  # The owned predecessor-set iterator uses the bundled MiniSAT backend.
  run_kani awaken-credential-vault -Z unstable-options --solver minisat \
    --harness credential_material_input_facts_without_predecessor \
    --cbmc-args --unwindset memcmp.0:16
  run_kani awaken-tool-relay \
    --harness hand_channel_only_exact_decoded_reply_restores_readiness \
    --harness hand_protocol_requires_supported_version_and_v2_operation_identity \
    --harness hand_result_disposition_allows_only_explicit_success \
    --harness ledger_completion_requires_the_exact_executing_operation_entry \
    --harness ledger_begin_allows_claim_only_from_absent_and_never_from_replay \
    --harness durable_hand_identity_requires_the_runtime_operation_token
  run_kani awaken-mcp-server-core \
    --harness sessionless_http_lifecycle_routes_each_method_to_its_exact_action
  run_kani awaken-config-resolver \
    --harness brokered_and_direct_model_readiness_require_their_exact_access_evidence \
    --harness agent_input_revision_decision_is_exact_non_wrapping_and_replay_first
  run_kani awaken-work-store \
    --harness work_lease_authority_transition_is_complete_and_fenced \
    --harness work_lease_epoch_is_exact_nonnegative_and_nonwrapping \
    --harness work_reclaim_cutoff_is_exact_and_underflow_free \
    --harness work_lease_deadlines_clamp_without_wrapping
  run_kani awaken-store-postgres \
    --harness dense_commit_sequence_is_exact_nonnegative_and_nonwrapping
  run_kani awaken-store-fs \
    --harness recovery_replay_preserves_complete_entry_and_time
  run_kani awaken-agent-config \
    --harness management_audit_admission_requires_exact_intent_and_closed_state \
    --harness management_effect_confirmation_requires_positive_exact_readback \
    --harness management_effect_enqueue_is_exact_replay_or_absent_insert \
    --harness processing_geography_requires_exact_evidence_from_every_candidate \
    --harness publication_revision_decision_is_target_safe_fail_closed_and_replay_first
  run_kani awaken-config-store \
    --harness management_effect_retirement_requires_present_matching_receipt
  run_kani awaken-credential-contract --solver minisat \
    --harness managed_rollout_identity_requires_every_payload_field
  run_kani awaken-credential-contract \
    --harness credential_envelope_issuance_accepts_exactly_the_complete_claim \
    --harness credential_profile_admission_requires_one_complete_material_tuple \
    --harness durable_credential_material_source_has_no_inline_plaintext_class \
    --harness environment_credential_custody_selects_exactly_one_authorized_profile \
    --harness named_material_shape_is_exact_and_non_widening \
    --harness mcp_delivery_selection_has_only_the_explicit_pair_classes \
    --harness mcp_delivery_receipt_requires_exact_binding_holder_and_mechanism \
    --harness worker_reference_revalidation_accepts_only_exact_available_state \
    --harness worker_plaintext_holder_projection_is_exact_and_non_widening
  run_kani awaken-run-executor-acp \
    --harness host_observation_projection_is_exactly_backend_scoped \
    --harness local_binding_batch_publication_is_complete_or_stutters \
    --harness projected_binding_use_rechecks_the_exact_projection_after_probe \
    --harness mcp_client_credential_admission_has_no_gateway_or_adapter_fallback
  run_kani awaken-dispatch-contract \
    --harness claim_credential_operation_is_total_and_terminal_dominates \
    --harness claim_credential_binding_revalidation_rejects_missing_widened_reordered_or_stale_wire \
    --harness only_awaiting_or_already_superseded_is_replacement_safe \
    --harness stale_dispatch_claim_cannot_modify_authoritative_state \
    --harness exact_dispatch_settlement_is_terminal_or_awaiting_only \
    --harness dispatch_cancel_revokes_old_epoch_and_is_idempotent \
    --harness dispatch_claim_mints_exactly_one_epoch_and_never_reopens_closed_rows \
    --harness dispatch_maintenance_transitions_are_closed_and_exact \
    --harness commit_epoch_completion_requires_observed_release \
    --harness commit_epoch_guard_admits_only_complete_exact_claim \
    --harness pre_start_no_effect_ack_requires_fresh_install_admission \
    --harness lease_renewal_observation_preserves_claim_and_attempt_fences \
    --harness physical_attempt_slot_installs_and_clears_only_exact_authority \
    --harness physical_binding_cannot_change_on_admission_retry \
    --harness retired_epoch_cannot_admit_another_execution \
    --harness thread_physical_peer_vetoes_begin_and_handoff \
    --harness physical_attempt_slot_never_normalizes_a_torn_owner_epoch_pair
  run_kani awaken-dispatch --no-default-features \
    --harness agent_message_completed_tombstone_replays_only_exact_dispatch_identity \
    --harness maintenance_schedule_exists_only_for_the_owning_adapter \
    --harness recovery_snapshot_admission_requires_exact_claim_identity \
    --harness acknowledged_projection_is_exact_monotonic_or_a_noop \
    --harness claimed_snapshot_execution_requires_exact_identity \
    --harness pending_signal_is_deliverable_only_without_consuming_commit \
    --harness dispatch_claim_preserves_every_frozen_binding \
    --harness shared_error_gate_admits_exactly_one_probe_after_the_deadline \
    --harness drain_error_backoff_is_bounded_monotonic_and_saturating \
    --harness store_deadline_is_monotonic_saturating_and_never_wraps \
    --harness terminal_scan_page_preserves_budget_cursor_and_cancellation
  run_kani awaken-credential-materializer \
    --harness exact_vault_revision_accepts_only_a_positive_matching_or_unpinned_source \
    --harness platform_relay_local_gate_admits_only_exact_platform_relay \
    --harness new_materialization_admits_only_active_credential_status
  run_kani awaken-executable-agent-contract \
    --harness requested_agent_profile_accepts_only_the_same_positive_revision \
    --harness executable_snapshot_pin_is_complete_exact_and_non_mixing \
    --harness every_duplicated_snapshot_pin_axis_is_binding
  run_kani awaken-file-store --features object-store \
    --harness object_store_configuration_accepts_exactly_the_provider_compatible_shape
  run_kani awaken-protocol-acp --features real-acp \
    --harness acp_read_ahead_admission_never_overflows_or_exceeds_either_bound \
    --harness permission_consensus_is_a_conservative_order_independent_semilattice \
    --harness acp_permission_projection_is_total_exact_and_non_widening \
    --harness acp_session_load_preserves_the_exact_mcp_projection \
    --harness acp_permission_tool_identity_normalization_is_exact
  # Z3 keeps the full-width division shared in this caller-refinement proof.
  # This selection remains inside the one strict runner and toolchain receipt.
  if command -v z3 >/dev/null 2>&1; then
    actual_z3_version="$(z3 --version)"
    if [ "$actual_z3_version" != "$z3_version" ]; then
      echo "unexpected Z3 version: $actual_z3_version" >&2
      exit 1
    fi
    run_kani awaken-protocol-managed --solver z3 \
      --harness memory_time_filter_compares_visible_milliseconds_without_merging_them
  else
    echo "skipped Z3: install the pinned solver on PATH (TLAPS bundles it)"
    missing=1
  fi
  # The reply selector visits at most three lifecycle facts; comparing the
  # exact retryable error code needs a separate builtin memcmp budget.
  run_kani awaken-protocol-managed -Z unstable-options --solver kissat \
    --harness managed_reply_anchor_selects_exact_fenced_commit \
    --cbmc-args --unwindset memcmp.0:16
  run_kani awaken-protocol-managed \
    --harness projection_publish_decision_is_total_exact_and_fail_closed \
    --harness runtime_tool_projection_phase_is_total_distinct_and_fail_closed \
    --harness projection_refresh_failure_stutters_and_success_preserves_one_pair \
    --harness projection_source_stamp_is_exact_for_the_complete_observed_thread_prefix \
    --harness canonical_event_order_never_depends_on_wall_time \
    --harness historical_pending_order_precedes_every_admitted_terminal \
    --solver kissat \
    --harness organization_bucket_refill_is_exact_and_bounded \
    --harness organization_bucket_saturated_refill_is_canonical_and_bounded \
    --harness organization_bucket_refill_clamp_preserves_capacity_invariant \
    --harness organization_bucket_consumption_is_exact_and_non_over_admitting \
    --harness deployment_run_failure_projection_is_total_exact_and_non_strengthening \
    --harness missing_agent_publication_is_available_only_to_noninteractive_projection \
    --harness inbound_admission_receipt_is_stable_across_processing_races \
    --harness inbound_order_is_fixed_before_effect_observation \
    --harness rollout_update_revision_is_monotonic_and_covers_event \
    --harness user_profile_capability_and_field_projection_is_total_exclusive_and_exact \
    --harness idempotency_key_scan_transition_is_exact_and_invalid_absorbing \
    --harness idempotency_key_length_and_summary_policy_is_exact \
    --harness idempotency_key_admission_rejects_empty_overlong_and_every_invalid_byte
  run_kani awaken-protocol-a2a \
    --harness a2a_task_state_projection_is_total_exact_and_non_strengthening
  run_kani awaken-protocol-ag-ui \
    --harness ag_ui_terminal_category_mapping_is_total_and_exact
  run_kani awaken-protocol-ai-sdk \
    --harness ai_sdk_terminal_category_mapping_is_total_and_exact
  run_kani awaken-provider-genai \
    --harness reasoning_replay_projection_preserves_reasoning_and_complete_responses \
    --harness reasoning_fold_prepends_exactly_once_and_is_transport_independent \
    --harness credential_probe_tri_state_is_total_and_fail_safe \
    --harness unclassified_provider_stop_reason_fails_closed
  run_kani awaken-ext-mcp \
    --harness sensitive_marker_never_projects_payload_publicly \
    --harness sensitivity_markers_can_never_widen_a_redacted_projection \
    --harness notification_admission_is_exact_and_unknown_fails_closed \
    --harness mcp_call_admission_requires_both_accepting_observations
  run_kani awaken-sandbox-container \
    --harness continuation_writable_roots_share_one_claim_without_aliasing \
    --harness allowlist_claim_never_exceeds_its_evidence
  run_kani awaken-sandbox-container --features k8s --solver kissat \
    --harness rebuild_activation_requires_exact_continuation_and_effect \
    --harness continuation_claim_selection_is_total_exact_and_non_widening \
    --harness disabled_continuation_has_no_claim_api_obligation_without_durable_evidence \
    --harness continuation_claim_deletion_requires_exact_uid_and_resource_version \
    --harness absent_pod_claim_admission_is_total_and_never_starts_deletion \
    --harness absent_pod_observation_never_confuses_recovery_with_completed_cleanup
  run_kani awaken-store-schema \
    --harness dense_migration_versions_are_strictly_increasing \
    --harness migration_step_never_rolls_back_or_skips_a_version \
    --harness replaying_a_fully_applied_migration_plan_is_a_noop
  run_kani awaken-ext-compact \
    --harness fold_point_preserves_the_requested_suffix \
    --harness fold_point_is_present_exactly_when_triggered_with_nonempty_prefix \
    --harness compaction_artifact_requires_exact_identity_coverage_and_content
  run_kani awaken-ext-memory \
    --harness memory_recall_contribution_is_exact_and_disabled_is_inert
  run_kani awaken-ext-goal \
    --harness applying_a_grade_obeys_decision_and_budget \
    --harness terminal_outcomes_are_absorbing
  run_kani awaken-ext-background-task \
    --harness background_execution_admission_is_exact_fenced_live_and_phase_monotone \
    --harness cancellation_is_monotone_and_terminal_states_are_absorbing \
    --harness never_replay_recovery_requires_a_durable_remote_continuation \
    --harness expired_background_recovery_is_exact_nonwrapping_and_never_invents_termination
  run_kani awaken-runtime \
    --harness circuit_admission_is_capacity_bounded_nonwrapping_and_rejection_stutters \
    --harness circuit_completion_requires_exact_phase_and_generation_for_every_outcome
  run_kani awaken-runtime-contract --solver kissat \
    --harness created_tool_handle_validation_never_invents_termination
  run_kani awaken-runtime-contract \
    --harness credential_realization_receipt_requires_every_exact_binding_coordinate \
    --harness model_operation_ordinal_never_wraps_or_reuses_a_coordinate \
    --harness model_execution_reentry_requires_positive_evidence \
    --harness claim_bound_external_effect_admits_only_current_ownership \
    --harness committed_terminal_projection_admits_only_exact_ended_identity \
    --harness max_token_continuation_requires_every_exact_guard \
    --harness anchored_context_window_never_moves_past_the_covered_prefix \
    --harness unanchored_legacy_context_never_hides_transcript_messages \
    --harness terminal_calls_are_never_reentered \
    --harness only_the_matching_approval_ticket_enters_execution \
    --harness every_tool_call_transition_has_the_unique_documented_precondition \
    --harness terminal_tool_calls_only_accept_result_staging \
    --harness run_end_sealing_targets_exactly_nonterminal_calls \
    --harness a_second_wait_is_never_admitted \
    --harness child_result_is_consumed_only_from_ready \
    --harness terminal_delivery_phases_never_reopen \
    --harness rejected_live_inbox_reorder_is_an_atomic_stutter \
    --harness live_inbox_identity_advances_strictly_or_exhausts \
    --harness advisor_never_substitutes_for_primary_model_admission \
    --harness fallback_selection_preserves_the_complete_publication_pin \
    --harness every_route_pin_axis_participates_in_exact_identity \
    --harness transcript_prefix_is_exact_request_only_context_not_durable_truth \
    --harness legacy_derived_context_remains_durable_but_never_reaches_a_new_model_request \
    --harness capture_meet_is_exact_commutative_and_non_widening \
    --harness capture_projection_admits_content_only_at_full \
    --harness non_full_capture_fails_closed_before_redaction \
    --harness tool_capability_intersection_never_widens_configured_authority \
    --harness permission_verdict_projects_to_exact_non_widening_gate_outcome \
    --harness every_gate_outcome_has_one_exact_audit_label \
    --harness retry_is_authorized_only_inside_the_exact_bounded_budget \
    --harness every_u8_is_admitted_exactly_when_it_is_within_the_search_limit \
    --harness same_resource_conflict_is_symmetric_and_exactly_one_write_or_more \
    --harness tool_wave_extension_requires_every_exact_guard \
    --harness tool_wave_extension_never_crosses_the_positive_bound_or_admits_serial \
    --harness non_retryable_failure_is_immediately_terminal \
    --harness terminal_retry_state_never_reopens \
    --harness unknown_retry_state_fails_closed \
    --harness plugin_activation_is_exactly_the_requested_identity \
    --harness plugin_activation_requires_every_declared_dependency \
    --harness plugin_activation_never_widens_the_capability_bound \
    --harness plugin_activation_never_admits_a_duplicate_identity
  run_kani awaken-ext-permission \
    --harness unmatched_permission_mode_is_exact_and_plan_fails_closed \
    --harness matched_deny_is_absolute_and_only_stricter_non_deny_replaces_authority
  run_kani awaken-ext-state-machine \
    --harness tool_and_event_transitions_never_cross_trigger_kinds \
    --harness state_transition_requires_every_exact_precondition \
    --harness result_transition_and_authored_fallback_are_exact \
    --harness terminal_state_classification_is_exact \
    --harness unknown_transition_trigger_and_result_fail_closed
  run_kani awaken-session-executor \
    --harness session_agent_message_admission_requires_every_committed_field \
    --harness session_realization_permit_admits_only_a_current_acquired_slot \
    --harness session_realization_control_permit_ownership_release_is_total_and_exact \
    --harness acp_mcp_projection_has_one_exact_authority \
    --harness acp_tool_export_requires_claim_execution_environment \
    --harness coordination_envelope_never_satisfies_execution_cache_admission \
    --harness runtime_environment_admission_is_total_and_coordination_never_borrows \
    --harness resume_receipt_classification_is_exact_and_conflict_absorbing \
    --harness file_read_authority_never_changes_path_class_or_admits_unsafe_input \
    --harness hand_availability_has_only_explicit_recovery_and_terminal_transitions \
    --harness hand_binding_can_only_become_ready_through_tracked_starting \
    --harness hand_generation_advance_never_wraps_and_exhaustion_is_terminal \
    --harness read_only_tree_publication_requires_a_complete_restricted_safe_stage \
    --harness session_realization_control_disposition_projects_exact_worker_effect \
    --harness trace_capture_clamp_is_exact_and_never_widens_persisted_content \
    --harness configured_capture_redactor_selection_is_total_and_exact \
    --harness session_resource_replacement_requires_newer_or_authority_amended_generation \
    --harness explicit_empty_resource_manifest_revokes_without_activation \
    --harness dispatched_resource_manifest_requires_exact_nonempty_scope \
    --harness session_realization_renewal_failure_disposition_is_total_exact_and_fail_closed \
    --harness container_hand_residency_recovery_mapping_is_total_exact_and_non_widening \
    --harness mcp_credential_realization_preserves_the_request_target_exactly \
    --harness durable_dispatch_admission_is_exact_and_commit_store_independent \
    --harness durable_dispatch_admission_is_monotonic_in_persistence_evidence \
    --harness sandbox_support_projection_is_total_exact_and_evidence_bound \
    --harness projection_revision_admission_is_monotonic_and_exact \
    --harness durable_projection_identity_is_exact_or_a_same_physical_owner_stutter \
    --harness committed_candidate_retains_effect_kind_and_rejects_identity_relabeling \
    --harness closed_environment_recovery_consumes_only_rebuildable_owner_classes \
    --harness terminal_quiescence_rejects_only_unstarted_reservations \
    --harness unsettled_running_is_always_rejected_and_settled_kinds_never_interchange \
    --harness settled_step_projection_cannot_invent_pending_or_observations
  run_kani awaken-config-service \
    --harness only_the_reserved_scope_selects_admin_catalog_membership \
    --harness every_non_reserved_scope_selects_strictly_global_membership
  run_kani awaken-service-lifecycle \
    --harness startup_wiring_is_exact_for_every_service_role \
    --harness startup_wiring_requires_every_role_owned_component \
    --harness startup_wiring_fails_closed_for_unknown_missing_or_extra_authority \
    --harness process_role_projects_exact_startup_authority \
    --harness worker_never_acquires_service_startup_or_local_mount_authority \
    --harness local_managed_mount_is_exactly_the_local_worker_component
  run_kani awaken-resource-contract \
    --harness artifact_publication_receipt_requires_every_identity_axis \
    --harness resource_config_publication_is_exact_and_never_wraps \
    --harness exhausted_resource_config_versions_fail_closed \
    --harness portable_aggregate_revision_is_strictly_monotonic_and_never_wraps \
    --harness resource_lifecycle_timestamps_project_the_exact_transition \
    --harness live_resource_binding_requires_exact_scope_active_state_and_existing_pin \
    --harness stale_resource_purge_claim_cannot_commit_receipt
  run_kani awaken-resource-store \
    --harness reclamation_fence_admission_requires_absent_owner_and_zero_references
  run_kani awaken-resource-reclaimer \
    --harness physical_purge_requires_zero_references_leases_and_fence
  run_kani awaken-mcp-server-core \
    --harness one_request_has_at_most_one_final_response \
    --harness notifications_never_have_a_jsonrpc_response \
    --harness final_response_is_progress_absorbing \
    --harness rejected_requests_never_enter_the_host \
    --harness cancellation_never_produces_a_success_result
  run_kani awaken-run-executor-a2a \
    --harness remote_operation_sequence_admits_only_prepared_before_send_and_accept_commit \
    --harness remote_operation_route_is_total_and_fail_closed \
    --harness terminal_remote_operation_command_is_exact_run_scoped_removal \
    --harness task_reference_admission_requires_complete_fields_and_required_identity_matches \
    --harness a2a_message_identity_rejects_only_resume_without_committed_correlation
  run_kani awaken-worker-contract \
    --harness accepted_version_is_inside_worker_range \
    --harness non_ready_worker_never_accepts_work \
    --harness placement_extension_admits_only_allowed_unique_identity \
    --harness never_replace_rejects_every_replacement \
    --harness sandbox_continuity_authorizes_replacement_exactly_when_bound \
    --harness same_incarnation_never_spends_replacement_authority \
    --harness required_worker_capability_cannot_be_omitted \
    --harness sandbox_tool_recovery_claim_axis_is_exact_and_non_widening \
    --harness manifest_recovery_mapping_accepts_only_the_exact_installed_capability \
    --harness dynamic_evidence_can_only_restrict_ready_worker_admission \
    --harness process_readiness_after_startup_is_probe_independent \
    --harness worker_replacement_and_heartbeat_share_one_non_resurrection_boundary \
    --harness changed_worker_observation_always_mints_a_strictly_fresh_fence \
    --harness observation_publication_fence_is_success_owned_exact_and_failure_stable \
    --harness worker_dynamic_observation_requires_exact_fact_and_half_open_lease
  run_kani awaken-worker \
    --harness environment_evidence_decision_never_refreshes_expired_proof \
    --harness observation_batch_is_single_generation_and_never_wraps \
    --harness observation_publication_budget_is_strict_and_overflow_safe \
    --harness worker_realization_scope_source_is_total_and_exact
  run_kani awaken-worker-transport-security \
    --harness worker_transport_selector_admits_only_supported_exact_postures \
    --harness remote_transport_never_downgrades_or_widens_identity
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
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/webhook-outbox" \
    -config formal/tla/WebhookOutbox.cfg formal/tla/WebhookOutbox.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/registration-intent" \
    -config formal/tla/RegistrationIntent.cfg formal/tla/RegistrationIntent.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/erasure-saga" \
    -config formal/tla/ErasureSaga.cfg formal/tla/ErasureSaga.tla
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
