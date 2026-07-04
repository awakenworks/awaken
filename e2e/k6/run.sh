#!/usr/bin/env bash
# Start awaken-server-local in `management` mode, run the k6 scenario suite against
# it, then stop it. Defaults to the smoke profile (a functional gate). Pass
# `stress` to run the concurrent load profile instead.
#
#   e2e/k6/run.sh              # scenario validation (1 VU, a few iterations)
#   e2e/k6/run.sh stress       # ramping-VUs load test
#   VUS=50 e2e/k6/run.sh stress # override the target VU count
set -euo pipefail
cd "$(dirname "$0")/../.."

PROFILE="${1:-smoke}"
PORT="${PORT:-38200}"
MODE="${MODE:-management}"

cargo build --quiet -p awaken-server-local --bin awaken-server-local
# Resolve the target dir (a global cargo config may override `target/`).
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
BIN="${TARGET_DIR}/debug/awaken-server-local"
[ -x "$BIN" ] || { echo "server binary not found at $BIN"; exit 1; }

AWAKEN_HTTP_ADDR="127.0.0.1:${PORT}" AWAKEN_MODEL_MODE="${MODE}" "$BIN" &
SRV=$!
trap 'kill "$SRV" 2>/dev/null || true' EXIT

# Wait for the server to accept connections.
for _ in $(seq 1 120); do
  (exec 3<>"/dev/tcp/127.0.0.1/${PORT}") 2>/dev/null && { exec 3<&- 3>&-; break; }
  sleep 0.5
done

echo "== k6 profile: ${PROFILE} against http://127.0.0.1:${PORT} (mode=${MODE}) =="
k6 run \
  -e "BASE_URL=http://127.0.0.1:${PORT}" \
  -e "K6_PROFILE=${PROFILE}" \
  ${VUS:+-e "VUS=${VUS}"} \
  ${ITERS:+-e "ITERS=${ITERS}"} \
  e2e/k6/management_scenarios.js
