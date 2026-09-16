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
# Usage: scripts/ci/pg_tests.sh [--require-docker|--self-test]   (from repo root)
set -uo pipefail
# Keep phase diagnostics visible even when a command suppresses probe output.
# This descriptor belongs to this runner; never log command arguments/secrets.
exec 3>&2
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"
source scripts/ci/_postgres_recovery_root.sh
source scripts/ci/_deadline.sh

require_docker=0
RECOVERY_PARENT=$(postgres_recovery_parent) || exit 2
PG_OPERATION_TIMEOUT_SECONDS="${AWAKEN_PG_OPERATION_TIMEOUT_SECONDS:-30}"
PG_READY_TIMEOUT_SECONDS="${AWAKEN_PG_READY_TIMEOUT_SECONDS:-90}"
PG_TEST_TIMEOUT_SECONDS="${AWAKEN_PG_TEST_TIMEOUT_SECONDS:-900}"
PG_RECOVERY_TIMEOUT_SECONDS="${AWAKEN_PG_RECOVERY_TIMEOUT_SECONDS:-900}"

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
  local started=$SECONDS
  printf 'postgres stage=%s event=start budget_seconds=%s\n' "$stage" "$seconds" >&3
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

published_port() {
  local binding="$1"
  local port="${binding##*:}"
  if [[ "$port" =~ ^[0-9]+$ ]] && [ "$port" -ge 1 ] && [ "$port" -le 65535 ]; then
    printf '%s\n' "$port"
    return 0
  fi
  return 1
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
  # Gate-completeness rules: P9 every Cargo suite enters pg_test and therefore
  # has a per-command deadline; P10 the recovery drill has its independent
  # stage deadline. A bare command is a false-green/hang path, not compatibility.
  if grep -Eq '^cargo test ' "$0"; then
    echo "Postgres gate contains an unbounded Cargo test" >&2
    return 1
  fi
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
  # P13 the Registry migration and conformance cases use the canonical fixture
  # URL and execute both library and integration targets in the live PG gate.
  grep -Fxq 'pg_test cargo test -p awaken-worker-registry --features test-support --lib --test conformance || record_failure "$?"' "$0" || {
    echo "Postgres gate must run WorkerRegistry migration and conformance tests" >&2
    return 1
  }
  grep -Fxq 'pg_test cargo test -p awaken-dispatch --features test-support || record_failure "$?"' "$0" || {
    echo "Postgres gate must run the complete ingress suite with test-support" >&2
    return 1
  }
  # P11 WorkQueue parity uses one generic suite for all three backends. Its
  # required test-support feature must be enabled by the live database gate.
  grep -Fxq 'pg_test cargo test -p awaken-work-store --features test-support || record_failure "$?"' "$0" || {
    echo "Postgres gate must run WorkQueue conformance with test-support" >&2
    return 1
  }
  # P12 initialization-only socket readiness must not qualify TCP clients.
  grep -Fxq '    docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres -d "$DB" >/dev/null 2>&1; then ready=1; break; fi' "$0" || {
    echo "Postgres gate must wait for the final TCP server" >&2
    return 1
  }
  # P8 the claim/commit guard lives in durable_postgres and its HTTP receipt
  # boundary in active_active_postgres. The removed RuntimeHost filter matched
  # zero tests; it is not a compatibility route or additional test coverage.
  if grep -Eq '^(pg_test )?cargo test -p awaken-session-executor' "$0"; then
    echo "Postgres gate retains the retired RuntimeHost test selector" >&2
    return 1
  fi
  # P9 immutable Pod registration must exercise the real database rather than
  # merely compiling a self-skipping test in the no-database workspace gate.
  # P10 independent predecessor stopping exercises the same exact Registry CAS.
  grep -Fq "cargo test -p awaken-dispatch-http --test active_active_postgres --test pod_worker_registration_http --test worker_registration_history_http --test predecessor_stop_http" "$0" || {
    echo "Postgres gate omits the coordinator-owned active/active transport test" >&2
    return 1
  }
  grep -Fq "cargo test -p awaken-session-store --features test-support --test session_repo_conformance" "$0" || {
    echo "Postgres gate omits Session and Deployment repository conformance" >&2
    return 1
  }
  grep -Fqx 'pg_test cargo test -p awaken-session-store --features test-support --lib postgres_ -- --test-threads=1 \' "$0" || {
    echo "Postgres gate omits Session history, read-only open, and repository parity" >&2
    return 1
  }
  grep -Fqx 'pg_test cargo test -p awaken-cli --lib installation_binding::tests::postgres_preflights_all_targets_then_binds_fresh_or_explicit_legacy -- --test-threads=1 --exact || record_failure "$?"' "$0" || {
    echo "Postgres gate omits exact CLI installation-binding continuity" >&2
    return 1
  }
  grep -Fqx 'pg_test cargo test -p awaken-coordinator application_access_store::tests::provisioned_postgres_application_access_durability_release_gate -- --ignored --exact || record_failure "$?"' "$0" || {
    echo "Postgres gate omits fail-closed ApplicationAccess durability conformance" >&2
    return 1
  }
  grep -Fq "cargo test -p awaken-model-catalog-store --features postgres,test-support" "$0" || {
    echo "Postgres gate omits the authoritative model-catalog store" >&2
    return 1
  }
  grep -Fq "concurrent_postgres_claim_reclaim_and_restart_preserve_one_exact_authority" "$0" || {
    echo "Postgres gate omits Environment image-build concurrency and restart fencing" >&2
    return 1
  }
  grep -Fq "cargo test -p awaken-credential-store --features postgres,test-support,sealed-aead" "$0" || {
    echo "Postgres gate omits the authoritative sealed credential store" >&2
    return 1
  }
  grep -Fq "cargo test -p awaken-data-subject-store --features postgres,test-support" "$0" || {
    echo "Postgres gate omits postgres,test-support for awaken-data-subject-store" >&2
    return 1
  }
  local crate
  for crate in awaken-admin-config-api awaken-memory-store awaken-skill-store; do
    grep -Fq "cargo test -p $crate --features postgres" "$0" || {
      echo "Postgres gate omits --features postgres for $crate" >&2
      return 1
    }
  done
  grep -Fq "cargo test -p awaken-file-store --features postgres,sqlite,object-store" "$0" || {
    echo "Durable backend gate omits the production object-store adapter" >&2
    return 1
  }
  grep -Fq "minio_rotated_credentials_revoke_old_secret_and_preserve_objects" "$0" || {
    echo "Durable backend gate omits object-store credential rotation" >&2
    return 1
  }
  test -f scripts/ci/fixtures/minio-file-store-policy.json || {
    echo "Durable backend gate omits the least-privilege object-store policy" >&2
    return 1
  }
  grep -Fq "cargo test -p awaken-resource-persistence" "$0" || {
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
  grep -Fq "cargo test -p awaken-store-sqlite --features test-support --test failure_atomicity" "$0" || {
    echo "Durable backend gate omits SQLite crash and injected fault atomicity" >&2
    return 1
  }
}

case "${1:-}" in
  "") ;;
  --require-docker) require_docker=1 ;;
  --self-test) self_test; exit $? ;;
  *) echo "usage: scripts/ci/pg_tests.sh [--require-docker|--self-test]" >&2; exit 2 ;;
esac

for deadline in "$PG_OPERATION_TIMEOUT_SECONDS" "$PG_READY_TIMEOUT_SECONDS" \
  "$PG_TEST_TIMEOUT_SECONDS" "$PG_RECOVERY_TIMEOUT_SECONDS"; do
  positive_seconds "$deadline" || {
    echo "PostgreSQL deadlines must be positive integers" >&2
    exit 2
  }
done

if ! command -v docker >/dev/null 2>&1 \
  || ! pg_run_with_deadline docker-info "$PG_OPERATION_TIMEOUT_SECONDS" docker info >/dev/null 2>&1; then
  if [ "$require_docker" -eq 1 ]; then
    echo "✗ Docker is required for Postgres-backed release tests" >&2
    exit 1
  fi
  echo "docker unavailable; skipping Postgres-backed tests (they self-skip without a DB)"
  exit 0
fi

NAME="awaken-ci-pg-$$"
OBJECT_NAME="awaken-ci-object-$$"
REQUESTED_PORT="${AWAKEN_CI_PG_PORT:-}"
PASSWORD="ci" # awaken-allow: secret (throwaway ephemeral container, torn down on exit)
DB="awaken"
POSTGRES_CONTAINER_ENV=("POSTGRES_PASSWORD=$PASSWORD" "POSTGRES_DB=$DB") # awaken-allow: secret
RECOVERY_ROOT=""
POSTGRES_CREATE_ATTEMPTED=0
OBJECT_CREATE_ATTEMPTED=0

startup_diagnostics() {
  local name="$1"
  # Read only this invocation's attempted container, including lost create
  # responses. Never inspect Config.Env or broaden to other Docker resources.
  pg_run_with_deadline container-state "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker inspect --format 'status={{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}} error={{json .State.Error}}' \
    "$name" >&3 2>&3 || true
  pg_run_with_deadline container-logs "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker logs --tail 40 "$name" >&3 2>&3 || true
}

cleanup() {
  pg_run_with_deadline cleanup "$PG_OPERATION_TIMEOUT_SECONDS" \
    docker rm -f "$NAME" "$OBJECT_NAME" >/dev/null 2>&1 || true
  if valid_recovery_root "$RECOVERY_ROOT"; then
    [ ! -d "$RECOVERY_ROOT" ] || rm -rf -- "$RECOVERY_ROOT"
  elif [ -n "$RECOVERY_ROOT" ]; then
    echo "refusing unexpected PostgreSQL recovery cleanup target: $RECOVERY_ROOT" >&2
  fi
}
finish() {
  local result=$?
  trap - EXIT
  if [ "$result" -ne 0 ]; then
    [ "$POSTGRES_CREATE_ATTEMPTED" -eq 0 ] || startup_diagnostics "$NAME"
    [ "$OBJECT_CREATE_ATTEMPTED" -eq 0 ] || startup_diagnostics "$OBJECT_NAME"
  fi
  cleanup
  exit "$result"
}
trap finish EXIT

RECOVERY_ROOT=$(mktemp -d "$(postgres_recovery_prefix "$RECOVERY_PARENT")XXXXXX")
valid_recovery_root "$RECOVERY_ROOT" \
  || { echo "mktemp returned an unexpected PostgreSQL recovery path: $RECOVERY_ROOT" >&2; exit 1; }
export AWAKEN_RECOVERY_ROOT="$RECOVERY_ROOT"
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
  docker run -d --name "$NAME" \
  -e "${POSTGRES_CONTAINER_ENV[0]}" -e "${POSTGRES_CONTAINER_ENV[1]}" \
  -v "$RECOVERY_ROOT/base:/recovery-base" \
  -v "$RECOVERY_ROOT/wal:/wal-archive" \
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

export AWAKEN_TEST_DATABASE_URL="postgres://postgres:$PASSWORD@127.0.0.1:$PORT/$DB" # awaken-allow: secret
echo "-> AWAKEN_TEST_DATABASE_URL configured for ephemeral Postgres :$PORT"

# Exact, reviewed tags. A runner that cannot reach Docker Hub may point these at
# a mirror holding the same digests; the tag stays exact either way, because a
# floating tag would let the object substrate change under the recovery drill.
MINIO_IMAGE="${AWAKEN_CI_OBJECT_IMAGE:-minio/minio:RELEASE.2024-12-18T13-15-44Z}"
MC_IMAGE="${AWAKEN_CI_OBJECT_CLIENT_IMAGE:-minio/mc:RELEASE.2024-11-21T17-21-54Z}"
export FILE_STORE_ACCESS_KEY="awaken-file-store" # awaken-allow: secret (throwaway fixture identity)
FILE_STORE_INITIAL_SECRET_KEY="awaken-file-store-initial" # awaken-allow: secret (throwaway fixture)
export FILE_STORE_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow: secret
# awaken-allow: secret (throwaway MinIO fixture, destroyed by the exit trap)
# This disposable object's lifetime is the fixture lifetime. Give its real S3
# substrate an independent bounded capacity: Docker's shared image filesystem
# can cross MinIO's free-drive threshold despite this fixture storing few bytes.
# The drill proves database PITR plus exact object copy/restore while MinIO stays
# live; it does not claim recovery from loss of the MinIO process or its storage.
OBJECT_CREATE_ATTEMPTED=1
if ! pg_run_with_deadline object-create "$PG_OPERATION_TIMEOUT_SECONDS" \
  docker run -d --name "$OBJECT_NAME" \
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
export AWAKEN_TEST_S3_ACCESS_KEY="$FILE_STORE_ACCESS_KEY" # awaken-allow: secret
export AWAKEN_TEST_S3_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow: secret
echo "-> AWAKEN_TEST_S3_ENDPOINT=$AWAKEN_TEST_S3_ENDPOINT"

# Authorization decision table: the fixture identity may access only the two
# prefixes exercised by conformance and rotation tests. The live Rust contract
# includes a forbidden write outside those prefixes.
# awaken-allow: secret (throwaway identity passed into its disposable container)
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
# Result-composition decision table: F1 all stages succeed => pass; F2 one
# stage fails => retain that status; F3 later stages fail after F2 => keep the
# first status while continuing the evidence sweep; F4 cleanup is best-effort
# and cannot replace F2/F3. This preserves the causal failure instead of
# reporting the last diagnostic or a generic successful teardown.
first_failure=0
record_failure() {
  local failure="$1"
  if [ "$first_failure" -eq 0 ]; then
    first_failure="$failure"
  fi
}
# P7 run the whole ingress contract in this fixture lifetime, including Worker
# cancellation/checkpoint regressions; a hand-maintained target list can omit
# new recovery suites and silently exercise them only without PostgreSQL.
# Registry lineage and replacement must execute against the same real database;
# a no-URL workspace pass is not PostgreSQL conformance evidence.
# Keep HTTP branch receipts visible: passing Rust counts alone cannot identify
# which database adapter ran. Configuration-contract subprocesses reject silent
# fallback before these same entries run against this live fixture.
pg_test cargo test -p awaken-worker-registry --features test-support --lib --test conformance || record_failure "$?"
pg_test cargo test -p awaken-dispatch --features test-support || record_failure "$?"
pg_test cargo test -p awaken-dispatch-http --test active_active_postgres --test pod_worker_registration_http --test worker_registration_history_http --test predecessor_stop_http -- --test-threads=1 --show-output \
  || record_failure "$?"
pg_test cargo test -p awaken-session-store --features test-support --test session_repo_conformance -- --test-threads=1 \
  || record_failure "$?"
pg_test cargo test -p awaken-session-store --features test-support --lib postgres_ -- --test-threads=1 \
  || record_failure "$?"
pg_test cargo test -p awaken-cli --lib installation_binding::tests::postgres_preflights_all_targets_then_binds_fresh_or_explicit_legacy -- --test-threads=1 --exact || record_failure "$?"
pg_test cargo test -p awaken-coordinator application_access_store::tests::provisioned_postgres_application_access_durability_release_gate -- --ignored --exact || record_failure "$?"
pg_test cargo test -p awaken-config-store --features test-support --test postgres || record_failure "$?"
pg_test cargo test -p awaken-admin-config-api --features postgres --test postgres_store || record_failure "$?"
pg_test cargo test -p awaken-store-postgres --test postgres_live || record_failure "$?"
pg_test cargo test -p awaken-model-catalog-store --features postgres,test-support --test repo_conformance || record_failure "$?"
pg_test cargo test -p awaken-credential-store --features postgres,test-support,sealed-aead --test repo_conformance || record_failure "$?"
pg_test cargo test -p awaken-data-subject-store --features postgres,test-support --test repo_conformance || record_failure "$?"
pg_test cargo test -p awaken-captured-content-store --features postgres,test-support --test repo_conformance || record_failure "$?"
pg_test cargo test -p awaken-env-store --features test-support --tests || record_failure "$?"
pg_test cargo test -p awaken-file-store --features postgres,sqlite,object-store || record_failure "$?"

# Credential-lifecycle state machine: seed -> revoke old credential -> issue
# replacement -> read the same durable object. Both transitions are mandatory.
export AWAKEN_TEST_S3_ROTATION_ID="rotation-$$"
pg_test cargo test -p awaken-file-store --features object-store \
  object::tests::minio_rotation_seed -- --exact \
  || record_failure "$?"
FILE_STORE_ROTATED_SECRET_KEY="awaken-file-store-rotated" # awaken-allow: secret (throwaway fixture)
export FILE_STORE_SECRET_KEY="$FILE_STORE_ROTATED_SECRET_KEY" # awaken-allow: secret
# awaken-allow: secret (replacement passed into the same disposable container)
rotation_status=0
pg_run_with_deadline object-credential-rotation "$PG_OPERATION_TIMEOUT_SECONDS" \
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
export AWAKEN_TEST_S3_OLD_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow: secret
export AWAKEN_TEST_S3_SECRET_KEY="$FILE_STORE_ROTATED_SECRET_KEY"
pg_test cargo test -p awaken-file-store --features object-store \
  object::tests::minio_rotated_credentials_revoke_old_secret_and_preserve_objects -- --exact \
  || record_failure "$?"
pg_test cargo test -p awaken-memory-store --features postgres --test conformance || record_failure "$?"
pg_test cargo test -p awaken-skill-store --features postgres --test conformance || record_failure "$?"
pg_test cargo test -p awaken-resource-store --all-features || record_failure "$?"
pg_test cargo test -p awaken-resource-persistence || record_failure "$?"
pg_test cargo test -p awaken-session-application --lib postgres_ -- --test-threads=1 \
  || record_failure "$?"
pg_test cargo test -p awaken-store-schema --test apply || record_failure "$?"
pg_test cargo test -p awaken-store-sqlite --features test-support --test failure_atomicity || record_failure "$?"
pg_test cargo test -p awaken-sandbox-policy-store --features test-support || record_failure "$?"
pg_test cargo test -p awaken-executable-agent-catalog || record_failure "$?"
pg_test cargo test -p awaken-executable-environment-catalog || record_failure "$?"
pg_test cargo test -p awaken-environment-image-build \
  postgres::tests::concurrent_postgres_claim_reclaim_and_restart_preserve_one_exact_authority \
  -- --exact --test-threads=1 || record_failure "$?"
pg_test cargo test -p awaken-work-store --features test-support || record_failure "$?"

# The same live PostgreSQL and object-store substrates finish with a physical
# point-in-time restore. This is a recovery drill, not another repository or
# an independent test graph.
pg_run_with_deadline recovery-drill "$PG_RECOVERY_TIMEOUT_SECONDS" \
  scripts/ci/postgres_recovery_drill.sh "$NAME" "$OBJECT_NAME" "$MC_IMAGE" \
  || record_failure "$?"

if [ "$first_failure" -ne 0 ]; then
  echo "✗ Postgres-backed tests failed (first status $first_failure; see above)"
  exit "$first_failure"
fi
echo "✓ durable backend tests passed against ephemeral PostgreSQL and object storage"
