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
# one of its readers. It installs nothing and changes nothing.
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
  echo "usage: scripts/ci/doctor.sh [--require <group>] [--self-test]" >&2
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
  echo "run scripts/ci/check-all.sh --list-groups to see every group."
  echo "run scripts/ci/gate_tools.py require --gate <group> to precheck one group."
  echo "run scripts/ci/reclaim_build_space.py to see every reclaimable path, --apply to free them."
}

self_test() {
  # | Rule | cause                                    | effect                        |
  # | T1   | a tool row names groups                  | tool_groups returns them      |
  # | T2   | an unknown tool                          | tool_groups fails             |
  # | T3   | a row carries a conditional qualifier    | the group name survives alone |
  # | T4   | every row is well formed                 | three fields, no empty field  |
  # | T5   | the manifest is this report's only table | the reader's own proof passes |
  # | T6   | the report also names disk headroom      | the reclamation proof passes  |
  local row name groups hint
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
    echo "T6 the build-space report cannot decide what is safe to reclaim" >&2
    return 1
  }
  echo "doctor self-test: every tool row names a group and a way to get it"
}

case "${1:-}" in
  "") report ;;
  --self-test) [ "$#" -eq 1 ] || usage; self_test; exit $? ;;
  --require)
    [ "$#" -eq 2 ] || usage
    python3 scripts/ci/gate_tools.py require --gate "$2" || {
      echo "group '$2' cannot run here; scripts/ci/doctor.sh names the missing tools" >&2
      exit 1
    }
    ;;
  *) usage ;;
esac
