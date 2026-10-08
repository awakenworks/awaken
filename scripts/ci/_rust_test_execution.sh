#!/usr/bin/env bash
# Required focused libtest runs: list and execute the SAME argv, then qualify
# named outcomes. Domain scripts still own composition; this owns no case list.
source "$(dirname "${BASH_SOURCE[0]}")/_deadline.sh"

run_required_rust_tests() (
  local evidence budget remaining started=$SECONDS status=0 separator=-- argument
  local locked=(--locked)
  budget="${AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS:-600}"
  [[ "$budget" =~ ^[1-9][0-9]*$ ]] || { echo "invalid required-test budget" >&2; exit 2; }
  mkdir -p "$CARGO_TARGET_DIR/test-timings"
  evidence=$(mktemp -d "$CARGO_TARGET_DIR/test-timings/rust-execution.XXXXXX") || exit 1
  git rev-parse HEAD > "$evidence/source.txt" || exit 1
  git diff --binary HEAD > "$evidence/working-tree.patch" || exit 1
  printf '%s\n' "$@" > "$evidence/arguments.txt"
  for argument in "$@"; do
    [ "$argument" != -- ] || separator=""
    [ "$argument" != --locked ] || locked=()
    case "$argument" in
      --list|--format|--format=*) echo "required test execution owns list/output format" >&2; exit 2 ;;
    esac
  done
  # A failed build/list cannot become an empty successful plan. Existing
  # deadline.py owns process-tree termination and Cargo admission for both.
  run_with_deadline "$budget" cargo test "${locked[@]}" "$@" ${separator:+$separator} --list > "$evidence/list.log" 2>&1 || {
    cat "$evidence/list.log"; exit 1;
  }
  remaining=$((budget - SECONDS + started))
  [ "$remaining" -gt 0 ] || { echo "required-test budget exhausted by listing" >&2; exit 124; }
  run_with_deadline "$remaining" cargo test "${locked[@]}" "$@" ${separator:+$separator} --show-output > "$evidence/results.log" 2>&1 || status=$?
  if ! python3 scripts/ci/check_rust_test_execution.py --inventory "$evidence/list.log" \
    --results "$evidence/results.log" --exit-code "$status"; then
    cat "$evidence/results.log"
    echo "execution evidence: $evidence" >&2
    exit 1
  fi
  echo "execution evidence: $evidence"
)
