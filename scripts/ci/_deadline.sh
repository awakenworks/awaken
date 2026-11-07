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

# One line: who holds the host build slots now and how many wait. Prefers the
# host's own `--status` slot table (scheduler.slots, fair-slots-v2); the older
# host tool has none, so this falls back to its queue tickets. Informational only.
awaken_admission_holder() {
  local observer="${AWAKEN_HOST_ADMISSION:-}"
  [ -n "$observer" ] || observer=$(command -v awaken-build-admission 2>/dev/null) || observer=""
  python3 - "${AWAKEN_ADMISSION_STATE:-$HOME/.local/state/awaken-worktree-gc}" "$observer" <<'PY'
import json, subprocess, sys, time
from pathlib import Path


def from_status(observer):
    try:
        status = json.loads(subprocess.run([observer, "--status"], capture_output=True, text=True,
                                           timeout=20).stdout)
        scheduler = status["scheduler"]
        slots = scheduler["slots"]
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        return None
    if not isinstance(slots, list):
        return None
    busy = []
    for slot in slots:
        if not isinstance(slot, dict) or slot.get("state") != "busy":
            continue
        command = " ".join(str(item) for item in slot.get("arguments") or [])[:120]
        held = slot.get("held_seconds")
        busy.append(f"slot {slot.get('slot')} {slot.get('workload_kind') or '?'}"
                    + (" exclusive" if slot.get("exclusive") else "")
                    + (f" held {held:.0f}s" if isinstance(held, (int, float)) else "")
                    + f" by pid {slot.get('pid')} in {slot.get('cwd') or '?'}: cargo {command}")
    by_kind = (scheduler.get("queue") or {}).get("by_kind") or {}
    queued = ", ".join(f"{kind} {count}" for kind, count in sorted(by_kind.items()))
    return (f"{len(busy)} of {len(slots)} slots busy" + "".join("; " + row for row in busy)
            + f"; queued: {queued or 'none'}")


def from_tickets(state):
    holder, queued = None, 0
    for ticket in sorted((Path(state) / "admission-queue").glob("*.json")):
        try:
            record = json.loads(Path(json.loads(ticket.read_text())["observation"]).read_text())
        except (OSError, ValueError, KeyError, TypeError):
            continue
        if record.get("status") == "queued":
            queued += 1
        elif record.get("holds_execution_lane") and record.get("status") in ("admission", "running"):
            holder = record
    if holder is None:
        return f"no current holder recorded; {queued} queued"
    held = time.time() - (holder.get("lane_started_at_unix") or holder.get("created_at_unix") or time.time())
    command = " ".join(str(item) for item in holder.get("arguments", []))[:120]
    return (f"held {held:.0f}s by pid {holder.get('pid')} ({holder.get('workload_kind', '?')}) "
            f"in {holder.get('cwd', '?')}: cargo {command}; {queued} queued")


print((sys.argv[2] and from_status(sys.argv[2])) or from_tickets(sys.argv[1]))
PY
}

# Print the wait and the current holder every 30 s until <marker> exists.
awaken_report_admission_wait() {
  local marker="$1" waited=0
  while sleep 30; do
    [ ! -e "$marker" ] || return 0
    waited=$((waited + 30))
    echo "[admission] waiting ${waited}s for a host build slot; $(awaken_admission_holder 2>/dev/null || echo 'holder unknown')" >&2
  done
}

# A budget a shell checks between its own stages (pg_tests.sh) excludes the
# time its admitted Cargo descendants spent queued, through the same ledger
# every deadline keeps (deadline.py). awaken_budget_open sets
# awaken_budget_ledger and lists it for descendants; with the older host tool
# the ledger stays empty and nothing is excluded.
awaken_budget_open() {
  local directory
  directory=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd) || return 1
  awaken_budget_ledger=$(python3 "$directory/deadline.py" --open-ledger) || return 1
  export AWAKEN_DEADLINE_LEDGERS="$awaken_budget_ledger${AWAKEN_DEADLINE_LEDGERS:+:$AWAKEN_DEADLINE_LEDGERS}"
}

# Whole seconds queued since awaken_budget_open; 0 without a ledger.
awaken_budget_queued_seconds() {
  local directory queued
  if [ -n "${awaken_budget_ledger:-}" ]; then
    directory=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd) &&
      queued=$(python3 "$directory/deadline.py" --queued-seconds "$awaken_budget_ledger") &&
      [[ "$queued" =~ ^[0-9]+$ ]] && { printf '%s\n' "$queued"; return 0; }
  fi
  echo 0
}

awaken_budget_close() {
  [ -n "${awaken_budget_ledger:-}" ] || return 0
  python3 "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/deadline.py" --close-ledger "$awaken_budget_ledger"
  awaken_budget_ledger=""
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
    # deadline.py owns which host schedulers can run the cooperative root.
    python3 "$deadline_dir/deadline.py" --require-cooperative \
      "$host_cargo_directory/awaken-build-admission" || return 75
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
    # Cargo admission owns the wait and capacity preflight. deadline.py --admit
    # enters it through Cargo's external-command protocol (a temporary
    # cargo-awaken-ci-deadline entry it owns); that post-admission entry owns
    # the execution budget, nested Cargo inherits the admitted job, and every
    # enclosing deadline leaves the queue out of its own budget.
    python3 "$deadline_dir/deadline.py" --admit "$seconds" -- "$@"
  else
    python3 "$deadline_dir/deadline.py" "$seconds" -- "$@"
  fi
)

# Existing command owner, used only at declared safe Formal phase boundaries.
# The cooperative root's deadline bounds all phases; with a host that exports
# admission grants, each phase's queue is left out of it (deadline.py ledger).
run_admitted_phase() {
  local budget="$1"
  shift
  if [ "${AWAKEN_ADMISSION_PHASE_MODE:-}" = 1 ] && [ "${AWAKEN_CARGO_ADMITTED:-}" != 1 ]; then
    run_with_deadline --admit "$budget" "$@"
  else
    "$@"
  fi
}
