#!/usr/bin/env bash
# Run the Postgres-backed test suites against an EPHEMERAL Postgres.
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
  grep -Fq "cargo test -p awaken-model-catalog-store --features postgres,test-support" "$0" || {
    echo "Postgres gate omits the authoritative model-catalog store" >&2
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
REQUESTED_PORT="${AWAKEN_CI_PG_PORT:-}"
PASSWORD="ci" # awaken-allow: secret (throwaway ephemeral container, torn down on exit)
DB="awaken"

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

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
  -e POSTGRES_PASSWORD="$PASSWORD" -e POSTGRES_DB="$DB" `# awaken-allow: secret` \
  -p "$PUBLISH" postgres:16-alpine >/dev/null; then
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
echo "-> AWAKEN_TEST_DATABASE_URL=$AWAKEN_TEST_DATABASE_URL"

# The Postgres-backed suites. `--test <name>` targets the integration tests that gate
# on a DB; each still self-skips a case if its schema pool cannot connect, but with the
# URL set they run for real. Single-threaded is unnecessary — each test isolates itself
# in a fresh schema.
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
cargo test -p awaken-config-store --test postgres || status=1
cargo test -p awaken-admin-config-api --features postgres --test postgres_store || status=1
cargo test -p awaken-store-postgres --test postgres_live || status=1
cargo test -p awaken-model-catalog-store --features postgres,test-support --test repo_conformance || status=1
cargo test -p awaken-credential-store --features postgres,test-support,sealed-aead --test repo_conformance || status=1
cargo test -p awaken-data-subject-store --features postgres,test-support --test repo_conformance || status=1
cargo test -p awaken-memory-store --features postgres --test conformance || status=1
cargo test -p awaken-skill-store --features postgres --test conformance || status=1
cargo test -p awaken-resource-store --all-features || status=1
cargo test -p awaken-work-store || status=1

if [ "$status" -ne 0 ]; then
  echo "✗ Postgres-backed tests failed (see above)"; exit 1
fi
echo "✓ Postgres-backed tests passed against the ephemeral database"
