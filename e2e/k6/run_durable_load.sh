#!/usr/bin/env bash
# Load-test the durable dispatch pool (O2/O4 scaling capstone / C10) with k6.
#
# Starts the real server binary in durable mode over a shared SQLite queue, runs
# `durable_load.js` (concurrent VUs submit background runs; each must be driven to
# completion — zero loss under load), then tears the server down.
#
# Requires: rustc 1.96, k6. Usage: e2e/k6/run_durable_load.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
PORT="${PORT:-38791}"
STORE_DIR="$(mktemp -d)"
export RUSTUP_TOOLCHAIN=1.96.0

echo "== build the server binary (rustc 1.96) =="
BIN=$(cargo build -q -p awaken-scenario-host --bin awaken-scenario-host --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-server': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { echo 'could not resolve server binary'; exit 1; }

echo "== start the durable server (shared queue) on :$PORT =="
AWAKEN_INGRESS=durable AWAKEN_STORAGE_DIR="$STORE_DIR" \
  AWAKEN_HTTP_ADDR="127.0.0.1:$PORT" AWAKEN_MODEL_MODE=echo \
  "$BIN" &
SERVER=$!
cleanup() { kill "$SERVER" 2>/dev/null || true; rm -rf "$STORE_DIR"; }
trap cleanup EXIT

# Wait for the port to accept connections.
for _ in $(seq 1 120); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then exec 3>&- 3<&-; break; fi
  sleep 0.5
done

echo "== run k6 load =="
BASE_URL="http://127.0.0.1:$PORT" VUS="${VUS:-20}" ITERS="${ITERS:-10}" \
  k6 run "$REPO_ROOT/e2e/k6/durable_load.js"
