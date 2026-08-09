#!/usr/bin/env bash
# Portable command deadline for Linux and macOS. The caller keeps `set -e`
# policy; this helper returns 124 on timeout, matching GNU coreutils `timeout`.

run_with_deadline() {
  local seconds=$1
  shift
  local marker
  marker=$(mktemp "${TMPDIR:-/tmp}/awaken-command-timeout.XXXXXX")
  rm -f "$marker"

  "$@" &
  local command_pid=$!
  (
    sleep "$seconds"
    if kill -0 "$command_pid" 2>/dev/null; then
      : >"$marker"
      kill -TERM "$command_pid" 2>/dev/null || true
      sleep 5
      kill -KILL "$command_pid" 2>/dev/null || true
    fi
  ) &
  local watchdog_pid=$!

  local status=0
  wait "$command_pid" || status=$?
  kill "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  if [[ -f "$marker" ]]; then
    rm -f "$marker"
    return 124
  fi
  rm -f "$marker"
  return "$status"
}
