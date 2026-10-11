#!/usr/bin/env bash
# Required focused libtest runs: list and execute the SAME argv, then qualify
# named outcomes. Domain scripts still own composition; this owns no case list.
source "$(dirname "${BASH_SOURCE[0]}")/_deadline.sh"

run_required_rust_tests() (
  local evidence budget status=0 validation=0
  budget="${AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS:-600}"
  [[ "$budget" =~ ^[1-9][0-9]*$ ]] || { echo "invalid required-test budget" >&2; exit 2; }
  mkdir -p "$CARGO_TARGET_DIR/test-timings"
  evidence=$(mktemp -d "$CARGO_TARGET_DIR/test-timings/rust-execution.XXXXXX") || exit 1
  # E9: admit the complete list/run job before its unchanged execution budget
  # starts. The existing sequence owns commands; no second per-phase timer.
  run_with_deadline --admit "$budget" python3 scripts/ci/check_rust_test_execution.py \
    --run --inventory "$evidence/list.log" --results "$evidence/results.log" \
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
