#!/usr/bin/env bash
# Observation bridge only. Lefthook retains selection and execution ownership.
set -uo pipefail
if [ "$#" -ne 1 ] || [ "$1" != --observe ]; then
  echo 'usage: observe.sh --observe' >&2
  exit 2
fi
cd "$(dirname "$0")/../../.." || exit 1
hook_status=""
trap 'exit "${hook_status:-130}"' INT
trap 'exit "${hook_status:-143}"' TERM
. scripts/ci/_build_observation.sh
# Each Git attempt owns a child run, even inside an already observed job.
unset AWAKEN_BUILD_RUN_DIRECTORY
awaken_begin_hook_observation || exit $?
hook_config=""
hook_body_complete=0
finish_observed_hook() {
  local status=$? cleanup_status=0
  trap - EXIT
  trap '' INT TERM
  if [ "$hook_body_complete" -ne 1 ] && [ "$status" -eq 0 ]; then status=1; fi
  if [ -n "$hook_config" ]; then
    python3 - "$hook_config" "$AWAKEN_BUILD_RUN_DIRECTORY" <<'PY' || cleanup_status=$?
import sys
from pathlib import Path
path, owner = map(Path, sys.argv[1:])
assert path.parent == owner and path.name.startswith('hook-config-') and path.suffix == '.yml'
path.unlink(missing_ok=True)
PY
  fi
  [ "$status" -ne 0 ] || status=$cleanup_status
  awaken_finish_build_observation "$status"
}
trap finish_observed_hook EXIT
hook_config=$(python3 scripts/ci/ci_performance.py hook-config) || exit $?
parent_config="${LEFTHOOK_CONFIG-}" parent_config_set="${LEFTHOOK_CONFIG+x}"
AWAKEN_HOOK_PARENT_CONFIG="$parent_config" AWAKEN_HOOK_PARENT_CONFIG_SET="$parent_config_set" \
  LEFTHOOK_CONFIG="$hook_config" python3 scripts/ci/_native_shell.py --lefthook-pre-commit
hook_status=$?
hook_body_complete=1
exit "$hook_status"
