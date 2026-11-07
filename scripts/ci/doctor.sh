#!/usr/bin/env bash
# Report which local prerequisites are present, and which check group each
# missing one costs.
#
# There is no prerequisite document in this repository, and a prose one would
# rot: the gates are what actually decide which tools are needed. This script
# used to carry its own table of tools, which rotted for exactly that reason —
# nothing compared it against what the gates really run, and four gate failures
# in one session each named the wrong cause.
#
# So the table is gone from here. `scripts/ci/gate_tools.json` declares every
# external tool once — how it is located, any version pin, which gates need it,
# and whether it is required or optional-with-a-named-skip — and this script is
# one of its readers. Reporting installs nothing and changes nothing.
#
# `--fix` is the one command that prepares a fresh worktree: it installs what
# local caches can supply — today every Node root, from its lockfile, offline —
# and is a no-op when that is already done. `--fix --online` lets the package
# managers fetch what the cache lacks. It never installs host tools and never
# writes Git configuration; the report still names those.
#
# Exit status is zero whenever the report itself is complete. A missing tool is
# a fact about this machine, not a failure of the report; `--require <group>`
# asks the opposite question ("can I run that group right now?") and fails when
# the answer is no, naming the tool rather than whatever its absence breaks.
set -uo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_provider_environment.sh
awaken_append_repo_tools

usage() {
  echo "usage: scripts/ci/doctor.sh [--fix [--online]] [--require <group>] [--self-test]" >&2
  exit 2
}

# Groups a named tool gates, as a space-separated list; fails when unknown.
tool_groups() { python3 scripts/ci/gate_tools.py gates-of "$1"; }

report() {
  python3 scripts/ci/gate_tools.py report
  echo ""
  # Disk is a prerequisite like any other. It filled twice in one session and
  # arrived at the formal gate as a SIGABRT on a truncated GOTO binary, so the
  # headroom and what could be reclaimed belong in the same report as the tools.
  python3 scripts/ci/reclaim_build_space.py --summary
  echo ""
  echo "run scripts/ci/doctor.sh --fix to install what local caches can supply (Node roots)."
  echo "run scripts/ci/check-all.sh --list-groups to see every group."
  echo "run scripts/ci/gate_tools.py require --gate <group> to precheck one group."
  echo "run scripts/ci/reclaim_build_space.py to preview the host GC; review its plan before --apply."
}

self_test() {
  # | Rule | cause                                    | effect                        |
  # | T1   | a tool row names groups                  | tool_groups returns them      |
  # | T2   | an unknown tool                          | tool_groups fails             |
  # | T3   | a row carries a conditional qualifier    | the group name survives alone |
  # | T4   | every row is well formed                 | three fields, no empty field  |
  # | T5   | the manifest is this report's only table | the reader's own proof passes |
  # | T6   | the report also names disk headroom      | the reclamation proof passes  |
  # | T7   | --fix with an unknown second word        | usage, exit 2, nothing runs   |
  # | T8   | a gate or hook entry point starts        | its temp is on the build disk |
  # The bootstrap's own table (idempotence, offline cache miss, the named fix)
  # is gate_tools.py M13-M15, which T5 runs.
  local row name groups hint status
  [ "$(tool_groups docker)" = "postgres kubernetes k3d sandbox e2e" ] || {
    echo "T1 a known tool must report the groups it gates" >&2
    return 1
  }
  if tool_groups definitely-not-a-tool >/dev/null 2>&1; then
    echo "T2 an unknown tool must not report groups" >&2
    return 1
  fi
  # `cmake` is needed only to rebuild the CBMC backend. That qualifier lives in
  # its own manifest field, so the group a caller can act on stays a group name.
  groups="$(tool_groups cmake)"
  [ "$groups" = "formal" ] || { echo "T3 a qualifier must not swallow the group" >&2; return 1; }
  while IFS= read -r row; do
    name="${row%%|*}"
    groups="${row#*|}"; hint="${groups#*|}"; groups="${groups%%|*}"
    if [ -z "$name" ] || [ -z "$groups" ] || [ -z "$hint" ] || [ "$hint" = "$row" ]; then
      echo "T4 malformed tool row: $row" >&2
      return 1
    fi
  done < <(python3 scripts/ci/gate_tools.py rows)
  python3 scripts/ci/gate_tools.py --self-test >/dev/null || {
    echo "T5 the tool manifest does not describe this machine" >&2
    return 1
  }
  # | T6 | the report names disk headroom | the reclamation tool's own proof passes |
  python3 scripts/ci/reclaim_build_space.py --self-test >/dev/null || {
    echo "T6 the build-space adapter cannot delegate to the canonical owner" >&2
    return 1
  }
  status=0
  "$0" --fix --bogus >/dev/null 2>&1 || status=$?
  [ "$status" -eq 2 ] || { echo "T7 --fix admitted an unknown argument" >&2; return 1; }
  # T8: a full root filesystem failed tests at random while the build disk was
  # empty. Entry points that select a Cargo target place their temp through
  # awaken_configure_cargo_target; the two that select none call it directly.
  # _cargo_target.sh --self-test owns the placement table itself.
  for name in scripts/ci/check-all.sh scripts/ci/pre-commit/observe.sh; do
    grep -Fq 'scripts/ci/_cargo_target.sh && awaken_configure_gate_temp "$PWD" || exit $?' "$name" ||
      { echo "T8 $name no longer places gate temp files on the build disk" >&2; return 1; }
  done
  grep -Fq 'awaken_configure_gate_temp "$repository_root" || exit $?' scripts/ci/_cargo_target.sh ||
    { echo "T8 selecting a Cargo target no longer places gate temp files" >&2; return 1; }
  echo "doctor self-test: every tool row names a group and a way to get it"
}

case "${1:-}" in
  "") report ;;
  --self-test) [ "$#" -eq 1 ] || usage; self_test; exit $? ;;
  --fix)
    case "$#:${2:-}" in
      1:) python3 scripts/ci/gate_tools.py bootstrap ;;
      2:--online) python3 scripts/ci/gate_tools.py bootstrap --online ;;
      *) usage ;;
    esac
    exit $?
    ;;
  --require)
    [ "$#" -eq 2 ] || usage
    python3 scripts/ci/gate_tools.py require --gate "$2" || {
      echo "group '$2' cannot run here; scripts/ci/doctor.sh names the missing tools" >&2
      exit 1
    }
    ;;
  *) usage ;;
esac
