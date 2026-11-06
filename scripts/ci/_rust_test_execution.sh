#!/usr/bin/env bash
# Required focused Rust runs. Callers keep their `cargo test`-shaped selection and
# --require-case=NAME floors; scripts/ci/nextest_execution.py projects it onto
# nextest, and qualifies nextest's own list JSON and JUnit report.
source "$(dirname "${BASH_SOURCE[0]}")/_deadline.sh"

run_required_rust_tests() (
  local evidence budget build_budget status=0
  budget="${AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS:-600}"
  [[ "$budget" =~ ^[1-9][0-9]*$ ]] || { echo "invalid required-test budget" >&2; exit 2; }
  build_budget="${AWAKEN_REQUIRED_TEST_BUILD_TIMEOUT_SECONDS:-3600}"
  [[ "$build_budget" =~ ^[1-9][0-9]*$ ]] || { echo "invalid required-build budget" >&2; exit 2; }
  mkdir -p "$CARGO_TARGET_DIR/test-timings"
  evidence=$(mktemp -d "$CARGO_TARGET_DIR/test-timings/rust-execution.XXXXXX") || exit 1
  # A cold build gets its own budget, so it never spends the execution deadline;
  # a successful build is never counted as a test run.
  run_with_deadline --admit "$build_budget" python3 scripts/ci/nextest_execution.py build -- "$@" \
    >"$evidence/build.log" 2>&1 || status=$?
  if [ "$status" -ne 0 ]; then
    cat "$evidence/build.log"
    echo "build evidence: $evidence/build.log"
    exit "$status"
  fi
  # A run the deadline kills keeps its receipt at status "running": it never qualifies.
  run_with_deadline --admit "$budget" python3 scripts/ci/nextest_execution.py run \
    --evidence "$evidence" -- "$@" || status=$?
  [ "$status" -eq 0 ] || cat "$evidence/results.log" 2>/dev/null || true
  echo "execution evidence: $evidence"
  exit "$status"
)
