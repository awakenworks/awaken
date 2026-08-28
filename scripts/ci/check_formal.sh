#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"

# Prefer the repository-pinned source build when bootstrapped. The published
# Kani 0.67 bundle embeds Rust 1.93 and cannot compile the workspace's Rust 1.96
# IAM dependencies. Its setup still owns the CBMC/GOTO backend binaries.
kani_cache_root="${AWAKEN_KANI_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/awaken-kani}"
kani_source_dir="${AWAKEN_KANI_SOURCE_DIR:-$kani_cache_root/current}"
if [ -x "$kani_source_dir/scripts/cargo-kani" ]; then
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

missing=0
formal_tmp_root="$(mktemp -d)"
trap 'rm -rf "$formal_tmp_root"' EXIT
rust_trace_dir="$formal_tmp_root/rust-traces"
rendered_trace_dir="$formal_tmp_root/rendered-traces"

python3 scripts/ci/check_feature_coverage.py --require-complete
python3 scripts/ci/check_formal_coverage.py --require-complete
python3 scripts/ci/check_formal_surface.py --require-complete
python3 scripts/ci/check_formal_web_surface.py
python3 scripts/ci/check_proof_boundaries.py
python3 scripts/ci/check_formal_mutations.py

# Explore the production in-memory worker registry's lock interleavings. This
# feature swaps only its mutex for Loom's instrumented mutex; the transition
# kernel and directory methods remain the production code.
cargo test -p awaken-worker-registry --features loom --lib loom_tests
# Explore the production reference WorkQueue's owner/epoch/expiry snapshot.
# Reclaim, release and observation share the same implementation used outside
# the Loom proof build; only the mutex implementation is substituted.
cargo test -p awaken-work-store --features loom --lib lease_book::loom_tests

AWAKEN_FORMAL_TRACE_DIR="$rust_trace_dir" \
  cargo test -p awaken-runtime --test formal_refinement
python3 scripts/ci/render_runtime_refinement_traces.py \
  "$rust_trace_dir" "$rendered_trace_dir" \
  --require-complete-transition-coverage

if command -v cargo-kani >/dev/null 2>&1; then
  # Kani compiles every selected package once and verifies its named harnesses
  # with a bounded worker pool. The old one-process-per-harness path rebuilt the
  # same package 54 times. Two jobs is the conservative default because CBMC is
  # memory-heavy. A single job is also the reproducible default: Kani 0.67 can
  # race while linking cached per-harness GOTO artifacts from one package.
  # CI may raise AWAKEN_KANI_JOBS only after proving its toolchain/cache layout
  # does not share those output paths.
  kani_jobs="${AWAKEN_KANI_JOBS:-1}"
  run_kani() {
    local package="$1"; shift
    cargo kani -p "$package" --output-format terse -j "$kani_jobs" "$@"
  }
  run_kani awaken-agent-contract \
    --harness ended_is_absorbing_for_every_next_state \
    --harness legacy_wire_accepts_exactly_the_legal_dispositions \
    --harness run_identity_binding_is_exactly_run_scoped \
    --harness exclusive_state_conflict_requires_every_exact_precondition \
    --harness state_materialization_effect_is_total_and_exact \
    --harness only_unsettled_relationships_occupy_a_parallel_slot \
    --harness every_relationship_effect_has_one_documented_precondition \
    --harness delegation_admission_requires_every_budget_and_lineage_guard \
    --harness cancellation_delivery_is_enabled_only_by_durable_intent \
    --harness tool_policy_selector_fails_closed_for_unregistered_widened_or_mismatched_calls \
    --harness stream_terminal_projection_is_exact_fail_closed_and_absorbing
  run_kani awaken-authorization-contract \
    --harness run_backed_route_policy_uses_exact_run_actions \
    --harness application_route_policy_never_enters_the_service_guard \
    --harness credential_ingress_role_contains_only_workspace_apikey_authority \
    --harness agent_publisher_role_contains_only_workspace_model_read_and_skill_authority \
    --harness hosted_admin_role_contains_exactly_its_five_workspace_authorities \
    --harness hosted_builder_role_has_writes_without_apikey_or_model_administration \
    --harness workspace_member_role_contains_exactly_read_only_workspace_authorities \
    --harness runtime_member_role_contains_exactly_run_read \
    --harness legacy_workspace_binding_migration_is_idempotent_and_authority_exact
  run_kani awaken-acp-contract \
    --harness acp_capability_is_detected_exactly_after_a_verified_observation
  run_kani awaken-session-contract \
    --harness session_realization_control_failure_disposition_is_total_exact_and_fail_closed \
    --harness session_model_override_reuses_only_the_same_identity_and_resolves_every_mismatch \
    --harness awaiting_constructor_cannot_create_a_terminal_or_failed_outcome \
    --harness ended_constructor_carries_the_only_failure_authority_and_no_pending_tool \
    --harness settled_step_has_no_parallel_observation_authority \
    --harness only_queued_work_is_claimable \
    --harness only_active_work_accepts_lease_extension \
    --harness stop_is_absorbing_for_every_work_state \
    --harness first_heartbeat_is_authorized_exactly_once \
    --harness matching_heartbeat_rejects_every_other_receipt \
    --harness resolved_environment_snapshot_accepts_only_exact_positive_identity_and_revision \
    --harness stopped_session_work_is_revived_only_by_a_claimed_nonterminal_run \
    --harness accepted_managed_budget_cost_never_wraps \
    --harness source_disposal_requires_ready_phase_and_checkpoint \
    --harness terminal_execution_never_reopens \
    --harness realization_renewal_never_widens_owner_epoch_or_expiry_authority \
    --harness runtime_intervals_open_once_and_never_close_before_start \
    --harness bounded_runtime_capability_iterator_uses_exact_member_projection \
    --harness runtime_capability_member_projection_is_total_exact_and_bounded \
    --harness skill_execution_pin_requires_exact_workspace_revision_and_hash \
    --harness every_skill_execution_pin_axis_is_binding \
    --harness frozen_worker_agent_publication_is_exact_or_fails_closed \
    --harness terminal_dream_statuses_are_absorbing \
    --harness dream_success_and_failure_require_a_running_process \
    --harness dream_recovery_and_cancel_never_widen_terminal_authority \
    --harness dream_transition_table_is_total_exact_and_closed \
    --harness quiescence_receipt_requires_exact_operation_epoch_and_zero_live_effects \
    --harness checkpoint_receipt_requires_every_immutable_generation_axis \
    --harness source_disposal_receipt_requires_exact_binding_and_termination \
    --harness restore_receipt_requires_exact_checkpoint_and_nonempty_binding \
    --harness session_cleanup_completion_requires_every_identity_axis \
    --harness session_cleanup_phase_advances_only_not_requested_fenced_requested_completed \
    --harness session_delete_request_plan_is_exact_hidden_terminal_and_idempotent \
    --harness session_tombstone_requires_hidden_disposition_terminal_execution_and_verified_cleanup
  run_kani awaken-service-auth-contract \
    --harness service_token_retry_is_enabled_only_for_an_exact_changed_token
  run_kani awaken-session-application \
    --harness environment_owner_topology_is_exact_and_frozen
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
    --harness managed_credential_workspace_admission_requires_exact_parent_and_owner \
    --harness managed_credential_insert_requires_every_aggregate_invariant \
    --harness managed_credential_replacement_requires_both_fences_and_exact_successor \
    --harness deleted_managed_credential_is_absorbing \
    --harness managed_creation_pair_requires_every_binding_axis \
    --harness managed_creation_begin_rejects_every_published_identity \
    --harness managed_mutation_operation_shape_is_closed_and_delete_is_absorbing \
    --harness credential_material_attempt_requires_owner_and_exact_physical_namespace \
    --harness managed_rollout_never_precedes_exact_pair_publication \
    --harness managed_rollout_ack_requires_converged_progress
  run_kani awaken-tool-relay \
    --harness hand_channel_only_exact_decoded_reply_restores_readiness \
    --harness hand_protocol_requires_supported_version_and_v2_operation_identity
  run_kani awaken-config-resolver \
    --harness brokered_and_direct_model_readiness_require_their_exact_access_evidence
  run_kani awaken-agent-config \
    --harness processing_geography_requires_exact_evidence_from_every_candidate \
    --harness publication_revision_decision_is_target_safe_fail_closed_and_replay_first
  run_kani awaken-credential-contract \
    --harness credential_envelope_issuance_accepts_exactly_the_complete_claim \
    --harness environment_credential_custody_selects_exactly_one_authorized_profile \
    --harness named_material_shape_is_exact_and_non_widening \
    --harness mcp_delivery_selection_has_only_the_explicit_pair_classes \
    --harness mcp_delivery_receipt_requires_exact_binding_holder_and_mechanism \
    --harness worker_plaintext_holder_projection_is_exact_and_non_widening
  run_kani awaken-run-executor-acp \
    --harness mcp_client_credential_admission_has_no_gateway_or_adapter_fallback
  run_kani awaken-run-ingress-contract \
    --harness session_resource_replacement_requires_exactly_a_newer_same_workspace_generation \
    --harness only_awaiting_or_already_superseded_is_replacement_safe \
    --harness stale_dispatch_claim_cannot_modify_authoritative_state \
    --harness exact_dispatch_settlement_is_terminal_or_awaiting_only \
    --harness dispatch_cancel_revokes_old_epoch_and_is_idempotent \
    --harness dispatch_claim_mints_exactly_one_epoch_and_never_reopens_closed_rows \
    --harness dispatch_maintenance_transitions_are_closed_and_exact
  run_kani awaken-credential-materializer \
    --harness exact_vault_revision_accepts_only_a_positive_matching_or_unpinned_source \
    --harness platform_relay_local_gate_admits_only_exact_platform_relay
  run_kani awaken-executable-agent-contract \
    --harness requested_agent_profile_accepts_only_the_same_positive_revision \
    --harness executable_snapshot_pin_is_complete_exact_and_non_mixing \
    --harness every_duplicated_snapshot_pin_axis_is_binding
  run_kani awaken-file-store --features object-store \
    --harness object_store_configuration_accepts_exactly_the_provider_compatible_shape
  run_kani awaken-protocol-acp --features real-acp \
    --harness permission_consensus_is_a_conservative_order_independent_semilattice \
    --harness acp_permission_projection_is_total_exact_and_non_widening \
    --harness acp_session_load_preserves_the_exact_mcp_projection \
    --harness acp_permission_tool_identity_normalization_is_exact
  run_kani awaken-protocol-managed \
    --solver kissat \
    --harness organization_bucket_refill_is_exact_and_bounded \
    --harness organization_bucket_saturated_refill_is_canonical_and_bounded \
    --harness organization_bucket_refill_clamp_preserves_capacity_invariant \
    --harness organization_bucket_consumption_is_exact_and_non_over_admitting \
    --harness deployment_run_failure_projection_is_total_exact_and_non_strengthening \
    --harness retired_agent_publication_bypass_is_exclusive_to_terminal_cleanup \
    --harness rollout_update_revision_is_monotonic_and_covers_event \
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
    --harness reasoning_fold_prepends_exactly_once_and_is_transport_independent
  run_kani awaken-ext-mcp \
    --harness sensitive_marker_never_projects_payload_publicly \
    --harness sensitivity_markers_can_never_widen_a_redacted_projection \
    --harness notification_admission_is_exact_and_unknown_fails_closed
  run_kani awaken-sandbox-container \
    --harness continuation_writable_roots_share_one_claim_without_aliasing \
    --harness allowlist_claim_never_exceeds_its_evidence
  run_kani awaken-sandbox-container --features k8s --solver kissat \
    --harness continuation_claim_selection_is_total_exact_and_non_widening \
    --harness continuation_claim_deletion_requires_exact_uid_and_resource_version
  run_kani awaken-store-schema \
    --harness dense_migration_versions_are_strictly_increasing \
    --harness migration_step_never_rolls_back_or_skips_a_version \
    --harness replaying_a_fully_applied_migration_plan_is_a_noop
  run_kani awaken-ext-compact \
    --harness fold_point_preserves_the_requested_suffix \
    --harness fold_point_is_present_exactly_when_triggered_with_nonempty_prefix
  run_kani awaken-ext-memory \
    --harness memory_recall_contribution_is_exact_and_disabled_is_inert
  run_kani awaken-ext-goal \
    --harness applying_a_grade_obeys_decision_and_budget \
    --harness terminal_outcomes_are_absorbing
  run_kani awaken-ext-background-task \
    --harness cancellation_is_monotone_and_terminal_states_are_absorbing
  run_kani awaken-runtime-contract \
    --harness terminal_calls_are_never_reentered \
    --harness only_the_matching_approval_ticket_enters_execution \
    --harness every_tool_call_transition_has_the_unique_documented_precondition \
    --harness terminal_tool_calls_only_accept_result_staging \
    --harness run_end_sealing_targets_exactly_nonterminal_calls \
    --harness child_result_is_consumed_only_from_ready \
    --harness terminal_delivery_phases_never_reopen \
    --harness rejected_live_inbox_reorder_is_an_atomic_stutter \
    --harness live_inbox_identity_advances_strictly_or_exhausts \
    --harness advisor_never_substitutes_for_primary_model_admission \
    --harness fallback_selection_preserves_the_complete_publication_pin \
    --harness every_route_pin_axis_participates_in_exact_identity \
    --harness transcript_prefix_is_exact_request_only_context_not_durable_truth \
    --harness capture_meet_is_exact_commutative_and_non_widening \
    --harness capture_projection_admits_content_only_at_full \
    --harness non_full_capture_fails_closed_before_redaction \
    --harness tool_capability_intersection_never_widens_configured_authority \
    --harness permission_verdict_projects_to_exact_non_widening_gate_outcome \
    --harness every_gate_outcome_has_one_exact_audit_label \
    --harness retry_is_authorized_only_inside_the_exact_bounded_budget \
    --harness every_u8_is_admitted_exactly_when_it_is_within_the_search_limit \
    --harness same_resource_conflict_is_symmetric_and_exactly_one_write_or_more \
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
  run_kani awaken-runtime-host \
    --harness resume_receipt_classification_is_exact_and_conflict_absorbing \
    --harness file_read_authority_never_changes_path_class_or_admits_unsafe_input \
    --harness hand_availability_has_only_explicit_recovery_and_terminal_transitions \
    --harness hand_binding_can_only_become_ready_through_tracked_starting \
    --harness hand_generation_advance_never_wraps_and_exhaustion_is_terminal \
    --harness read_only_tree_publication_requires_a_complete_restricted_safe_stage \
    --harness session_realization_control_disposition_projects_exact_worker_effect \
    --harness trace_capture_clamp_is_exact_and_never_widens_persisted_content \
    --harness configured_capture_redactor_selection_is_total_and_exact \
    --harness session_realization_renewal_failure_disposition_is_total_exact_and_fail_closed \
    --harness container_hand_residency_recovery_mapping_is_total_exact_and_non_widening \
    --harness mcp_credential_realization_preserves_the_request_target_exactly \
    --harness durable_dispatch_admission_is_exact_and_commit_store_independent \
    --harness durable_dispatch_admission_is_monotonic_in_persistence_evidence \
    --harness sandbox_support_projection_is_total_exact_and_evidence_bound \
    --harness unsettled_running_is_always_rejected_and_settled_kinds_never_interchange \
    --harness settled_step_projection_cannot_invent_pending_or_observations
  run_kani awaken-config-service \
    --harness only_the_reserved_scope_selects_admin_catalog_membership \
    --harness every_non_reserved_scope_selects_strictly_global_membership
  run_kani awaken-service-lifecycle \
    --harness startup_wiring_is_exact_for_every_service_role \
    --harness startup_wiring_requires_every_role_owned_component \
    --harness startup_wiring_fails_closed_for_unknown_missing_or_extra_authority
  run_kani awaken-resource-contract \
    --harness artifact_publication_receipt_requires_every_identity_axis \
    --harness resource_config_publication_is_exact_and_never_wraps \
    --harness exhausted_resource_config_versions_fail_closed \
    --harness portable_aggregate_revision_is_strictly_monotonic_and_never_wraps \
    --harness resource_lifecycle_timestamps_project_the_exact_transition
  run_kani awaken-mcp-server-core \
    --harness one_request_has_at_most_one_final_response \
    --harness notifications_never_have_a_jsonrpc_response \
    --harness final_response_is_progress_absorbing \
    --harness rejected_requests_never_enter_the_host \
    --harness cancellation_never_produces_a_success_result
  run_kani awaken-worker-contract \
    --harness accepted_version_is_inside_worker_range \
    --harness non_ready_worker_never_accepts_work \
    --harness never_replace_rejects_every_replacement \
    --harness sandbox_continuity_authorizes_replacement_exactly_when_bound \
    --harness same_incarnation_never_spends_replacement_authority \
    --harness sandbox_tool_recovery_claim_axis_is_exact_and_non_widening \
    --harness manifest_recovery_mapping_accepts_only_the_exact_installed_capability \
    --harness dynamic_evidence_can_only_restrict_ready_worker_admission \
    --harness process_readiness_after_startup_is_probe_independent \
    --harness worker_dynamic_observation_requires_exact_fact_and_half_open_lease
  run_kani awaken-worker-transport-security \
    --harness worker_transport_selector_admits_only_three_exact_postures \
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
  "$tlapm_bin" -I formal/tla formal/tla/RuntimeSystemProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/RuntimeImplementationProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/RustCommitSystemProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/WorkQueueProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/SessionActivityProof.tla
  "$tlapm_bin" -I formal/tla formal/tla/SessionRootProof.tla
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
  tlc_state_root="$formal_tmp_root/tlc-states"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/run-ingress" \
    -config formal/tla/RunIngress.cfg formal/tla/RunIngress.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/thread-state" \
    -config formal/tla/ThreadState.cfg formal/tla/ThreadState.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/work-queue" \
    -config formal/tla/WorkQueue.cfg formal/tla/WorkQueue.tla
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
    -metadir "$tlc_state_root/runtime-system" \
    -config formal/tla/RuntimeSystem.cfg formal/tla/RuntimeSystem.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/runtime-implementation" \
    -config formal/tla/RuntimeImplementation.cfg formal/tla/RuntimeImplementation.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/rust-commit-system" \
    -config formal/tla/RustCommitSystem.cfg formal/tla/RustCommitSystem.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/remote-tool" \
    -config formal/tla/RemoteTool.cfg formal/tla/RemoteTool.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/remote-attempt" \
    -config formal/tla/RemoteAttempt.cfg formal/tla/RemoteAttempt.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/authz-kernel" \
    -config formal/tla/AuthzKernel.cfg formal/tla/AuthzKernel.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-ownership" \
    -config formal/tla/SessionOwnership.cfg formal/tla/SessionOwnership.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-deletion" \
    -config formal/tla/SessionDeletion.cfg formal/tla/SessionDeletion.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/workspace-path-projection" \
    -config formal/tla/WorkspacePathProjection.cfg \
    formal/tla/WorkspacePathProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-runtime-projection" \
    -config formal/tla/SessionRuntimeProjection.cfg \
    formal/tla/SessionRuntimeProjection.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/tool-permission-policy" \
    -config formal/tla/ToolPermissionPolicy.cfg \
    formal/tla/ToolPermissionPolicy.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/circuit-breaker" \
    -config formal/tla/CircuitBreaker.cfg formal/tla/CircuitBreaker.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/config-cas" \
    -config formal/tla/AggregateCAS.cfg formal/tla/AggregateCAS.tla
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
    -metadir "$tlc_state_root/skill-version-pin" \
    -config formal/tla/SkillVersionPin.cfg formal/tla/SkillVersionPin.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/tool-result-protocol" \
    -config formal/tla/ToolResultProtocol.cfg formal/tla/ToolResultProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-drain" \
    -config formal/tla/WorkerDrain.cfg formal/tla/WorkerDrain.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-credential-liveness" \
    -config formal/tla/WorkerCredentialLiveness.cfg \
    formal/tla/WorkerCredentialLiveness.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/audit-commit" \
    -config formal/tla/AuditCommit.cfg formal/tla/AuditCommit.tla
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
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/agent-input-revision" \
    -config formal/tla/AgentInputRevision.cfg formal/tla/AgentInputRevision.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/outcome-lifecycle" \
    -config formal/tla/OutcomeLifecycle.cfg formal/tla/OutcomeLifecycle.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-resource-activation" \
    -config formal/tla/SessionResourceActivation.cfg \
    formal/tla/SessionResourceActivation.tla
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
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-realization-mutex" \
    -config formal/tla/SessionRealizationMutex.cfg \
    formal/tla/SessionRealizationMutex.tla
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
    -workers auto \
    -metadir "$tlc_state_root/session-start-protocol" \
    -config formal/tla/SessionStartProtocol.cfg \
    formal/tla/SessionStartProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/session-start-protocol-reachability" \
    -config formal/tla/SessionStartProtocolReachability.cfg \
    formal/tla/SessionStartProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -workers auto \
    -metadir "$tlc_state_root/remote-worker-protocol" \
    -config formal/tla/RemoteWorkerProtocol.cfg formal/tla/RemoteWorkerProtocol.tla
  for trace_config in "$rendered_trace_dir"/RustTrace*.cfg; do
    trace_module="${trace_config%.cfg}.tla"
    trace_name="$(basename "$trace_module" .tla)"
    java -XX:+UseParallelGC \
      -jar "$tla_jar" \
      -metadir "$tlc_state_root/$trace_name" \
      -config "$trace_config" "$trace_module"
  done
else
  echo "skipped TLC: set TLA2TOOLS_JAR and install Java 11+"
  missing=1
fi

if [ "$require_tools" -eq 1 ] && [ "$missing" -ne 0 ]; then
  echo "formal verification tools are required but unavailable" >&2
  exit 1
fi
