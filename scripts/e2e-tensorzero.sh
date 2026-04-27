#!/usr/bin/env bash
# Boot a local TensorZero stack, run the awaken-server TZ e2e suite, and
# tear the stack down on exit.
#
# Required:
#   DEEPSEEK_API_KEY (or OPENAI_API_KEY when shifting variants in
#   e2e/tensorzero/config/tensorzero.toml).
#
# Override the test filter:
#   TENSORZERO_E2E_FILTER=tz_simple_qa ./scripts/e2e-tensorzero.sh
#
# Reuse a running stack (skip boot/teardown):
#   TENSORZERO_E2E_KEEP=1 ./scripts/e2e-tensorzero.sh
#
# Override the gateway URL (e.g. when using a remote stack):
#   TENSORZERO_GATEWAY_URL=http://10.0.0.5:3000 \
#     TENSORZERO_E2E_KEEP=1 \
#     ./scripts/e2e-tensorzero.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${REPO_ROOT}/e2e/tensorzero/docker-compose.yml"
KEEP="${TENSORZERO_E2E_KEEP:-0}"
FILTER="${TENSORZERO_E2E_FILTER:-tz_}"

cleanup() {
  if [[ "${KEEP}" != "1" ]]; then
    docker compose -f "${COMPOSE_FILE}" down -v --remove-orphans >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ -z "${TENSORZERO_GATEWAY_URL:-}" ]]; then
  export TENSORZERO_GATEWAY_URL="http://127.0.0.1:3000"
fi

if [[ "${KEEP}" != "1" ]]; then
  echo "[tz-e2e] booting docker-compose stack…"
  docker compose -f "${COMPOSE_FILE}" up -d --wait
fi

echo "[tz-e2e] running cargo tests (filter: ${FILTER})"
cargo test \
  -p awaken-server \
  --test e2e_tensorzero \
  -- --ignored --nocapture "${FILTER}"
