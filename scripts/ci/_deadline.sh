#!/usr/bin/env bash
# Portable command deadline for Linux and macOS. The caller keeps `set -e`
# policy; this helper returns 124 on timeout, matching GNU coreutils `timeout`.

run_with_deadline() (
  local admitted_job=0
  if [ "${1:-}" = --admit ]; then
    admitted_job=1
    shift
  fi
  local seconds=$1
  shift
  local deadline_dir
  deadline_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
      # Native Windows Cargo cannot discover a Unix shebang/symlink addon.
      # Its Rustup/Cargo process tree is owned by the Windows Job in this same
      # deadline authority. POSIX Cargo admission retains its protocol below.
      local native_entry
      native_entry=$(cygpath -m "$deadline_dir/deadline.py") || return 2
      case "$1" in
        *.sh)
          set -- "$(cygpath -m "$(command -v bash)")" "$(cygpath -m "$1")" "${@:2}"
          ;;
        /*) set -- "$(cygpath -m "$1")" "${@:2}" ;;
      esac
      # Convert only the executable/script coordinates we own. MSYS rewriting
      # arbitrary argv corrupts Docker container paths, bind mounts and data.
      MSYS2_ARG_CONV_EXCL='*' python3 "$native_entry" "$seconds" -- "$@"
      return
      ;;
  esac
  if [ "$admitted_job" -eq 1 ] || [ "${1:-}" = cargo ]; then
    # Cargo admission owns the wait and capacity preflight. Its external-command
    # protocol then enters this same deadline owner; nested Cargo inherits the
    # admitted job. Only actual execution spends the caller's unchanged budget.
    local cargo_deadline_dir
    cargo_deadline_dir=$(mktemp -d "${TMPDIR:-/tmp}/awaken-cargo-deadline.XXXXXX")
    trap 'rm -f -- "$cargo_deadline_dir/cargo-awaken-ci-deadline"; rmdir -- "$cargo_deadline_dir"' EXIT
    ln -s "$deadline_dir/deadline.py" "$cargo_deadline_dir/cargo-awaken-ci-deadline"
    # A redirected call under errexit can unwind this function's locals before
    # Bash runs EXIT. Finish inside this subshell so cleanup retains its exact
    # owned path and cannot mask an admission or command failure.
    local cargo_deadline_status
    if PATH="$cargo_deadline_dir:$PATH" cargo awaken-ci-deadline "$seconds" -- "$@"; then
      cargo_deadline_status=0
    else
      cargo_deadline_status=$?
    fi
    exit "$cargo_deadline_status"
  else
    python3 "$deadline_dir/deadline.py" "$seconds" -- "$@"
  fi
)
