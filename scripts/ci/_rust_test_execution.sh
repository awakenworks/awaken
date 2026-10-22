#!/usr/bin/env bash
# Required focused libtest runs: list and execute the SAME argv, then qualify
# named outcomes. Domain scripts still own composition; this owns no case list.
source "$(dirname "${BASH_SOURCE[0]}")/_deadline.sh"

run_required_rust_tests() (
  local evidence budget build_budget status=0 validation=0
  budget="${AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS:-600}"
  [[ "$budget" =~ ^[1-9][0-9]*$ ]] || { echo "invalid required-test budget" >&2; exit 2; }
  build_budget="${AWAKEN_REQUIRED_TEST_BUILD_TIMEOUT_SECONDS:-3600}"
  [[ "$build_budget" =~ ^[1-9][0-9]*$ ]] || { echo "invalid required-build budget" >&2; exit 2; }
  mkdir -p "$CARGO_TARGET_DIR/test-timings"
  evidence=$(mktemp -d "$CARGO_TARGET_DIR/test-timings/rust-execution.XXXXXX") || exit 1
  # Compile the same authored Cargo graph before spending the unchanged shared
  # list/run budget. Cold MSVC compilation may exceed that execution deadline.
  # Built is a separate, source-bound fact; it never counts as test execution.
  run_with_deadline --admit "$build_budget" python3 scripts/ci/check_rust_test_execution.py \
    --prepare --build-log "$evidence/build.log" --receipt "$evidence/build.json" \
    -- "$@" || status=$?
  if [ "$status" -ne 0 ]; then
    cat "$evidence/build.log" 2>/dev/null || true
    echo "build evidence: $evidence/build.json"
    exit "$status"
  fi
  # E9 still owns one admitted deadline for listing AND execution; neither
  # starts before its queue finishes, and list/run do not get separate timers.
  run_with_deadline --admit "$budget" python3 scripts/ci/check_rust_test_execution.py \
    --run --prepared "$evidence/build.json" --inventory "$evidence/list.log" --results "$evidence/results.log" \
    --receipt "$evidence/results.json" -- "$@" || status=$?
  # E5/E10/E12: an observed deadline/child terminal retains its original cause
  # even when source or record finalization fails. If this parent shell itself
  # dies, this call cannot be promised: its on-disk record remains partial.
  python3 scripts/ci/check_rust_test_execution.py --inventory "$evidence/list.log" \
    --results "$evidence/results.log" --exit-code "$status" \
    --receipt "$evidence/results.json" || validation=$?
  if [ "$status" -ne 0 ] || [ "$validation" -ne 0 ]; then
    cat "$evidence/list.log" "$evidence/results.log" 2>/dev/null || true
  fi
  echo "execution evidence: $evidence"
  [ "$status" -eq 0 ] || exit "$status"
  exit "$validation"
)
