# Affected selection for the service-backed groups (pg_tests.sh, check-reliability.sh).
# scripts/ci/service_test_plan.py decides which suites a change can reach; these
# functions only look its decision up by the line that starts each suite. Without
# --affected nothing here changes what a group runs.
declare -A AFFECTED_SUITES=()
AFFECTED_ACTIVE=0

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

# affected_skip_suite <file:line>: 0 = skip (reported), 1 = run. A suite line the
# planner did not map is a hard error, never a silent skip.
affected_skip_suite() {
  [ "$AFFECTED_ACTIVE" -eq 1 ] || return 1
  case "${AFFECTED_SUITES[$1]:-}" in
    run) return 1 ;;
    skip) echo "↷ $1 skipped: the change does not reach its packages"; return 0 ;;
  esac
  echo "rule: every suite of an affected group maps to the packages it tests" >&2
  echo "  $1 has no affected decision; fix: run service_test_plan.py --group <group> --base <base> --explain" >&2
  exit 2
}

# affected_skip_stage <file:line>: like affected_skip_suite, but a line the planner
# does not list is substrate work (set-up, cleanup, a wrapper's own call) and runs.
affected_skip_stage() {
  [ "$AFFECTED_ACTIVE" -eq 1 ] && [ "${AFFECTED_SUITES[$1]:-}" = skip ] || return 1
  echo "↷ $1 skipped: the change does not reach its packages"
}

# affected_suite <command...>: run one suite command unless the planner skipped it.
affected_suite() {
  if affected_skip_suite "${BASH_SOURCE[1]##*/}:${BASH_LINENO[0]}"; then return 0; fi
  "$@"
}

# affected_selected_after <line>: 0 when a selected suite starts after <line>, or
# when the run is not narrowed. Substrate set-up asks this before it starts.
affected_selected_after() {
  [ "$AFFECTED_ACTIVE" -eq 1 ] || return 0
  local key
  for key in "${!AFFECTED_SUITES[@]}"; do
    if [ "${AFFECTED_SUITES[$key]}" = run ] && [ "${key##*:}" -gt "$1" ]; then return 0; fi
  done
  return 1
}
