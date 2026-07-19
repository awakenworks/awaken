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
