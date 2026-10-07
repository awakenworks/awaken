#!/usr/bin/env bash
# Portable command deadline for Linux and macOS. The caller keeps `set -e`
# policy; this helper returns 124 on timeout, matching GNU coreutils `timeout`.

run_with_deadline() (
  local seconds=$1
  shift
  local deadline_dir
  deadline_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  if [ "${1:-}" = cargo ]; then
    # Cargo admission owns the wait and capacity preflight. Its external-command
    # protocol then enters this same deadline owner; nested Cargo inherits the
    # admitted job. Only actual execution spends the caller's unchanged budget.
    local cargo_deadline_dir
    cargo_deadline_dir=$(mktemp -d "${TMPDIR:-/tmp}/awaken-cargo-deadline.XXXXXX")
    trap 'rm -f -- "$cargo_deadline_dir/cargo-awaken-ci-deadline"; rmdir -- "$cargo_deadline_dir"' EXIT
    ln -s "$deadline_dir/deadline.py" "$cargo_deadline_dir/cargo-awaken-ci-deadline"
    PATH="$cargo_deadline_dir:$PATH" cargo awaken-ci-deadline "$seconds" -- "$@"
  else
    python3 "$deadline_dir/deadline.py" "$seconds" -- "$@"
  fi
)
