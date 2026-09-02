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
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"

require_docker=0
RECOVERY_PARENT=${TMPDIR:-/tmp}
[ "$RECOVERY_PARENT" = "/" ] || RECOVERY_PARENT=${RECOVERY_PARENT%/}

published_port() {
  local binding="$1"
  local port="${binding##*:}"
  if [[ "$port" =~ ^[0-9]+$ ]] && [ "$port" -ge 1 ] && [ "$port" -le 65535 ]; then
    printf '%s\n' "$port"
    return 0
  fi
  return 1
}

valid_recovery_root() {
  case "$1" in
    "$RECOVERY_PARENT"/awaken-pg-recovery.??????) return 0 ;;
    *) return 1 ;;
  esac
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
  # Cleanup cause/effect rules: C4 only the exact mktemp-owned six-character
  # suffix is present => E4 cleanup may remove it. C5 empty, caller-owned, or
  # structurally broader paths => E5 cleanup refuses them. This keeps a failed
  # allocation from falling back to an inherited AWAKEN_RECOVERY_ROOT.
  valid_recovery_root "$RECOVERY_PARENT/awaken-pg-recovery.A1b2C3" || return 1
  ! valid_recovery_root "" || return 1
  ! valid_recovery_root "$RECOVERY_PARENT/caller-owned" || return 1
  ! valid_recovery_root "$RECOVERY_PARENT/awaken-pg-recovery.A1b2C3/child" || return 1
  # P5: every feature-gated Postgres adapter is compiled as such. P6: test
  # targets whose volatile fixtures are isolated behind `test-support` enable
  # that feature explicitly; Cargo refusing to start a target is a red gate,
  # never evidence that the Postgres behavior passed.
  grep -Fq "cargo test -p awaken-run-ingress --features test-support" "$0" || {
    echo "Postgres gate omits --features test-support for awaken-run-ingress" >&2
    return 1
  }
  grep -Fq "cargo test -p awaken-run-ingress-http --test active_active_postgres" "$0" || {
    echo "Postgres gate omits the coordinator-owned active/active transport test" >&2
    return 1
  }
  grep -Fq "cargo test -p awaken-session-store --features test-support --test session_repo_conformance" "$0" || {
    echo "Postgres gate omits Session and Deployment repository conformance" >&2
    return 1
  }
  grep -Fqx 'cargo test -p awaken-session-store --features test-support --lib postgres_ -- --test-threads=1 \' "$0" || {
    echo "Postgres gate omits Session history, read-only open, and repository parity" >&2
    return 1
  }
  grep -Fqx 'cargo test -p awaken-cli --lib installation_binding::tests::postgres_preflights_all_targets_then_binds_fresh_or_explicit_legacy -- --test-threads=1 --exact || status=1' "$0" || {
    echo "Postgres gate omits exact CLI installation-binding continuity" >&2
    return 1
  }
  grep -Fqx "cargo test -p awaken-coordinator application_access_store::tests::provisioned_postgres_application_access_durability_release_gate -- --ignored --exact || status=1" "$0" || {
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

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
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

cleanup() {
  docker rm -f "$NAME" "$OBJECT_NAME" >/dev/null 2>&1 || true
  if valid_recovery_root "$RECOVERY_ROOT"; then
    [ ! -d "$RECOVERY_ROOT" ] || rm -rf -- "$RECOVERY_ROOT"
  elif [ -n "$RECOVERY_ROOT" ]; then
    echo "refusing unexpected PostgreSQL recovery cleanup target: $RECOVERY_ROOT" >&2
  fi
}
trap cleanup EXIT

RECOVERY_ROOT=$(mktemp -d "$RECOVERY_PARENT/awaken-pg-recovery.XXXXXX")
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
if ! docker run -d --name "$NAME" \
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
  if ! PORT="$(published_port "$(docker port "$NAME" 5432/tcp)")"; then
    echo "✗ Docker did not publish a valid Postgres TCP port" >&2
    exit 1
  fi
  echo "-> Docker published ephemeral Postgres on :$PORT"
fi

# Wait until the server accepts connections (bounded; fail loud if it never does).
ready=0
for _ in $(seq 1 60); do
  if docker exec "$NAME" pg_isready -U postgres -d "$DB" >/dev/null 2>&1; then ready=1; break; fi
  sleep 1
done
if [ "$ready" -ne 1 ]; then
  echo "✗ ephemeral Postgres never became ready"; docker logs "$NAME" | tail -20; exit 1
fi

export AWAKEN_TEST_DATABASE_URL="postgres://postgres:$PASSWORD@127.0.0.1:$PORT/$DB" # awaken-allow: secret
echo "-> AWAKEN_TEST_DATABASE_URL configured for ephemeral Postgres :$PORT"

MINIO_IMAGE="minio/minio:RELEASE.2024-12-18T13-15-44Z"
MC_IMAGE="minio/mc:RELEASE.2024-11-21T17-21-54Z"
export FILE_STORE_ACCESS_KEY="awaken-file-store" # awaken-allow: secret (throwaway fixture identity)
FILE_STORE_INITIAL_SECRET_KEY="awaken-file-store-initial" # awaken-allow: secret (throwaway fixture)
export FILE_STORE_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow: secret
# awaken-allow: secret (throwaway MinIO fixture, destroyed by the exit trap)
if ! docker run -d --name "$OBJECT_NAME" \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  -p 127.0.0.1::9000 "$MINIO_IMAGE" server /data --address :9000 >/dev/null; then
  echo "✗ Docker could not create the ephemeral object store" >&2
  exit 1
fi
if ! OBJECT_PORT="$(published_port "$(docker port "$OBJECT_NAME" 9000/tcp)")"; then
  echo "✗ Docker did not publish a valid object-store TCP port" >&2
  exit 1
fi

object_ready=0
for _ in $(seq 1 60); do
  if docker run --rm --network "container:$OBJECT_NAME" --entrypoint /bin/sh \
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
  docker logs "$OBJECT_NAME" | tail -20
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
if ! docker run --rm --network "container:$OBJECT_NAME" \
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
status=0
cargo test -p awaken-run-ingress \
  --features test-support \
  --test dispatch_conformance \
  --test durable_postgres \
  --test runtime_postgres \
  --test any_store \
  --test sandbox_binding \
  || status=1
cargo test -p awaken-runtime-host \
  commit_claimed_postgres_guard_blocks_reclaim_until_http_commit_finishes \
  || status=1
cargo test -p awaken-run-ingress-http --test active_active_postgres -- --test-threads=1 \
  || status=1
cargo test -p awaken-session-store --features test-support --test session_repo_conformance -- --test-threads=1 \
  || status=1
cargo test -p awaken-session-store --features test-support --lib postgres_ -- --test-threads=1 \
  || status=1
cargo test -p awaken-cli --lib installation_binding::tests::postgres_preflights_all_targets_then_binds_fresh_or_explicit_legacy -- --test-threads=1 --exact || status=1
cargo test -p awaken-coordinator application_access_store::tests::provisioned_postgres_application_access_durability_release_gate -- --ignored --exact || status=1
cargo test -p awaken-config-store --test postgres || status=1
cargo test -p awaken-admin-config-api --features postgres --test postgres_store || status=1
cargo test -p awaken-store-postgres --test postgres_live || status=1
cargo test -p awaken-model-catalog-store --features postgres,test-support --test repo_conformance || status=1
cargo test -p awaken-credential-store --features postgres,test-support,sealed-aead --test repo_conformance || status=1
cargo test -p awaken-data-subject-store --features postgres,test-support --test repo_conformance || status=1
cargo test -p awaken-captured-content-store --features postgres,test-support --test repo_conformance || status=1
cargo test -p awaken-env-store --features test-support --tests || status=1
cargo test -p awaken-file-store --features postgres,sqlite,object-store || status=1

# Credential-lifecycle state machine: seed -> revoke old credential -> issue
# replacement -> read the same durable object. Both transitions are mandatory.
export AWAKEN_TEST_S3_ROTATION_ID="rotation-$$"
cargo test -p awaken-file-store --features object-store \
  object::tests::minio_rotation_seed -- --exact \
  || status=1
FILE_STORE_ROTATED_SECRET_KEY="awaken-file-store-rotated" # awaken-allow: secret (throwaway fixture)
export FILE_STORE_SECRET_KEY="$FILE_STORE_ROTATED_SECRET_KEY" # awaken-allow: secret
# awaken-allow: secret (replacement passed into the same disposable container)
if ! docker run --rm --network "container:$OBJECT_NAME" \
  --env FILE_STORE_ACCESS_KEY \
  --env FILE_STORE_SECRET_KEY \
  --entrypoint /bin/sh "$MC_IMAGE" -c \
  'mc alias set local http://127.0.0.1:9000 minioadmin minioadmin >/dev/null &&
   mc admin user remove local "$FILE_STORE_ACCESS_KEY" >/dev/null &&
   mc admin user add local "$FILE_STORE_ACCESS_KEY" "$FILE_STORE_SECRET_KEY" >/dev/null &&
   mc admin policy attach local awaken-file-store --user "$FILE_STORE_ACCESS_KEY" >/dev/null'
then
  echo "✗ failed to rotate object-store fixture credential" >&2
  status=1
fi
export AWAKEN_TEST_S3_OLD_SECRET_KEY="$FILE_STORE_INITIAL_SECRET_KEY" # awaken-allow: secret
export AWAKEN_TEST_S3_SECRET_KEY="$FILE_STORE_ROTATED_SECRET_KEY"
cargo test -p awaken-file-store --features object-store \
  object::tests::minio_rotated_credentials_revoke_old_secret_and_preserve_objects -- --exact \
  || status=1
cargo test -p awaken-memory-store --features postgres --test conformance || status=1
cargo test -p awaken-skill-store --features postgres --test conformance || status=1
cargo test -p awaken-resource-store --all-features || status=1
cargo test -p awaken-resource-persistence || status=1
cargo test -p awaken-store-schema --test apply || status=1
cargo test -p awaken-store-sqlite --features test-support --test failure_atomicity || status=1
cargo test -p awaken-sandbox-policy-store --features test-support || status=1
cargo test -p awaken-executable-agent-catalog || status=1
cargo test -p awaken-executable-environment-catalog || status=1
cargo test -p awaken-environment-image-build \
  postgres::tests::concurrent_postgres_claim_reclaim_and_restart_preserve_one_exact_authority \
  -- --exact --test-threads=1 || status=1
cargo test -p awaken-work-store || status=1

# The same live PostgreSQL and object-store substrates finish with a physical
# point-in-time restore. This is a recovery drill, not another repository or
# an independent test graph.
scripts/ci/postgres_recovery_drill.sh "$NAME" "$OBJECT_NAME" "$MC_IMAGE" || status=1

if [ "$status" -ne 0 ]; then
  echo "✗ Postgres-backed tests failed (see above)"; exit 1
fi
echo "✓ durable backend tests passed against ephemeral PostgreSQL and object storage"
