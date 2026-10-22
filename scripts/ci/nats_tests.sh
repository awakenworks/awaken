#!/usr/bin/env bash
# Domain-owned live NATS laws, with the shared exact-selection/deadline owner.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"
source scripts/ci/_rust_test_execution.sh
if [ "$#" -ne 0 ] || [ -z "${AWAKEN_TEST_NATS_URL:-}" ]; then
  echo "usage: scripts/ci/nats_tests.sh (requires AWAKEN_TEST_NATS_URL)" >&2
  exit 2
fi
export AWAKEN_TEST_REQUIRE_NATS=1
run_required_rust_tests -p awaken-dispatch --features nats --lib wake::nats_tests \
  --require-case=wake::nats_tests::nats_wake_signal_notifies_a_waiter \
  --require-case=wake::nats_tests::nats_fixture_absence_and_configured_failure_are_distinct -- --test-threads=1
run_required_rust_tests -p awaken-sandbox --features hand --lib nats \
  --require-case=hand::tests::run_nats_serves_a_real_tool_when_a_broker_is_reachable \
  --require-case=hand::tests::nats_fixture_absence_and_configured_failure_are_distinct -- --test-threads=1
