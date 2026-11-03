#!/usr/bin/env bash
# Context for existing execution owners. This helper never executes their work.

awaken_finish_build_observation() {
  local original_status="$1" observation_status=0
  trap - EXIT
  python3 scripts/ci/ci_performance.py finish --exit-code "$original_status" || observation_status=$?
  if [ "$observation_status" -ne 0 ] && [ "$original_status" -eq 0 ]; then
    original_status=74
  fi
  exit "$original_status"
}

awaken_begin_build_observation() {
  local label="$1" context_file context_status=0
  if [ -n "${AWAKEN_BUILD_RUN_DIRECTORY:-}" ]; then
    python3 scripts/ci/ci_performance.py validate || context_status=$?
    [ "$context_status" -ne 0 ] || return 0
    [ "$context_status" -eq 3 ] || return "$context_status"
  fi
  context_file=$(mktemp "${TMPDIR:-/tmp}/awaken-build-context.XXXXXX") || return 74
  if ! python3 scripts/ci/ci_performance.py begin --label "$label" --shell-file "$context_file"; then
    rm -f "$context_file"
    return 74
  fi
  # Only shlex-quoted exports emitted by our context writer are sourced.
  source "$context_file"
  rm -f "$context_file"
  trap 'awaken_finish_build_observation "$?"' EXIT
}

awaken_begin_hook_observation() {
  export AWAKEN_BUILD_WORKLOAD_KIND="${AWAKEN_BUILD_WORKLOAD_KIND:-feedback}"
  export AWAKEN_BUILD_OBSERVATION_KIND="${AWAKEN_BUILD_OBSERVATION_KIND:-project}"
  awaken_begin_build_observation pre-commit
}
