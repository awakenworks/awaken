#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

require_tools=0
if [ "${1:-}" = "--require-tools" ]; then
  require_tools=1
fi

missing=0

if command -v cargo-kani >/dev/null 2>&1; then
  cargo kani -p awaken-agent-contract \
    --harness ended_is_absorbing_for_every_next_state
  cargo kani -p awaken-agent-contract \
    --harness legacy_wire_accepts_exactly_the_legal_dispositions
  cargo kani -p awaken-agent-contract \
    --harness a_result_can_be_marked_delivered_only_from_pending
  cargo kani -p awaken-agent-contract \
    --harness an_ended_parent_never_accepts_a_result_delivery
  cargo kani -p awaken-agent-contract \
    --harness exactly_once_effects_have_unique_preconditions
  cargo kani -p awaken-session-contract \
    --harness awaiting_constructor_cannot_create_a_terminal_or_failed_outcome
  cargo kani -p awaken-session-contract \
    --harness ended_constructor_carries_the_only_failure_authority_and_no_pending_tool
else
  echo "skipped Kani: install with 'cargo install --locked kani-verifier && cargo kani setup'"
  missing=1
fi

tla_jar="${TLA2TOOLS_JAR:-}"
if command -v java >/dev/null 2>&1 && [ -n "$tla_jar" ] && [ -f "$tla_jar" ]; then
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -config formal/tla/RunIngress.cfg formal/tla/RunIngress.tla
  java -XX:+UseParallelGC -jar "$tla_jar" \
    -config formal/tla/Delegation.cfg formal/tla/Delegation.tla
else
  echo "skipped TLC: set TLA2TOOLS_JAR and install Java 11+"
  missing=1
fi

if [ "$require_tools" -eq 1 ] && [ "$missing" -ne 0 ]; then
  echo "formal verification tools are required but unavailable" >&2
  exit 1
fi
