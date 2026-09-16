#!/usr/bin/env bash
# Report which local prerequisites are present, and which check group each
# missing one costs.
#
# There is no prerequisite document in this repository, and a prose one would
# rot: the gates are what actually decide which tools are needed. So this names
# every external tool a group runs, says whether it is here, and — when it is
# not — says exactly which group stops working and how to get it. It installs
# nothing and changes nothing.
#
# Exit status is zero whenever the report itself is complete. A missing tool is
# a fact about this machine, not a failure of the report; `--require <group>`
# asks the opposite question ("can I run that group right now?") and fails when
# the answer is no.
set -uo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_provider_environment.sh
awaken_append_repo_tools

# One row per tool: <tool>|<groups it gates>|<how to get it>
# `docker` appears once although several groups need it; the groups column is
# the whole truth about what its absence costs.
readonly TOOLS=(
  "cargo|rust api formal reliability|https://rustup.rs"
  "cargo-nextest|rust|cargo install cargo-nextest --locked"
  "cargo-public-api|api|cargo install cargo-public-api --locked"
  "rustup|rust api formal|https://rustup.rs"
  "node|static e2e frontend contracts|https://nodejs.org (this repo pins it through nvm)"
  "pnpm|static e2e frontend|corepack enable pnpm"
  "npm|static e2e|ships with node"
  "python3|static docs formal contracts|your distribution's python3"
  "docker|postgres kubernetes k3d sandbox e2e|https://docs.docker.com/engine/install/"
  "podman|sandbox|your distribution's podman"
  "bwrap|sandbox|your distribution's bubblewrap"
  "k3d|k3d kubernetes|https://k3d.io"
  "kubectl|k3d kubernetes|https://kubernetes.io/docs/tasks/tools/"
  "kani|formal|scripts/ci/bootstrap_kani.sh"
  "z3|formal|bundled with tlapm under \$HOME/.local/share/awaken-formal"
  "tlapm|formal|scripts/ci/check_formal.sh reports its expected location"
  "cmake|formal (only to rebuild the CBMC backend)|your distribution's cmake"
  "ninja|formal (only to rebuild the CBMC backend)|your distribution's ninja-build"
)

usage() {
  echo "usage: scripts/ci/doctor.sh [--require <group>] [--self-test]" >&2
  exit 2
}

tool_present() { command -v "$1" >/dev/null 2>&1; }

# Groups a named tool gates, as a space-separated list; empty when unknown.
tool_groups() {
  local row
  for row in "${TOOLS[@]}"; do
    [ "${row%%|*}" = "$1" ] || continue
    row="${row#*|}"
    printf '%s' "${row%%|*}"
    return 0
  done
  return 1
}

# Every group named by a tool this machine does not have.
blocked_groups() {
  local row name groups blocked=()
  for row in "${TOOLS[@]}"; do
    name="${row%%|*}"
    tool_present "$name" && continue
    groups="${row#*|}"; groups="${groups%%|*}"
    # A parenthesised qualifier documents a conditional cost; the group name is
    # the first word, and that is what a caller can act on.
    blocked+=("${groups%% (*}")
  done
  printf '%s\n' "${blocked[@]:-}" | tr ' ' '\n' | grep -v '^$' | sort -u
}

report() {
  local row name groups hint missing=0
  printf '%-18s %-8s %s\n' TOOL STATUS "GROUPS IT GATES"
  for row in "${TOOLS[@]}"; do
    name="${row%%|*}"
    groups="${row#*|}"; hint="${groups#*|}"; groups="${groups%%|*}"
    if tool_present "$name"; then
      printf '%-18s %-8s %s\n' "$name" "present" "$groups"
    else
      missing=$((missing + 1))
      printf '%-18s %-8s %s\n' "$name" "ABSENT" "$groups"
      printf '%-18s %-8s -> %s\n' "" "" "$hint"
    fi
  done
  echo ""
  # Only the findings belong in the report; the checker's own self-test line is
  # evidence for the checker, not for this machine.
  python3 scripts/ci/check_node_dependencies.py 2>&1 |
    sed -n 's/^ERROR: /node dependencies: /p'
  if [ "$missing" -eq 0 ]; then
    echo "every group's external tools are present on this machine."
  else
    echo "$missing tool(s) absent; the groups above cannot run until they are installed."
    echo "run scripts/ci/check-all.sh --list-groups to see every group."
  fi
}

self_test() {
  # | Rule | cause                                   | effect                        |
  # | T1   | a tool row names groups                 | tool_groups returns them      |
  # | T2   | an unknown tool                         | tool_groups fails             |
  # | T3   | a row carries a parenthesised qualifier | the group name survives alone |
  # | T4   | every row is well formed                | three fields, no empty field  |
  local row name groups hint
  [ "$(tool_groups docker)" = "postgres kubernetes k3d sandbox e2e" ] || {
    echo "T1 a known tool must report the groups it gates" >&2
    return 1
  }
  if tool_groups definitely-not-a-tool >/dev/null 2>&1; then
    echo "T2 an unknown tool must not report groups" >&2
    return 1
  fi
  groups="$(tool_groups cmake)"
  [ "${groups%% (*}" = "formal" ] || { echo "T3 a qualifier must not swallow the group" >&2; return 1; }
  for row in "${TOOLS[@]}"; do
    name="${row%%|*}"
    groups="${row#*|}"; hint="${groups#*|}"; groups="${groups%%|*}"
    if [ -z "$name" ] || [ -z "$groups" ] || [ -z "$hint" ] || [ "$hint" = "$row" ]; then
      echo "T4 malformed tool row: $row" >&2
      return 1
    fi
  done
  echo "doctor self-test: every tool row names a group and a way to get it"
}

case "${1:-}" in
  "") report ;;
  --self-test) [ "$#" -eq 1 ] || usage; self_test; exit $? ;;
  --require)
    [ "$#" -eq 2 ] || usage
    if blocked_groups | grep -qx "$2"; then
      echo "group '$2' cannot run here; scripts/ci/doctor.sh names the missing tools" >&2
      exit 1
    fi
    echo "group '$2' has every external tool it needs"
    ;;
  *) usage ;;
esac
