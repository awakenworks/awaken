#!/usr/bin/env bash
# Portable command deadline for Linux and macOS. The caller keeps `set -e`
# policy; this helper returns 124 on timeout, matching GNU coreutils `timeout`.

# A full qualification requires the host's admitted Cargo entry, even when a
# toolchain setup action prepended rustup's native Cargo to PATH. Both installed
# names contain the same host policy; the observer itself is never an executor.
# Keep this selection local to the full gate: ordinary developer deadlines do
# not require the qualified Linux host installation.
awaken_host_cargo_directory() {
  local observer="${AWAKEN_HOST_ADMISSION:-}" directory
  if [ -z "$observer" ]; then
    observer=$(command -v awaken-build-admission) || {
      echo "full gate requires the canonical awaken-build-admission command" >&2
      return 75
    }
  fi
  directory=$(cd -- "$(dirname -- "$observer")" && pwd -P) || return 75
  observer="$directory/$(basename -- "$observer")"
  if [ ! -x "$observer" ] || [ ! -x "$directory/cargo" ] ||
     ! cmp -s -- "$observer" "$directory/cargo"; then
    echo "full gate requires cargo and awaken-build-admission installed from the same host policy" >&2
    return 75
  fi
  printf '%s\n' "$directory"
}

# One line: who holds the host build lane now and how many wait. Reads the
# host admission queue (the `cargo` wrapper's own records); informational only.
awaken_admission_holder() {
  python3 - "${AWAKEN_ADMISSION_STATE:-$HOME/.local/state/awaken-worktree-gc}" <<'PY'
import json, sys, time
from pathlib import Path
holder, queued = None, 0
for ticket in sorted((Path(sys.argv[1]) / "admission-queue").glob("*.json")):
    try:
        record = json.loads(Path(json.loads(ticket.read_text())["observation"]).read_text())
    except (OSError, ValueError, KeyError, TypeError):
        continue
    if record.get("status") == "queued":
        queued += 1
    elif record.get("holds_execution_lane") and record.get("status") in ("admission", "running"):
        holder = record
if holder is None:
    print(f"no current holder recorded; {queued} queued")
else:
    held = time.time() - (holder.get("lane_started_at_unix") or holder.get("created_at_unix") or time.time())
    command = " ".join(str(item) for item in holder.get("arguments", []))[:120]
    print(f"held {held:.0f}s by pid {holder.get('pid')} ({holder.get('workload_kind', '?')}) "
          f"in {holder.get('cwd', '?')}: cargo {command}; {queued} queued")
PY
}

# Print the wait and the current holder every 30 s until <marker> exists.
awaken_report_admission_wait() {
  local marker="$1" waited=0
  while sleep 30; do
    [ ! -e "$marker" ] || return 0
    waited=$((waited + 30))
    echo "[admission] waiting ${waited}s for the host build lane; $(awaken_admission_holder 2>/dev/null || echo 'holder unknown')" >&2
  done
}

run_with_deadline() (
  local admitted_job=0 cooperative_job=0
  if [ "${1:-}" = --admit ]; then
    admitted_job=1
    shift
  elif [ "${1:-}" = --cooperative-admit ]; then
    admitted_job=1
    cooperative_job=1
    shift
  fi
  local seconds=$1
  shift
  local deadline_dir
  deadline_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  if [ "$cooperative_job" -eq 1 ]; then
    local host_cargo_directory
    host_cargo_directory=$(awaken_host_cargo_directory) || return "$?"
    python3 - "$host_cargo_directory/awaken-build-admission" <<'PY' || return 75
import json, subprocess, sys
try:
    value=json.loads(subprocess.check_output([sys.argv[1], '--capabilities'], text=True))
    if (value.get('kind') != 'host-admission-capabilities' or value.get('schema_version') != 1
            or value.get('scheduler') != 'fair-single-lane-v1'
            or value.get('execution_slots') != 1 or value.get('cooperative_formal') is not True):
        raise ValueError('unsupported cooperative admission')
except (OSError, ValueError, subprocess.SubprocessError) as error:
    print('cooperative formal admission unavailable; install the committed host policy: '+str(error), file=sys.stderr)
    raise SystemExit(75)
PY
    PATH="$host_cargo_directory:$PATH"
    export AWAKEN_ADMISSION_MODE=cooperative AWAKEN_BUILD_WORKLOAD_KIND=formal
  fi
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

# Existing command owner, used only at declared safe Formal phase boundaries.
# The cooperative root's original deadline still bounds all phases and queues.
run_admitted_phase() {
  local budget="$1"
  shift
  if [ "${AWAKEN_ADMISSION_PHASE_MODE:-}" = 1 ] && [ "${AWAKEN_CARGO_ADMITTED:-}" != 1 ]; then
    run_with_deadline --admit "$budget" "$@"
  else
    "$@"
  fi
}
