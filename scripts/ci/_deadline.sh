#!/usr/bin/env bash
# Portable command deadline for Linux and macOS. The caller keeps `set -e`
# policy; this helper returns 124 on timeout, matching GNU coreutils `timeout`.

run_with_deadline() {
  local seconds=$1
  shift
  local deadline_dir
  deadline_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  python3 "$deadline_dir/deadline.py" "$seconds" -- "$@"
}
