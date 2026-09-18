#!/usr/bin/env bash
# Load-test the durable dispatch pool (O2/O4 scaling capstone / C10) with k6.
#
# Starts the real server binary in durable mode over a shared SQLite queue, runs
# `durable_load.js` (concurrent VUs submit background runs; each must be driven to
# completion — zero loss under load), then tears the server down.
#
# Requires: rustc 1.96, k6. Usage: e2e/k6/run_durable_load.sh
set -euo pipefail


# This runner takes no arguments. Accepting one silently is how a caller's
# intent disappears into a run that answers a different question, and here it
# is minutes of real cluster or server work; refuse before any of it.
if [ "$#" -ne 0 ]; then
  echo "usage: e2e/k6/run_durable_load.sh" >&2
  exit 2
fi
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
PORT="${PORT:-38791}"
STORE_DIR="$(mktemp -d)"
export RUSTUP_TOOLCHAIN=1.96.0

echo "== build the server binary (rustc 1.96) =="
BIN=$(node "$REPO_ROOT/e2e/cargo_binary.mjs" awaken-scenario-host awaken-scenario-host)

echo "== start the durable server (shared queue) on :$PORT =="
SESSION_DEPLOYMENT_INGRESS=durable SESSION_DEPLOYMENT_STORAGE_DIR="$STORE_DIR" \
  AWAKEN_HTTP_ADDR="127.0.0.1:$PORT" AWAKEN_MODEL_MODE=echo \
  "$BIN" &
SERVER=$!
cleanup() { kill "$SERVER" 2>/dev/null || true; rm -rf "$STORE_DIR"; }
trap cleanup EXIT

# Wait for the port to accept connections.
LISTENING=0
for _ in $(seq 1 120); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then exec 3>&- 3<&-; LISTENING=1; break; fi
  sleep 0.5
done
[ "$LISTENING" = 1 ] || { echo "server never accepted a connection on 127.0.0.1:$PORT within 60s" >&2; exit 1; }

echo "== run k6 load =="
BASE_URL="http://127.0.0.1:$PORT" VUS="${VUS:-20}" ITERS="${ITERS:-10}" \
  k6 run "$REPO_ROOT/e2e/k6/durable_load.js"
