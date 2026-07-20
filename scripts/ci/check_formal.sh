#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

require_tools=0
if [ "${1:-}" = "--require-tools" ]; then
  require_tools=1
fi

missing=0
formal_tmp_root="$(mktemp -d)"
trap 'rm -rf "$formal_tmp_root"' EXIT
rust_trace_dir="$formal_tmp_root/rust-traces"
rendered_trace_dir="$formal_tmp_root/rendered-traces"

python3 scripts/ci/check_formal_coverage.py

# Explore the production in-memory worker registry's lock interleavings. This
# feature swaps only its mutex for Loom's instrumented mutex; the transition
# kernel and directory methods remain the production code.
cargo test -p awaken-worker-registry --features loom --lib loom_tests

AWAKEN_FORMAL_TRACE_DIR="$rust_trace_dir" \
  cargo test -p awaken-runtime --test formal_refinement
python3 scripts/ci/render_runtime_refinement_traces.py \
  "$rust_trace_dir" "$rendered_trace_dir"

if command -v cargo-kani >/dev/null 2>&1; then
  cargo kani -p awaken-agent-contract \
    --harness ended_is_absorbing_for_every_next_state
  cargo kani -p awaken-agent-contract \
    --harness legacy_wire_accepts_exactly_the_legal_dispositions
  cargo kani -p awaken-agent-contract \
    --harness only_unsettled_relationships_occupy_a_parallel_slot
  cargo kani -p awaken-agent-contract \
    --harness every_relationship_effect_has_one_documented_precondition
  cargo kani -p awaken-agent-contract \
    --harness delegation_admission_requires_every_budget_and_lineage_guard
  cargo kani -p awaken-agent-contract \
    --harness cancellation_delivery_is_enabled_only_by_durable_intent
  cargo kani -p awaken-session-contract \
    --harness awaiting_constructor_cannot_create_a_terminal_or_failed_outcome
  cargo kani -p awaken-session-contract \
    --harness ended_constructor_carries_the_only_failure_authority_and_no_pending_tool
  cargo kani -p awaken-session-contract \
    --harness only_queued_work_is_claimable
  cargo kani -p awaken-session-contract \
    --harness only_active_work_accepts_lease_extension
  cargo kani -p awaken-session-contract \
    --harness stop_is_absorbing_for_every_work_state
  cargo kani -p awaken-session-contract \
    --harness first_heartbeat_is_authorized_exactly_once
  cargo kani -p awaken-session-contract \
    --harness matching_heartbeat_rejects_every_other_receipt
  cargo kani -p awaken-tenancy \
    --harness successful_scope_resolution_never_widens_authority
  cargo kani -p awaken-tenancy \
    --harness any_uncovered_selector_fails_closed
  cargo kani -p awaken-tenancy \
    --harness selector_order_cannot_change_an_authorized_result
  cargo kani -p awaken-provisioning-contract \
    --harness credential_expiry_never_exceeds_lease_or_own_ttl
  cargo kani -p awaken-provisioning-contract \
    --harness revoked_or_expired_lease_always_denies_egress
  cargo kani -p awaken-provisioning-contract \
    --harness reap_reason_obeys_fixed_fail_closed_priority
  cargo kani -p awaken-provisioning-contract \
    --harness sandbox_admission_never_weakens_the_isolation_floor
  cargo kani -p awaken-provisioning-contract \
    --harness sandbox_admission_requires_every_requested_capability
  cargo kani -p awaken-provisioning-contract \
    --harness fail_closed_sandbox_policy_never_authorizes_a_downgrade
  cargo kani -p awaken-data-subject \
    --harness any_withdrawal_vetoes_full_content_capture
  cargo kani -p awaken-data-subject \
    --harness consent_upsert_leaves_exactly_one_row_for_the_incoming_purpose
  cargo kani -p awaken-data-subject \
    --harness erasure_withdrawal_is_absorbing_and_idempotent
  cargo kani -p awaken-credential-vault \
    --harness disabled_credential_pool_members_are_never_eligible
  cargo kani -p awaken-credential-vault \
    --harness credential_cooldown_boundary_is_exact_and_inclusive
  cargo kani -p awaken-credential-vault \
    --harness exhausted_credentials_are_unavailable_at_every_time
  cargo kani -p awaken-credential-vault \
    --harness a_pool_with_no_enabled_available_member_fails_closed
  cargo kani -p awaken-store-schema \
    --harness dense_migration_versions_are_strictly_increasing
  cargo kani -p awaken-store-schema \
    --harness migration_step_never_rolls_back_or_skips_a_version
  cargo kani -p awaken-store-schema \
    --harness replaying_a_fully_applied_migration_plan_is_a_noop
  cargo kani -p awaken-ext-compact \
    --harness fold_point_preserves_the_requested_suffix
  cargo kani -p awaken-ext-compact \
    --harness fold_point_is_present_exactly_when_triggered_with_nonempty_prefix
  cargo kani -p awaken-runtime-contract \
    --harness terminal_calls_are_never_reentered
  cargo kani -p awaken-runtime-contract \
    --harness only_the_matching_approval_ticket_enters_execution
  cargo kani -p awaken-runtime-contract \
    --harness every_tool_call_transition_has_the_unique_documented_precondition
  cargo kani -p awaken-runtime-contract \
    --harness terminal_tool_calls_only_accept_result_staging
  cargo kani -p awaken-runtime-contract \
    --harness run_end_sealing_targets_exactly_nonterminal_calls
  cargo kani -p awaken-runtime-contract \
    --harness child_result_is_consumed_only_from_ready
  cargo kani -p awaken-runtime-contract \
    --harness terminal_delivery_phases_never_reopen
  cargo kani -p awaken-mcp-server-core \
    --harness one_request_has_at_most_one_final_response
  cargo kani -p awaken-mcp-server-core \
    --harness notifications_never_have_a_jsonrpc_response
  cargo kani -p awaken-mcp-server-core \
    --harness final_response_is_progress_absorbing
  cargo kani -p awaken-mcp-server-core \
    --harness rejected_requests_never_enter_the_host
  cargo kani -p awaken-mcp-server-core \
    --harness cancellation_never_produces_a_success_result
  cargo kani -p awaken-worker-contract \
    --harness accepted_version_is_inside_worker_range
  cargo kani -p awaken-worker-contract \
    --harness non_ready_worker_never_accepts_work
  cargo kani -p awaken-worker-contract \
    --harness never_replace_rejects_every_replacement
  cargo kani -p awaken-worker-contract \
    --harness sandbox_continuity_authorizes_replacement_exactly_when_bound
  cargo kani -p awaken-worker-contract \
    --harness same_incarnation_never_spends_replacement_authority
else
  echo "skipped Kani: install with 'cargo install --locked kani-verifier && cargo kani setup'"
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
else
  echo "skipped TLAPS: set TLAPM_BIN or install tlapm"
  missing=1
fi

tla_jar="${TLA2TOOLS_JAR:-}"
if command -v java >/dev/null 2>&1 && [ -n "$tla_jar" ] && [ -f "$tla_jar" ]; then
  tlc_state_root="$formal_tmp_root/tlc-states"
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/run-ingress" \
    -config formal/tla/RunIngress.cfg formal/tla/RunIngress.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/work-queue" \
    -config formal/tla/WorkQueue.cfg formal/tla/WorkQueue.tla
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
    -metadir "$tlc_state_root/authz-kernel" \
    -config formal/tla/AuthzKernel.cfg formal/tla/AuthzKernel.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/session-ownership" \
    -config formal/tla/SessionOwnership.cfg formal/tla/SessionOwnership.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/circuit-breaker" \
    -config formal/tla/CircuitBreaker.cfg formal/tla/CircuitBreaker.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/config-cas" \
    -config formal/tla/ConfigCAS.cfg formal/tla/ConfigCAS.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/live-inbox" \
    -config formal/tla/LiveInbox.cfg formal/tla/LiveInbox.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/checkpoint-recovery" \
    -config formal/tla/CheckpointRecovery.cfg formal/tla/CheckpointRecovery.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/webhook-outbox" \
    -config formal/tla/WebhookOutbox.cfg formal/tla/WebhookOutbox.tla
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
    -metadir "$tlc_state_root/tool-result-protocol" \
    -config formal/tla/ToolResultProtocol.cfg formal/tla/ToolResultProtocol.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-drain" \
    -config formal/tla/WorkerDrain.cfg formal/tla/WorkerDrain.tla
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
    -metadir "$tlc_state_root/management-audit-intent" \
    -config formal/tla/ManagementAuditIntent.cfg formal/tla/ManagementAuditIntent.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/credential-inventory" \
    -config formal/tla/CredentialInventory.cfg formal/tla/CredentialInventory.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/mcp-server" \
    -config formal/tla/McpServer.cfg formal/tla/McpServer.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -metadir "$tlc_state_root/worker-replacement" \
    -config formal/tla/WorkerReplacement.cfg formal/tla/WorkerReplacement.tla
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
