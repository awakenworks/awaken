# Suite table runner for the service-backed groups (pg_tests.sh, check-reliability.sh).
# scripts/ci/service_suites.toml lists every suite of both groups by a stable id.
# A group script runs a row with `suite_run <id>`, or its own step for a row with
# `suite_step <id> <command...>`; an id the table lacks stops the run with 2.
# With --affected, service_test_plan.py decides which ids run; without it every
# id runs, so nothing here changes what a whole group runs.
declare -A SUITE_ROWS=() SUITE_PHASES=() AFFECTED_SUITES=()
SUITE_IDS=()
AFFECTED_ACTIVE=0

# suites_load <group>: read the group's rows (id phase env runner args) once.
suites_load() {
  local rows line
  local -a row
  [ -f scripts/ci/service_suites.toml ] || { echo "scripts/ci/service_suites.toml is missing" >&2; return 2; }
  rows="$(python3 scripts/ci/service_test_plan.py --rows "$1")" || return $?
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    eval "row=($line)"
    SUITE_IDS+=("${row[0]}")
    SUITE_PHASES["${row[0]}"]="${row[1]}"
    SUITE_ROWS["${row[0]}"]="$line"
  done <<< "$rows"
}

# affected_suites_load <group> <base>: plan once, print the plan, keep its verdicts.
affected_suites_load() {
  local group="$1" base="$2" plan verdict key _rest
  if [ "${AWAKEN_BUILD_WORKLOAD_KIND:-}" = qualification ]; then
    echo "$group: --affected refused: a qualification run is never narrowed" >&2
    return 2
  fi
  plan="$(python3 scripts/ci/service_test_plan.py --group "$group" --base "$base" --explain)" || return $?
  printf '%s\n' "$plan" | sed "s/^/$group affected: /"
  while read -r verdict key _rest; do
    case "$verdict" in run|skip) AFFECTED_SUITES["$key"]="$verdict" ;; esac
  done <<< "$plan"
  AFFECTED_ACTIVE=1
}

# suite_skipped <id>: 0 = skip (reported), 1 = run. An id outside the table, or
# one the affected plan did not decide, is a hard error, never a silent skip.
suite_skipped() {
  if [ -z "${SUITE_ROWS[$1]+set}" ]; then
    echo "rule: every suite of a service group is a row of scripts/ci/service_suites.toml" >&2
    echo "  $1 is not a row; fix: add the row (id, phase, packages) or use an id the table names" >&2
    exit 2
  fi
  [ "$AFFECTED_ACTIVE" -eq 1 ] || return 1
  case "${AFFECTED_SUITES[$1]:-}" in
    run) return 1 ;;
    skip) echo "↷ $1 skipped: the change does not reach it"; return 0 ;;
  esac
  echo "rule: every suite of an affected group has a decision" >&2
  echo "  $1 has none; fix: run service_test_plan.py --group <group> --base <base> --explain" >&2
  exit 2
}

# suite_run <id>: run one row with its runner, arguments and environment.
suite_run() {
  if suite_skipped "$1"; then return 0; fi
  local -a row pairs
  local pair
  eval "row=(${SUITE_ROWS[$1]})"
  if [ "${row[3]}" = step ]; then
    echo "$1 is a step of its group script; run it with suite_step $1 <command...>" >&2
    exit 2
  fi
  local -x AWAKEN_SERVICE_SUITE="$1"
  read -r -a pairs <<< "${row[2]}"
  for pair in "${pairs[@]}"; do local -x "$pair"; done
  "${row[3]}" "${row[@]:4}"
}

# suite_step <id> <command...>: run the group script's own command for a step row.
suite_step() {
  if suite_skipped "$1"; then return 0; fi
  case "${SUITE_ROWS[$1]}" in
    *" step") ;;
    *) echo "$1 has args in scripts/ci/service_suites.toml; run it with suite_run $1" >&2; exit 2 ;;
  esac
  local -x AWAKEN_SERVICE_SUITE="$1"
  shift
  "$@"
}

# suite_ids <phase>: the ids of one phase, in table order.
suite_ids() {
  local id
  for id in "${SUITE_IDS[@]}"; do
    [ "${SUITE_PHASES[$id]}" != "$1" ] || printf '%s\n' "$id"
  done
}

# suites_selected <phase...>: 0 when the run is not narrowed or a row of one of
# the phases is selected. Substrate and tool set-up ask this before starting.
suites_selected() {
  [ "$AFFECTED_ACTIVE" -eq 1 ] || return 0
  local id phase
  for id in "${SUITE_IDS[@]}"; do
    for phase in "$@"; do
      if [ "${SUITE_PHASES[$id]}" = "$phase" ] && [ "${AFFECTED_SUITES[$id]:-}" = run ]; then return 0; fi
    done
  done
  return 1
}
