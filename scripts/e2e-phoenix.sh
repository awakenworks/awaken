#!/usr/bin/env bash
# Boot a local Phoenix instance, run the OTLP/HTTP observability e2e suite,
# and tear the stack down on exit.
#
# Override the test filter:
#   PHOENIX_E2E_FILTER=phoenix_via_helpers ./scripts/e2e-phoenix.sh
#
# Override the Phoenix endpoints (e.g. when reusing a shared instance):
#   PHOENIX_BASE_URL=http://10.0.0.5:6006 \
#     PHOENIX_OTLP_TRACES_ENDPOINT=http://10.0.0.5:6006/v1/traces \
#     PHOENIX_E2E_KEEP=1 \
#     ./scripts/e2e-phoenix.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${REPO_ROOT}/e2e/phoenix/docker-compose.yml"
KEEP="${PHOENIX_E2E_KEEP:-0}"
FILTER="${PHOENIX_E2E_FILTER:-phoenix}"

cleanup() {
  if [[ "${KEEP}" != "1" ]]; then
    docker compose -f "${COMPOSE_FILE}" down -v --remove-orphans >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ -z "${PHOENIX_BASE_URL:-}" ]]; then
  export PHOENIX_BASE_URL="http://127.0.0.1:6006"
fi
if [[ -z "${PHOENIX_OTLP_TRACES_ENDPOINT:-}" ]]; then
  export PHOENIX_OTLP_TRACES_ENDPOINT="${PHOENIX_BASE_URL%/}/v1/traces"
fi

echo "[phoenix-e2e] booting docker-compose stack…"
docker compose -f "${COMPOSE_FILE}" up -d --wait

echo "[phoenix-e2e] running cargo tests (filter: ${FILTER})"
cargo test \
  -p awaken-server \
  --test phoenix_observability_e2e \
  --features otel \
  -- --ignored --nocapture "${FILTER}"
