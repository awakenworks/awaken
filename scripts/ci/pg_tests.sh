#!/usr/bin/env bash
# Run durable backend suites against ephemeral PostgreSQL and S3-compatible
# object storage substrates.
#
# Half of awaken's distributed-correctness tests (durable dispatch, fencing,
# cross-node failover, store conformance) only exercise real behaviour on the
# Postgres backend; without a database they self-skip (`schema_pool` returns None),
# so a database-less CI reports them "green" while never running them. This script
# stands up a throwaway Postgres, points the tests at it via AWAKEN_TEST_DATABASE_URL,
# runs them, and tears the container down — closing the "false green" gap (matrix P0).
#
# By default Docker absence is reported as an explicit local skip. The release
# gate passes ``--require-docker`` so the same condition fails instead of
# producing a false green.
#
# Usage: scripts/ci/pg_tests.sh [--affected <base>] [--require-docker|--backend-parity|--work-store|--http|--critical|--storage|--storage-observations|--resources|--self-test]
# --backend-parity uses a caller-owned live URL and runs only the named matrix.
# --work-store uses a caller-owned live URL and requires the complete Work suite.
# --http requires the four live transport targets, including native failover.
# Every suite is a row of scripts/ci/service_suites.toml, run here by id.
# --affected <base> runs only the rows the change since <base> can reach
# (scripts/ci/service_test_plan.py), and starts no substrate when none is.
set -uo pipefail
# Keep phase diagnostics visible even when a command suppresses probe output.
# This descriptor belongs to this runner; never log command arguments/secrets.
exec 3>&2
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"
source scripts/ci/_postgres_recovery_root.sh
source scripts/ci/_deadline.sh
source scripts/ci/_rust_test_execution.sh
source scripts/ci/_affected_suites.sh
suites_load postgres || exit $?

require_docker=0
RECOVERY_PARENT=$(postgres_recovery_parent) || exit 2
PG_OPERATION_TIMEOUT_SECONDS="${AWAKEN_PG_OPERATION_TIMEOUT_SECONDS:-30}"
PG_READY_TIMEOUT_SECONDS="${AWAKEN_PG_READY_TIMEOUT_SECONDS:-90}"
PG_TEST_TIMEOUT_SECONDS="${AWAKEN_PG_TEST_TIMEOUT_SECONDS:-900}"
PG_RECOVERY_TIMEOUT_SECONDS="${AWAKEN_PG_RECOVERY_TIMEOUT_SECONDS:-900}"
# Per-command deadlines bound one command. They do not bound the gate: 47 Cargo
# suites at 900s each is eleven hours of "still running", which is how a gate
# once held CI for eight. The budget below is the whole gate's wall clock, and
# it is checked before every execution stage rather than by a second timer, so the failure
# names the stage that ran out instead of killing an unrelated child.
# Owned diagnostics and cleanup retain their operation deadlines after execution expires.
# Time an admitted Cargo stage spends queued for the host build admission is not
# spent from it (awaken_budget_* in _deadline.sh; nothing with the older host tool).
PG_GATE_BUDGET_SECONDS="${AWAKEN_PG_GATE_BUDGET_SECONDS:-5400}"
PG_GATE_STARTED=$SECONDS

pg_gate_elapsed() {
  local elapsed=$((SECONDS - PG_GATE_STARTED - $(awaken_budget_queued_seconds)))
  [ "$elapsed" -ge 0 ] || elapsed=0
  printf '%s\n' "$elapsed"
}

positive_seconds() {
  [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

pg_run_with_deadline() {
  local stage="$1" seconds="$2"
  shift 2
  positive_seconds "$seconds" || {
    echo "invalid PostgreSQL deadline for $stage: $seconds" >&2
    return 2
  }
  local elapsed
  elapsed=$(pg_gate_elapsed)
  if [[ "$stage" != cleanup-* && "$stage" != diagnostic-* ]] && [ "$elapsed" -ge "$PG_GATE_BUDGET_SECONDS" ]; then
    echo "PostgreSQL gate exhausted its ${PG_GATE_BUDGET_SECONDS}s budget before $stage" >&2
    echo "    (${elapsed}s executing, $((SECONDS - PG_GATE_STARTED))s wall; raise AWAKEN_PG_GATE_BUDGET_SECONDS only with a reason)" >&2
    return 124
  fi
  # A stage admitted just before the gate deadline may consume only the
  # remaining gate budget. Owned cleanup keeps its independent operation bound.
  if [[ "$stage" != cleanup-* && "$stage" != diagnostic-* ]]; then
    local remaining=$((PG_GATE_BUDGET_SECONDS - elapsed))
    [ "$seconds" -le "$remaining" ] || seconds=$remaining
  fi
  local started=$SECONDS
  printf 'postgres stage=%s event=start budget_seconds=%s gate_elapsed_seconds=%s\n' \
    "$stage" "$seconds" "$elapsed" >&3
  run_with_deadline "$seconds" "$@"
  local result=$?
  printf 'postgres stage=%s event=finish status=%s duration_seconds=%s\n' \
    "$stage" "$result" "$((SECONDS - started))" >&3
  if [ "$result" -eq 124 ]; then
    echo "PostgreSQL stage timed out: $stage (${seconds}s)" >&2
  fi
  return "$result"
}

pg_test() {
  pg_run_with_deadline "test:${4:-command}" "$PG_TEST_TIMEOUT_SECONDS" "$@"
}

pg_required_test() {
  local remaining=$((PG_GATE_BUDGET_SECONDS - $(pg_gate_elapsed)))
  [ "$remaining" -gt 0 ] || { echo "PostgreSQL gate budget exhausted before required tests" >&2; return 124; }
  [ "$remaining" -le "$PG_TEST_TIMEOUT_SECONDS" ] || remaining=$PG_TEST_TIMEOUT_SECONDS
  AWAKEN_REQUIRED_TEST_TIMEOUT_SECONDS="$remaining" run_required_rust_tests "$@"
}

# pg_phase <phase>: run the table rows of one phase in order. A phase of several
# rows composes their failures (record_failure keeps the first status); a phase of
# one row returns its status to the caller.
pg_phase() {
  local -a ids
  local id
  mapfile -t ids < <(suite_ids "$1")
  [ "${#ids[@]}" -gt 0 ] || { echo "scripts/ci/service_suites.toml has no row of phase $1" >&2; return 2; }
  if [ "${#ids[@]}" -eq 1 ]; then
    suite_run "${ids[0]}"
    return
  fi
  for id in "${ids[@]}"; do
    suite_run "$id" || record_failure "$?"
  done
}

checked_backend_parity() {
  python3 scripts/ci/_native_postgres_gap_policy.py --check || return "$?"
  python3 scripts/ci/_native_work_target_coverage.py --check || return "$?"
  backend_parity
}

backend_parity() {
  pg_phase backend_parity
}

checked_work_store_evidence() {
  python3 scripts/ci/_native_work_target_coverage.py --check || return "$?"
  work_store_evidence
}

work_store_evidence() {
  pg_phase work_store_evidence
}

http_transport_evidence() {
  pg_phase http_transport_evidence
}

critical_durability_evidence() {
  pg_phase critical_durability_evidence
}

storage_observation_evidence() {
  pg_phase storage_observation_evidence
}

resources_evidence() {
  pg_phase resources_evidence
}

storage_evidence() {
  pg_phase storage_evidence
}

published_port() {
  local binding="$1"
  local port="${binding##*:}"
  if [[ "$port" =~ ^[0-9]+$ ]] && [ "$port" -ge 1 ] && [ "$port" -le 65535 ]; then
    printf '%s\n' "$port"
    return 0
  fi
  return 1
}

# Result-composition decision table: F1 all stages succeed => pass; F2 one
# stage fails => retain that status; F3 later ordinary failures keep the first
# status during the evidence sweep; F4 EXIT cleanup retains a nonzero command
# status, but cannot turn failed cleanup into a successful gate. F5 resource
# refusal ends the sweep with 75 and the same cleanup.
first_failure=0
record_failure() {
  local failure="$1"
  if [ "$first_failure" -eq 0 ]; then
    first_failure="$failure"
  fi
  if [ "$failure" -eq 75 ]; then
    echo "PostgreSQL resource qualification unavailable; remaining suites were not started" >&2
    exit 75
  fi
}

pg_cleanup_fixture_containers() {
  local owner="$1" identifiers identifier
  [[ "$owner" =~ ^[a-f0-9]{32}$ ]] || return 2
  identifiers="$(pg_run_with_deadline cleanup-discover "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker ps --all --quiet --no-trunc --filter "label=io.awaken.ci.postgres-run=$owner")" || return $?
  [ -n "$identifiers" ] || return 0
  local containers=()
  while IFS= read -r identifier; do
    # Native Windows CLIs can emit CRLF. Remove exactly that line terminator;
    # embedded/additional CR or any other non-hex byte remains a hard refusal.
    identifier="${identifier%$'\r'}"
    [[ "$identifier" =~ ^[a-f0-9]{64}$ ]] || {
      echo "refusing malformed owned PostgreSQL container identity" >&2
      return 2
    }
    containers+=("$identifier")
  done <<< "$identifiers"
  # Pin full IDs, not names that another invocation could later reuse. Docker's
  # volume flag removes anonymous image volumes; bind mounts remain caller-owned.
  pg_run_with_deadline cleanup-remove "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker rm --force --volumes "${containers[@]}" >/dev/null
}

finish() {
  local result=$? cleanup_status=0
  trap - EXIT
  if [ "$result" -ne 0 ]; then
    [ "$POSTGRES_CREATE_ATTEMPTED" -eq 0 ] || startup_diagnostics "$NAME"
    [ "$OBJECT_CREATE_ATTEMPTED" -eq 0 ] || startup_diagnostics "$OBJECT_NAME"
  fi
  cleanup || cleanup_status=$?
  awaken_budget_close
  [ "$result" -ne 0 ] || result=$cleanup_status
  exit "$result"
}

self_test() {
  # Startup cause/effect decision table:
  # C1 an explicit port is requested; C2 Docker creates the container; C3 the
  # dynamically published binding is a valid TCP port.
  #
  # | Rule | C1 | C2 | C3 | Effect |
  # | P1   | T  | T  | -  | use exact requested port |
  # | P2   | F  | T  | T  | discover Docker-assigned free port |
  # | P3   | *  | F  | -  | fail immediately; never enter readiness loop |
  # | P4   | F  | T  | F  | fail before constructing database URL |
  test "$(published_port '127.0.0.1:49152')" = "49152" || return 1
  ! published_port 'invalid-binding' >/dev/null || return 1
  ! published_port '127.0.0.1:0' >/dev/null || return 1
  # Deadline cause/effect table: C6 positive stage budget + command return =>
  # E6 preserve the exact return; C7 malformed/zero budget => E7 reject before
  # command; C8 timeout status => E8 remain 124 and identify the stage. The
  # shared deadline helper owns process-group termination, so this runner does
  # not implement a second timer.
  local deadline_calls=0 deadline_status=0
  run_with_deadline() {
    local _seconds="$1"
    shift
    deadline_calls=$((deadline_calls + 1))
    "$@"
  }
  pg_run_with_deadline fixture 1 bash -c 'exit 0' || return 1
  pg_run_with_deadline fixture 1 bash -c 'exit 23' || deadline_status=$?
  [ "$deadline_status" -eq 23 ] || return 1
  ! pg_run_with_deadline fixture 0 bash -c 'exit 0' || return 1
  [ "$deadline_calls" -eq 2 ] || return 1
  # P19 source scripts need no executable bit (the S3 owner is Git100644).
  # Cause: a non-executable script receives a phase and a spaced argument.
  # Effect: the same native shell adapter used by the live S3 phases forwards
  # both arguments and exit23 through the existing deadline, without chmod or
  # a second shell-selection policy. The adapter's own argv/Windows tests own
  # nested shebang and quoting behavior.
  local object_script_fixture object_script_status=0
  object_script_fixture=$(mktemp -d) || return 1
  printf '%s\n' '#!/usr/bin/env bash' \
    '[ "$1" = --verify ] && [ "$2" = "spaced argument" ] || exit 17' \
    'printf routed > "$3"' 'exit 23' > "$object_script_fixture/phase.sh"
  chmod 0644 "$object_script_fixture/phase.sh" || return 1
  pg_run_with_deadline object-script-fixture 1 python3 scripts/ci/_native_shell.py \
    "$object_script_fixture/phase.sh" --verify 'spaced argument' \
    "$object_script_fixture/observed" || object_script_status=$?
  if [ "$object_script_status" -ne 23 ] || \
    [ "$(cat "$object_script_fixture/observed" 2>/dev/null)" != routed ]; then
    rm -rf -- "$object_script_fixture"
    echo "Postgres object stages lost native script arguments or status" >&2
    return 1
  fi
  rm -rf -- "$object_script_fixture"
  # F2/F3 ordinary failures retain the first outcome. F5 a later resource
  # refusal exits 75 before dependent work; the same EXIT hook still executes.
  (
    record_failure 23
    record_failure 31
    [ "$first_failure" -eq 23 ]
  ) || return 1
  local failure_fixture refusal_status=0
  failure_fixture=$(mktemp -d) || return 1
  (
    trap 'touch "$failure_fixture/cleaned"' EXIT
    record_failure 23
    record_failure 75
    touch "$failure_fixture/dependent"
  ) || refusal_status=$?
  if [ "$refusal_status" -ne 75 ] || [ ! -f "$failure_fixture/cleaned" ] \
    || [ -e "$failure_fixture/dependent" ]; then
    rm -rf -- "$failure_fixture"
    return 1
  fi
  rm -rf -- "$failure_fixture"
  # C9 exact ownership + zero/two containers; C10 discovery/removal failures;
  # C11 malformed identity or missing ownership; C12 execution budget spent.
  # Only discovered full IDs may be deleted, including anonymous image volumes.
  # Refusal must reach discovery when expected, never pass from an earlier error.
  local lifecycle_case
  for lifecycle_case in empty owned owned-crlf discover-fails malformed malformed-crlf embedded-cr remove-fails expired; do
    (
      local fixture_log fixture_owner=11111111111111111111111111111111
      local fixture_first fixture_second fixture_status=0 fixture_expected=0
      fixture_log=$(mktemp) || exit 1
      trap 'rm -f -- "$fixture_log"' EXIT
      printf -v fixture_first '%064d' 1
      printf -v fixture_second '%064d' 2
      docker() {
        printf '%s\n' "$*" >> "$fixture_log"
        case "$1" in
          ps)
            case "$lifecycle_case" in
              empty) ;;
              discover-fails) return 23 ;;
              malformed) printf '%s\n' "$fixture_first" 'not-an-id' ;;
              owned-crlf) printf '%s\r\n' "$fixture_first" "$fixture_second" ;;
              malformed-crlf) printf '%s\r\n' "$fixture_first" 'not-an-id' ;;
              embedded-cr) printf '%s\n' "$fixture_first" "${fixture_second:0:32}"$'\r'"${fixture_second:32}" ;;
              *) printf '%s\n' "$fixture_first" "$fixture_second" ;;
            esac ;;
          rm) [ "$lifecycle_case" != remove-fails ] || return 31 ;;
          *) return 2 ;;
        esac
      }
      [ "$lifecycle_case" != expired ] || PG_GATE_STARTED=$((SECONDS - PG_GATE_BUDGET_SECONDS - 1))
      pg_cleanup_fixture_containers "$fixture_owner" || fixture_status=$?
      case "$lifecycle_case" in discover-fails) fixture_expected=23 ;; malformed|malformed-crlf|embedded-cr) fixture_expected=2 ;; remove-fails) fixture_expected=31 ;; esac
      [ "$fixture_status" -eq "$fixture_expected" ] || exit 1
      local fixture_calls=()
      while IFS= read -r fixture_call; do fixture_calls+=("$fixture_call"); done < "$fixture_log"
      [[ " ${fixture_calls[0]} " == *" --filter label=io.awaken.ci.postgres-run=$fixture_owner "* ]] || exit 1
      case "$lifecycle_case" in
        owned|owned-crlf|remove-fails|expired)
          [ "${#fixture_calls[@]}" -eq 2 ] || exit 1
          [ "${fixture_calls[1]}" = "rm --force --volumes $fixture_first $fixture_second" ] || exit 1 ;;
        *) [ "${#fixture_calls[@]}" -eq 1 ] || exit 1 ;;
      esac
      : > "$fixture_log"
      fixture_status=0
      pg_cleanup_fixture_containers '' || fixture_status=$?
      [ "$fixture_status" -eq 2 ] && [ ! -s "$fixture_log" ] || exit 1
    ) || return 1
  done
  # A cleanup failure must reject an otherwise successful gate, while retaining
  # the original command/resource status when that command had already failed.
  local fixture_prior fixture_cleanup fixture_expected fixture_actual
  for lifecycle_case in '0 0 0' '0 31 31' '23 0 23' '23 31 23' '75 31 75'; do
    read -r fixture_prior fixture_cleanup fixture_expected <<< "$lifecycle_case"
    fixture_actual=0
    (
      POSTGRES_CREATE_ATTEMPTED=0 OBJECT_CREATE_ATTEMPTED=0
      cleanup() { return "$fixture_cleanup"; }
      prior_outcome() { return "$fixture_prior"; }
      prior_outcome
      finish
    ) || fixture_actual=$?
    [ "$fixture_actual" -eq "$fixture_expected" ] || return 1
  done
  # Gate-completeness rules: P9 every Cargo suite enters pg_test and therefore
  # has a per-command deadline; P10 the recovery drill has its independent
  # stage deadline. A bare command is a false-green/hang path, not compatibility.
  # The old pattern was anchored at column zero and knew only `cargo test`, so
  # an indented line, an environment prefix or `cargo nextest run` walked past
  # it unbounded.
  if grep -Eq '^[[:space:]]*(([A-Z][A-Z0-9_]*=[^[:space:]]*[[:space:]]+)*)cargo[[:space:]]+(test|nextest[[:space:]]+run)\b' "$0"; then
    echo "Postgres gate contains an unbounded Cargo test" >&2
    grep -nE '^[[:space:]]*(([A-Z][A-Z0-9_]*=[^[:space:]]*[[:space:]]+)*)cargo[[:space:]]+(test|nextest[[:space:]]+run)\b' "$0" >&2
    return 1
  fi
  # P14 the whole gate has a wall-clock budget, and a stage that starts after it
  # is exhausted is refused rather than begun. Per-command deadlines alone let
  # 47 suites take eleven hours while every single one stayed inside its bound.
  # The stub must consume the deadline argument exactly as the real helper does.
  # A stub that forwards it ran `1 bash -c ...`, got "command not found", and
  # made this probe pass whether or not the budget was enforced -- the same
  # shape of vacuous evidence this whole file exists to refuse.
  (
    PG_GATE_STARTED=$((SECONDS - PG_GATE_BUDGET_SECONDS - 1))
    run_with_deadline() { local _seconds="$1"; shift; "$@"; }
    ! pg_run_with_deadline exhausted 1 bash -c 'exit 0'
  ) || {
    echo "Postgres gate started a stage after its whole-gate budget was spent" >&2
    return 1
  }
  # ... and the same stage inside the budget must still run, so the rule refuses
  # an exhausted budget rather than refusing every stage.
  (
    PG_GATE_STARTED=$SECONDS
    run_with_deadline() { local _seconds="$1"; shift; "$@"; }
    pg_run_with_deadline within-budget 1 bash -c 'exit 0'
  ) || {
    echo "Postgres gate refused a stage that was inside its whole-gate budget" >&2
    return 1
  }
  # P17 a stage that fits its own timeout can still overrun the remaining
  # whole-gate budget. Observe the actual argument passed to the deadline
  # owner, and retain the command failure. Cleanup remains independently
  # bounded even when the execution budget is almost spent.
  (
    PG_GATE_BUDGET_SECONDS=60
    PG_GATE_STARTED=$((SECONDS - 50))
    local observed=0 result=0
    run_with_deadline() { observed="$1"; shift; "$@"; }
    pg_run_with_deadline remaining-budget 30 bash -c 'exit 23' || result=$?
    [ "$result" -eq 23 ] && [ "$observed" -gt 0 ] && [ "$observed" -le 10 ] || exit 1
    result=0
    pg_run_with_deadline cleanup-budget 30 bash -c 'exit 31' || result=$?
    [ "$result" -eq 31 ] && [ "$observed" -eq 30 ]
  ) || {
    echo "PostgreSQL stage deadline must fit the remaining gate budget and preserve cleanup" >&2
    return 1
  }
  # P19 time admitted stages spent queued for host build admission is not
  # spent from the gate budget: 61s of wall with 30s queued leaves 29s.
  (
    PG_GATE_BUDGET_SECONDS=60
    PG_GATE_STARTED=$((SECONDS - 61))
    local observed=0 result=0
    awaken_budget_open || exit 1
    trap awaken_budget_close EXIT
    python3 -c 'import json, sys, time; now = time.time()
json.dump({"requested_at_unix": now - 40, "granted_at_unix": now - 10, "queue_bound_seconds": 600},
          open(sys.argv[1] + "/fixture.json", "w"))' "$awaken_budget_ledger" || exit 1
    run_with_deadline() { observed="$1"; shift; "$@"; }
    pg_run_with_deadline queued-budget 300 bash -c 'exit 23' || result=$?
    [ "$result" -eq 23 ] && [ "$observed" -ge 25 ] && [ "$observed" -le 29 ]
  ) || {
    echo "PostgreSQL gate budget must leave out time spent queued for host build admission" >&2
    return 1
  }
  # P18 exhaustion refuses new tests, but cannot suppress bounded failure
  # diagnostics before teardown. Preserve their exact command status too.
  (
    PG_GATE_BUDGET_SECONDS=60
    PG_GATE_STARTED=$((SECONDS - 61))
    local observed=0 result=0
    run_with_deadline() { observed="$1"; shift; "$@"; }
    pg_run_with_deadline diagnostic-fixture 30 bash -c 'exit 23' || result=$?
    [ "$result" -eq 23 ] && [ "$observed" -eq 30 ]
  ) || {
    echo "PostgreSQL gate must retain bounded diagnostics after its execution budget" >&2
    return 1
  }
  grep -Fq 'pg_run_with_deadline recovery-drill "$PG_RECOVERY_TIMEOUT_SECONDS"' "$0" || {
    echo "Postgres recovery drill has no stage deadline" >&2
    return 1
  }
  # Cleanup cause/effect rules: C4 only the exact mktemp-owned six-character
  # suffix is present => E4 cleanup may remove it. C5 empty, caller-owned, or
  # structurally broader paths => E5 cleanup refuses them. This keeps a failed
  # allocation from falling back to an inherited AWAKEN_RECOVERY_ROOT.
  valid_recovery_root "$RECOVERY_PARENT/awaken-pg-recovery.A1b2C3" || return 1
  ! valid_recovery_root "" || return 1
  ! valid_recovery_root "$RECOVERY_PARENT/caller-owned" || return 1
  ! valid_recovery_root "$RECOVERY_PARENT/awaken-pg-recovery.A1b2C3/child" || return 1
  ! valid_recovery_root "$RECOVERY_PARENT/awaken-pg-recovery.A/../." || return 1
  scripts/ci/postgres_recovery_drill.sh --self-test || return 1
  # P5: every feature-gated Postgres adapter is compiled as such. P6: test
  # targets whose volatile fixtures are isolated behind `test-support` enable
  # that feature explicitly; Cargo refusing to start a target is a red gate,
  # never evidence that the Postgres behavior passed.
  # The suites are rows of scripts/ci/service_suites.toml; each check below reads
  # them as one command line ("[ENV=value] runner args"), as this gate runs them.
  local suite_commands
  suite_commands="$(printf '%s\n' "${SUITE_ROWS[@]}" | sed -E "s/^[^ ]+ [^ ]+ ('' )?//")"
  # P13 the Registry migration and conformance cases use the canonical fixture
  # URL and execute both library and integration targets in the live PG gate.
  grep -Fxq 'pg_test cargo test -p awaken-worker-registry --features test-support --lib --test conformance' <<< "$suite_commands" || {
    echo "Postgres gate must run WorkerRegistry migration and conformance tests" >&2
    return 1
  }
  grep -Fxq 'pg_test cargo test -p awaken-dispatch --features test-support' <<< "$suite_commands" || {
    echo "Postgres gate must run the complete ingress suite with test-support" >&2
    return 1
  }
  # P11 the full gate and the focused Work gate share one required selection,
  # including every named identity/archive witness and test-support. The exact
  # unindented call below belongs to the full gate, not its focused branch.
  grep -Fxq 'checked_work_store_evidence || record_failure "$?"' "$0" || {
    echo "Postgres gate must run the required complete Work suite" >&2
    return 1
  }
  # W1 a selected Work database suite includes ignored cases and all thirteen
  # physical, migration and receipt-decoder outcome floors. W2 the existing runner's failure/refusal status
  # propagates unchanged; no success is minted by the selection wrapper.
  if ! (
    local expected_status observed_status arguments required_case
    pg_required_test() {
      arguments=" $* "
      [[ "$arguments" == *' -p awaken-work-store --features test-support '* ]] || return 2
      [[ "$arguments" == *' -- --include-ignored --test-threads=1 '* ]] || return 2
      [[ "$arguments" == *' --skip environment_revision_restart_child --skip restart::identity_restart_child '* ]] || return 2
      for required_case in \
        schema::identity_upgrade_tests::postgres_both_published_histories_preserve_live_leases_and_fence_deleted_ids \
        schema::ack_upgrade_tests::sqlite_both_published_histories_add_only_known_ack_generations \
        schema::ack_upgrade_tests::postgres_both_published_histories_add_only_known_ack_generations \
        schema::stop_upgrade_tests::sqlite_both_published_histories_preserve_unknown_old_stop_subjects \
        schema::stop_upgrade_tests::postgres_both_published_histories_preserve_unknown_old_stop_subjects \
        mutation::tests::stop_receipts_preserve_absence_and_reject_incomplete_or_ungranted_subjects \
        mutation::tests::stop_receipt_replay_requires_original_owner_and_current_generation \
        postgres_archive_keeps_existing_session_authority_after_reconnect \
        durable::postgres_allocator_work_and_watermark_roll_back_as_one_transaction \
        durable::postgres_independent_pools_allocate_concurrently_without_reset \
        durable::postgres_missing_malformed_and_exhausted_allocators_fail_closed \
        postgres_work_identity_is_not_reused_after_lifecycle_removal \
        restart::postgres_identity_high_watermark_survives_a_native_process_restart; do
        [[ "$arguments" == *" --require-case=$required_case "* ]] || return 2
      done
      return "$expected_status"
    }
    for expected_status in 0 23 75; do
      observed_status=0
      work_store_evidence || observed_status=$?
      [ "$observed_status" -eq "$expected_status" ] || exit 1
    done
  ); then
    echo "Postgres Work selection must retain its cases, ignored execution and runner status" >&2
    return 1
  fi
  # P12 initialization-only socket readiness must not qualify TCP clients.
  grep -Fxq '    docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres -d "$DB" >/dev/null 2>&1; then ready=1; break; fi' "$0" || {
    echo "Postgres gate must wait for the final TCP server" >&2
    return 1
  }
  # P8 the claim/commit guard lives in durable_postgres and its HTTP receipt
  # boundary in active_active_postgres. The removed RuntimeHost filter matched
  # zero tests; it is not a compatibility route or additional test coverage.
  if grep -Eq '^(pg_test )?cargo test -p awaken-session-executor' "$0" <(printf '%s\n' "$suite_commands"); then
    echo "Postgres gate retains the retired RuntimeHost test selector" >&2
    return 1
  fi
  # P9 immutable Pod registration must exercise the real database rather than
  # merely compiling a self-skipping test in the no-database workspace gate.
  # P10 independent predecessor stopping exercises the same exact Registry CAS.
  grep -Fq "pg_required_test -p awaken-dispatch-http --test active_active_postgres --test pod_worker_registration_http --test worker_registration_history_http --test predecessor_stop_http" <<< "$suite_commands" || {
    echo "Postgres gate omits the coordinator-owned active/active transport test" >&2
    return 1
  }
  grep -Fq 'pg_required_test -p awaken-session-store --features test-support --test session_repo_conformance' <<< "$suite_commands" || {
    echo "Postgres gate omits Session and Deployment repository conformance" >&2
    return 1
  }
  grep -Fq 'pg_required_test --locked -p awaken-session-store --features test-support --lib postgres_ ' <<< "$suite_commands" || {
    echo "Postgres gate omits Session history, read-only open, and repository parity" >&2
    return 1
  }
  grep -Fq 'pg_required_test -p awaken-process-startup --locked --lib installation_binding::tests::postgres_preflights_all_targets_then_binds_fresh_or_explicit_legacy' <<< "$suite_commands" || {
    echo "Postgres gate omits exact CLI installation-binding continuity" >&2
    return 1
  }
  # P15 an ignored process-pool test exists + this runner supplies a live URL
  # => execute that exact test under the existing deadline/first-failure owner.
  # Without --ignored the default CLI suite can pass without measuring cap2.
  grep -Fq 'pg_required_test -p awaken-process-startup --locked --lib postgres_pools::tests::live_coordinator_adapters_share_the_process_connection_ceiling' <<< "$suite_commands" || {
    echo "Postgres gate omits live process-pool cardinality and closure" >&2
    return 1
  }
  # P16 live URL + postgres feature + ignored correctness tests -> execute R1-R7
  # in this fixture. The separate performance observation keeps its own opt-in.
  grep -Fq 'pg_required_test --locked -p awaken-sql-storage --features postgres --lib postgres_tests ' <<< "$suite_commands" || {
    echo "Postgres gate omits explicit shared checkout correctness" >&2
    return 1
  }
  grep -Fxq 'pg_required_test -p awaken-coordinator --locked --lib application_access_store::tests::provisioned_postgres_application_access_durability_release_gate --require-case=application_access_store::tests::provisioned_postgres_application_access_durability_release_gate -- --ignored --exact --test-threads=1' <<< "$suite_commands" || {
    echo "Postgres gate omits fail-closed ApplicationAccess durability conformance" >&2
    return 1
  }
  grep -Fq 'pg_required_test -p awaken-model-catalog-store --features postgres,sqlite,test-support' <<< "$suite_commands" || {
    echo "Postgres gate omits the authoritative model-catalog store" >&2
    return 1
  }
  grep -Fq "concurrent_postgres_claim_reclaim_and_restart_preserve_one_exact_authority" <<< "$suite_commands" || {
    echo "Postgres gate omits Environment image-build concurrency and restart fencing" >&2
    return 1
  }
  # C1 independent schema fixtures share one finite PostgreSQL connection
  # budget; C2 each test's explicit race witnesses must remain concurrent.
  # R1 serialize only outer test cases; R2 preserve concurrency inside each.
  grep -Fq 'pg_required_test -p awaken-credential-store --features postgres,sqlite,test-support,sealed-aead --test repo_conformance' <<< "$suite_commands" || {
    echo "Postgres gate omits serialized sealed credential store conformance" >&2
    return 1
  }
  grep -Fq 'pg_required_test -p awaken-data-subject-store --features postgres,test-support' <<< "$suite_commands" || {
    echo "Postgres gate omits postgres,test-support for awaken-data-subject-store" >&2
    return 1
  }
  local crate
  for crate in awaken-admin-config-api; do
    grep -Fq "pg_required_test --locked -p $crate --features postgres" <<< "$suite_commands" || {
      echo "Postgres gate omits --features postgres for $crate" >&2
      return 1
    }
  done
  grep -Fq "pg_required_test --locked -p awaken-file-store --features postgres,sqlite,object-store" <<< "$suite_commands" || {
    echo "Durable backend gate omits the production object-store adapter" >&2
    return 1
  }
  # P20 all four live object phases use that same interpreter under the
  # original per-stage deadline; one stale direct exec would still fail on
  # Linux while native Windows happened to pass. No phase may go unselected.
  local object_phase
  for object_phase in admission conformance seed verify; do
    grep -Fq "pg_run_with_deadline object-$object_phase \"\$PG_TEST_TIMEOUT_SECONDS\" python3 scripts/ci/_native_shell.py scripts/ci/object_store_tests.sh --$object_phase" "$0" || {
      echo "Postgres object phase $object_phase bypasses the native shell owner" >&2
      return 1
    }
  done
  grep -Fq "scripts/ci/object_store_tests.sh --verify" "$0" || {
    echo "Durable backend gate omits object-store credential rotation" >&2
    return 1
  }
  test -f scripts/ci/fixtures/minio-file-store-policy.json || {
    echo "Durable backend gate omits the least-privilege object-store policy" >&2
    return 1
  }
  grep -Fq "pg_required_test --locked -p awaken-resource-persistence" <<< "$suite_commands" || {
    echo "Postgres gate omits the composed Resources restart matrix" >&2
    return 1
  }
  grep -Fq "archive_mode=on" "$0" || {
    echo "Postgres gate omits WAL archiving for physical recovery" >&2
    return 1
  }
  grep -Fq 'scripts/ci/postgres_recovery_drill.sh "$NAME" "$OBJECT_NAME" "$MC_IMAGE"' "$0" || {
    echo "Postgres gate omits the PostgreSQL/object-store recovery drill" >&2
    return 1
  }
  test -x scripts/ci/postgres_recovery_drill.sh || {
    echo "Postgres recovery drill is not executable" >&2
    return 1
  }
  grep -Fq "pg_required_test --locked -p awaken-store-sqlite --features test-support --test failure_atomicity" <<< "$suite_commands" || {
    echo "Durable backend gate omits SQLite crash and injected fault atomicity" >&2
    return 1
  }
  grep -Fq -- '--test dispatch_claim_perf -- --ignored --test-threads=1 --nocapture' <<< "$suite_commands" || {
    echo "Durable backend gate omits explicit ignored scale qualification" >&2
    return 1
  }
}

affected_base=""
pg_arguments=()
while [ "$#" -gt 0 ]; do
  if [ "$1" = --affected ] && [ "$#" -ge 2 ] && [ -n "$2" ] && [ -z "$affected_base" ]; then
    affected_base="$2"
    shift 2
  else
    pg_arguments+=("$1")
    shift
  fi
done
set -- "${pg_arguments[@]}"
[ -z "$affected_base" ] || [ "${1:-}" = "" ] || [ "${1:-}" = --require-docker ] || {
  echo "--affected narrows the full PostgreSQL gate only; it composes with --require-docker alone" >&2; exit 2; }
[ "$#" -le 1 ] || { echo "usage: scripts/ci/pg_tests.sh [--affected <base>] [--require-docker|--backend-parity|--work-store|--http|--critical|--storage|--storage-observations|--resources|--self-test]" >&2; exit 2; }

backend_only=0
work_store_only=0
http_only=0
critical_only=0
storage_only=0
resources_only=0
storage_observations_only=0
case "${1:-}" in
  "") ;;
  --require-docker) require_docker=1 ;;
  --self-test) self_test; exit $? ;;
  --backend-parity) backend_only=1 ;;
  --work-store) work_store_only=1 ;;
  --http) http_only=1 ;;
  --critical) critical_only=1 ;;
  --storage) storage_only=1 ;;
  --resources) resources_only=1 ;;
  --storage-observations) storage_observations_only=1 ;;
  *) echo "usage: scripts/ci/pg_tests.sh [--affected <base>] [--require-docker|--backend-parity|--work-store|--http|--critical|--storage|--storage-observations|--resources|--self-test]" >&2; exit 2 ;;
esac

for deadline in "$PG_OPERATION_TIMEOUT_SECONDS" "$PG_READY_TIMEOUT_SECONDS" \
  "$PG_TEST_TIMEOUT_SECONDS" "$PG_RECOVERY_TIMEOUT_SECONDS" "$PG_GATE_BUDGET_SECONDS"; do
  positive_seconds "$deadline" || {
    echo "PostgreSQL deadlines must be positive integers" >&2
    exit 2
  }
done

if [ "$backend_only" -eq 1 ] || [ "$work_store_only" -eq 1 ] || [ "$http_only" -eq 1 ] || [ "$critical_only" -eq 1 ] || [ "$storage_only" -eq 1 ] || [ "$storage_observations_only" -eq 1 ] || [ "$resources_only" -eq 1 ]; then
  [ "$#" -eq 1 ] && [ -n "${AWAKEN_TEST_DATABASE_URL:-}" ] || {
    echo "focused PostgreSQL tests require an explicit owned fixture URL" >&2; exit 2;
  }
  export AWAKEN_TEST_REQUIRE_POSTGRES=1
  if [ "$resources_only" -eq 1 ]; then
    resources_evidence
  elif [ "$storage_observations_only" -eq 1 ]; then
    storage_observation_evidence
  elif [ "$storage_only" -eq 1 ]; then
    storage_evidence
  elif [ "$critical_only" -eq 1 ]; then
    critical_durability_evidence
  elif [ "$http_only" -eq 1 ]; then
    http_transport_evidence || record_failure "$?"
  elif [ "$work_store_only" -eq 1 ]; then
    checked_work_store_evidence || record_failure "$?"
  else
    checked_backend_parity || record_failure "$?"
  fi
  exit "$first_failure"
fi

if [ -n "$affected_base" ]; then
  affected_suites_load postgres "$affected_base" || exit $?
  if ! suites_selected storage_evidence ingress http_transport_evidence critical_durability_evidence \
    backend_parity object_store resources_evidence work_store_evidence recovery_drill; then
    echo "postgres: no suite is reached by the change since $affected_base; no substrate started"
    exit 0
  fi
fi

if ! command -v docker >/dev/null 2>&1 \
  || ! pg_run_with_deadline docker-info "$PG_OPERATION_TIMEOUT_SECONDS" docker info >/dev/null 2>&1; then
  if [ "$require_docker" -eq 1 ]; then
    echo "✗ Docker is required for Postgres-backed release tests" >&2
    exit 1
  fi
  echo "docker unavailable; skipping Postgres-backed tests (they self-skip without a DB)"
  exit 0
fi

# The substrate this gate starts is declared in scripts/ci/gate_tools.json, so
# an image that is not cached is named here rather than reaching Docker as
# "pull access denied", which reads as a permission problem on a private
# repository. Its volumes need room, so the filesystem is checked too.
python3 scripts/ci/gate_tools.py require --gate postgres || exit 1
python3 scripts/ci/gate_tools.py disk --stage postgres || exit $?

PG_OWNER=$(python3 -c 'import secrets; print(secrets.token_hex(16))') || exit 2
[[ "$PG_OWNER" =~ ^[a-f0-9]{32}$ ]] || exit 2
NAME="awaken-ci-pg-$$-${PG_OWNER:0:12}"
OBJECT_NAME="awaken-ci-object-$$-${PG_OWNER:0:12}"
REQUESTED_PORT="${AWAKEN_CI_PG_PORT:-}"
PASSWORD="ci" # awaken-allow(secret): (throwaway ephemeral container, torn down on exit)
DB="awaken"
POSTGRES_CONTAINER_ENV=("POSTGRES_PASSWORD=$PASSWORD" "POSTGRES_DB=$DB") # awaken-allow(secret):
RECOVERY_ROOT=""
POSTGRES_CREATE_ATTEMPTED=0
OBJECT_CREATE_ATTEMPTED=0

startup_diagnostics() {
  local name="$1"
  # Read only this invocation's attempted container, including lost create
  # responses. Never inspect Config.Env or broaden to other Docker resources.
  pg_run_with_deadline diagnostic-container-state "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker inspect --format 'status={{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}} error={{json .State.Error}}' \
    "$name" >&3 2>&3 || true
  pg_run_with_deadline diagnostic-container-logs "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker logs --tail 40 "$name" >&3 2>&3 || true
}

cleanup() {
  local cleanup_status=0
  pg_cleanup_fixture_containers "$PG_OWNER" || cleanup_status=$?
  if [ "$cleanup_status" -ne 0 ]; then
    echo "PostgreSQL fixture cleanup failed; retaining its owned recovery directory" >&2
    return "$cleanup_status"
  fi
  if valid_recovery_root "$RECOVERY_ROOT"; then
    [ ! -d "$RECOVERY_ROOT" ] || pg_run_with_deadline cleanup-files "$PG_OPERATION_TIMEOUT_SECONDS" \
      rm -rf -- "$RECOVERY_ROOT"
  elif [ -n "$RECOVERY_ROOT" ]; then
    echo "refusing unexpected PostgreSQL recovery cleanup target: $RECOVERY_ROOT" >&2
    return 1
  fi
}
trap finish EXIT
awaken_budget_open || { echo "cannot open the PostgreSQL gate's admission-queue ledger" >&2; exit 1; }

RECOVERY_ROOT=$(mktemp -d "$(postgres_recovery_prefix "$RECOVERY_PARENT")XXXXXX")
valid_recovery_root "$RECOVERY_ROOT" \
  || { echo "mktemp returned an unexpected PostgreSQL recovery path: $RECOVERY_ROOT" >&2; exit 1; }
export AWAKEN_RECOVERY_ROOT="$RECOVERY_ROOT"
RECOVERY_MOUNT_ROOT=$(postgres_recovery_mount_root "$RECOVERY_ROOT") || exit 2
mkdir -p "$RECOVERY_ROOT/base" "$RECOVERY_ROOT/wal"
chmod 0777 "$RECOVERY_ROOT/base" "$RECOVERY_ROOT/wal"

if [ -n "$REQUESTED_PORT" ]; then
  if ! PORT="$(published_port "127.0.0.1:$REQUESTED_PORT")"; then
    echo "✗ AWAKEN_CI_PG_PORT must be an integer from 1 to 65535" >&2
    exit 2
  fi
  PUBLISH="127.0.0.1:$PORT:5432"
  echo "-> starting ephemeral Postgres ($NAME) on requested :$PORT"
else
  PUBLISH="127.0.0.1::5432"
  echo "-> starting ephemeral Postgres ($NAME) on a Docker-assigned free port"
fi
POSTGRES_CREATE_ATTEMPTED=1
if ! pg_run_with_deadline postgres-create "$PG_OPERATION_TIMEOUT_SECONDS" \
  docker run -d --name "$NAME" --label "io.awaken.ci.postgres-run=$PG_OWNER" \
  --tmpfs /var/lib/postgresql/data \
  -e "${POSTGRES_CONTAINER_ENV[0]}" -e "${POSTGRES_CONTAINER_ENV[1]}" \
  -v "$RECOVERY_MOUNT_ROOT/base:/recovery-base" \
  -v "$RECOVERY_MOUNT_ROOT/wal:/wal-archive" \
  -p "$PUBLISH" postgres:16-alpine \
  -c wal_level=replica \
  -c archive_mode=on \
  -c archive_timeout=1s \
  -c "archive_command=test ! -f /wal-archive/%f && cp %p /wal-archive/%f" \
  >/dev/null; then
  echo "✗ Docker could not create the ephemeral Postgres container" >&2
  exit 1
fi

if [ -z "$REQUESTED_PORT" ]; then
  if ! PORT="$(published_port "$(pg_run_with_deadline postgres-port "$PG_OPERATION_TIMEOUT_SECONDS" docker port "$NAME" 5432/tcp)")"; then
    echo "✗ Docker did not publish a valid Postgres TCP port" >&2
    exit 1
  fi
  echo "-> Docker published ephemeral Postgres on :$PORT"
fi

# Wait on TCP: the image first starts a temporary Unix-socket-only server for
# initialization and then restarts it. Socket readiness can falsely greenlight
# clients before the externally reachable server exists (bounded; fail loud).
ready=0
ready_deadline=$((SECONDS + PG_READY_TIMEOUT_SECONDS))
while (( SECONDS < ready_deadline )); do
  if pg_run_with_deadline postgres-ready "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres -d "$DB" >/dev/null 2>&1; then ready=1; break; fi
  sleep 1
done
if [ "$ready" -ne 1 ]; then
  echo "✗ ephemeral Postgres never became ready"
  exit 1
fi

export AWAKEN_TEST_DATABASE_URL="postgres://postgres:$PASSWORD@127.0.0.1:$PORT/$DB" # awaken-allow(secret):
export AWAKEN_TEST_REQUIRE_POSTGRES=1
echo "-> AWAKEN_TEST_DATABASE_URL configured for ephemeral Postgres :$PORT"

# Exact, reviewed tags, served from MinIO's own registry. Docker Hub answers
# `docker pull minio/minio` with "pull access denied ... repository does not
# exist", which reads as a credentials problem but is not one: upstream publishes
# to quay.io, and the Hub repository is gone. A runner that cannot reach quay.io
# may point these at a mirror holding the same digests; the tag stays exact
# either way, because a floating tag would let the object substrate change under
# the recovery drill.
MINIO_IMAGE="${AWAKEN_CI_OBJECT_IMAGE:-quay.io/minio/minio:RELEASE.2024-12-18T13-15-44Z}"
MC_IMAGE="${AWAKEN_CI_OBJECT_CLIENT_IMAGE:-quay.io/minio/mc:RELEASE.2024-11-21T17-21-54Z}"
export FILE_STORE_ACCESS_KEY="awaken-file-store" # awaken-allow(secret): (throwaway fixture identity)
FILE_STORE_INITIAL_SECRET_KEY="awaken-file-store-initial" # awaken-allow(secret): (throwaway fixture)
export FILE_STORE_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow(secret):
# awaken-allow(secret): (throwaway MinIO fixture, destroyed by the exit trap)
# This disposable object's lifetime is the fixture lifetime. Give its real S3
# substrate an independent bounded capacity: Docker's shared image filesystem
# can cross MinIO's free-drive threshold despite this fixture storing few bytes.
# The drill proves database PITR plus exact object copy/restore while MinIO stays
# live; it does not claim recovery from loss of the MinIO process or its storage.
OBJECT_CREATE_ATTEMPTED=1
if ! pg_run_with_deadline object-create "$PG_OPERATION_TIMEOUT_SECONDS" \
  docker run -d --name "$OBJECT_NAME" --label "io.awaken.ci.postgres-run=$PG_OWNER" \
  --tmpfs /data:rw,nosuid,nodev,noexec,size=4g \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  -p 127.0.0.1::9000 "$MINIO_IMAGE" server /data --address :9000 >/dev/null; then
  # Docker renders an unreachable registry, a rate limit and a genuinely private
  # repository with the same "pull access denied" text, so name the image and
  # the override rather than letting the reader conclude it is a permission
  # problem. The image is only absent here because it was never pulled.
  echo "✗ Docker could not create the ephemeral object store from $MINIO_IMAGE" >&2
  echo "  if the pull failed, check registry reachability (docker's \"access denied\" also" >&2
  echo "  covers network and rate-limit failures), or set AWAKEN_CI_OBJECT_IMAGE to a mirror" >&2
  exit 1
fi
if ! OBJECT_PORT="$(published_port "$(pg_run_with_deadline object-port "$PG_OPERATION_TIMEOUT_SECONDS" docker port "$OBJECT_NAME" 9000/tcp)")"; then
  echo "✗ Docker did not publish a valid object-store TCP port" >&2
  exit 1
fi

object_ready=0
object_ready_deadline=$((SECONDS + PG_READY_TIMEOUT_SECONDS))
while (( SECONDS < object_ready_deadline )); do
  if pg_run_with_deadline object-ready "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker run --rm --network "container:$OBJECT_NAME" --entrypoint /bin/sh \
    "$MC_IMAGE" -c \
    'mc alias set local http://127.0.0.1:9000 minioadmin minioadmin >/dev/null && mc ready local >/dev/null && mc mb --ignore-existing local/awaken-blobs >/dev/null'
  then
    object_ready=1
    break
  fi
  sleep 1
done
if [ "$object_ready" -ne 1 ]; then
  echo "✗ ephemeral object store never became ready"
  exit 1
fi
export AWAKEN_TEST_S3_ENDPOINT="http://127.0.0.1:$OBJECT_PORT"
export AWAKEN_TEST_S3_ACCESS_KEY="$FILE_STORE_ACCESS_KEY" # awaken-allow(secret):
export AWAKEN_TEST_S3_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow(secret):
echo "-> AWAKEN_TEST_S3_ENDPOINT=$AWAKEN_TEST_S3_ENDPOINT"

# Authorization decision table: the fixture identity may access only the two
# prefixes exercised by conformance and rotation tests. The live Rust contract
# includes a forbidden write outside those prefixes.
# awaken-allow(secret): (throwaway identity passed into its disposable container)
if ! pg_run_with_deadline object-policy "$PG_OPERATION_TIMEOUT_SECONDS" \
  docker run --rm --network "container:$OBJECT_NAME" \
  --volume "$PWD/scripts/ci/fixtures/minio-file-store-policy.json:/policy.json:ro" \
  --env FILE_STORE_ACCESS_KEY \
  --env FILE_STORE_SECRET_KEY \
  --entrypoint /bin/sh "$MC_IMAGE" -c \
  'mc alias set local http://127.0.0.1:9000 minioadmin minioadmin >/dev/null &&
   mc admin user add local "$FILE_STORE_ACCESS_KEY" "$FILE_STORE_SECRET_KEY" >/dev/null &&
   mc admin policy create local awaken-file-store /policy.json >/dev/null &&
   mc admin policy attach local awaken-file-store --user "$FILE_STORE_ACCESS_KEY" >/dev/null'
then
  echo "✗ failed to create least-privilege object-store fixture identity" >&2
  exit 1
fi

# The Postgres-backed suites. `--test <name>` targets the integration tests that gate
# on a DB. Optional local runs may self-skip only when no URL was configured; this
# script exports one, so a connection failure is red. Single-threaded is unnecessary
# where each test isolates itself in a fresh schema; the Session history matrix is
# serialized explicitly while it creates and drops its canonical-prefix fixtures.
# P7 run the whole ingress contract in this fixture lifetime, including Worker
# cancellation/checkpoint regressions; a hand-maintained target list can omit
# new recovery suites and silently exercise them only without PostgreSQL.
# Registry lineage and replacement must execute against the same real database;
# a no-URL workspace pass is not PostgreSQL conformance evidence.
# Keep HTTP branch receipts visible: passing Rust counts alone cannot identify
# which database adapter ran. Configuration-contract subprocesses reject silent
# fallback before these same entries run against this live fixture.
# Shared checkout's cancellation, poisoned backend and capability boundaries
# are meaningful only against this live database. This selects all R1-R7 rules.
storage_evidence
pg_phase ingress
http_transport_evidence || record_failure "$?"
critical_durability_evidence
# P15 live fixture + explicit ignored selector => measure the canonical shared
# cap and closed-pool rejection, never a no-database/default-suite passing skip.
# The Rust case owns adapter assertions; this runner owns fixture and deadlines.
checked_backend_parity || record_failure "$?"

# Credential-lifecycle state machine: seed -> revoke old credential -> issue
# replacement -> read the same durable object. Both transitions are mandatory.
export AWAKEN_TEST_S3_ROTATION_ID="rotation-$$"
suite_step postgres.object-store pg_run_with_deadline object-admission "$PG_TEST_TIMEOUT_SECONDS" python3 scripts/ci/_native_shell.py scripts/ci/object_store_tests.sh --admission || record_failure "$?"
suite_step postgres.object-store pg_run_with_deadline object-conformance "$PG_TEST_TIMEOUT_SECONDS" python3 scripts/ci/_native_shell.py scripts/ci/object_store_tests.sh --conformance || record_failure "$?"
suite_step postgres.object-store pg_run_with_deadline object-seed "$PG_TEST_TIMEOUT_SECONDS" python3 scripts/ci/_native_shell.py scripts/ci/object_store_tests.sh --seed || record_failure "$?"
FILE_STORE_ROTATED_SECRET_KEY="awaken-file-store-rotated" # awaken-allow(secret): (throwaway fixture)
export FILE_STORE_SECRET_KEY="$FILE_STORE_ROTATED_SECRET_KEY" # awaken-allow(secret):
# awaken-allow(secret): (replacement passed into the same disposable container)
rotation_status=0
suite_step postgres.object-store pg_run_with_deadline object-credential-rotation "$PG_OPERATION_TIMEOUT_SECONDS" \
  docker run --rm --network "container:$OBJECT_NAME" \
  --env FILE_STORE_ACCESS_KEY \
  --env FILE_STORE_SECRET_KEY \
  --entrypoint /bin/sh "$MC_IMAGE" -c \
  'mc alias set local http://127.0.0.1:9000 minioadmin minioadmin >/dev/null &&
   mc admin user remove local "$FILE_STORE_ACCESS_KEY" >/dev/null &&
   mc admin user add local "$FILE_STORE_ACCESS_KEY" "$FILE_STORE_SECRET_KEY" >/dev/null &&
   mc admin policy attach local awaken-file-store --user "$FILE_STORE_ACCESS_KEY" >/dev/null' \
  || rotation_status=$?
if [ "$rotation_status" -ne 0 ]; then
  echo "✗ failed to rotate object-store fixture credential" >&2
  record_failure "$rotation_status"
fi
export AWAKEN_TEST_S3_OLD_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow(secret):
export AWAKEN_TEST_S3_SECRET_KEY="$FILE_STORE_ROTATED_SECRET_KEY"
suite_step postgres.object-store pg_run_with_deadline object-verify "$PG_TEST_TIMEOUT_SECONDS" python3 scripts/ci/_native_shell.py scripts/ci/object_store_tests.sh --verify || record_failure "$?"
# Run the existing Managed HITL frontier decision table with two independent
# PostgreSQL Session connections; a no-URL workspace test cannot claim this.
# Both entry points require these physical outcomes. Private restart children
# stay parent-only; the shared helper selects all seven live witnesses.
resources_evidence
checked_work_store_evidence || record_failure "$?"

# The same live PostgreSQL and object-store substrates finish with a physical
# point-in-time restore. This is a recovery drill, not another repository or
# an independent test graph.
suite_step postgres.recovery-drill pg_run_with_deadline recovery-drill "$PG_RECOVERY_TIMEOUT_SECONDS" \
  scripts/ci/postgres_recovery_drill.sh "$NAME" "$OBJECT_NAME" "$MC_IMAGE" \
  || record_failure "$?"

if [ "$first_failure" -ne 0 ]; then
  echo "✗ Postgres-backed tests failed (first status $first_failure; see above)"
  exit "$first_failure"
fi
echo "✓ durable backend tests passed against ephemeral PostgreSQL and object storage"
