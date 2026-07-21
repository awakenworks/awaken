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
# Docker-gated: if docker is unavailable the script SKIPS (exit 0), so it is safe to
# wire into a hook/CI that also runs on machines without docker. CI images that
# provide docker get the real coverage; laptops without it keep the fast path.
#
# Usage: scripts/ci/pg_tests.sh   (from repo root)
set -uo pipefail
cd "$(dirname "$0")/../.."

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "docker unavailable; skipping Postgres-backed tests (they self-skip without a DB)"
  exit 0
fi

NAME="awaken-ci-pg-$$"
PORT="${AWAKEN_CI_PG_PORT:-55432}"
PASSWORD="ci" # awaken-allow: secret (throwaway ephemeral container, torn down on exit)
DB="awaken"

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "-> starting ephemeral Postgres ($NAME) on :$PORT"
docker run -d --name "$NAME" \
  -e POSTGRES_PASSWORD="$PASSWORD" -e POSTGRES_DB="$DB" `# awaken-allow: secret` \
  -p "$PORT:5432" postgres:16-alpine >/dev/null

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
  --test dispatch_conformance \
  --test durable_postgres \
  --test runtime_postgres \
  --test any_store \
  --test sandbox_binding \
  || status=1
cargo test -p awaken-runtime-host \
  commit_claimed_postgres_guard_blocks_reclaim_until_http_commit_finishes \
  || status=1
cargo test -p awaken-config-store --test postgres || status=1
cargo test -p awaken-admin-config-api --test postgres_store || status=1
cargo test -p awaken-store-postgres --test postgres_live || status=1
cargo test -p awaken-model-catalog --test repo_conformance || status=1
cargo test -p awaken-credential-vault --test repo_conformance || status=1
cargo test -p awaken-data-subject --test repo_conformance || status=1
cargo test -p awaken-memory-store --test conformance || status=1
cargo test -p awaken-skill-store --test conformance || status=1
cargo test -p awaken-resource-store --all-features || status=1
cargo test -p awaken-work-store || status=1

if [ "$status" -ne 0 ]; then
  echo "✗ Postgres-backed tests failed (see above)"; exit 1
fi
echo "✓ Postgres-backed tests passed against the ephemeral database"
