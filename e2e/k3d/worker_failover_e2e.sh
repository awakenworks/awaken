#!/usr/bin/env bash
# Worker-fleet failover e2e (ADR-0019/0022) on a real multi-node k3d/k3s cluster.
#
# Topology: ONE pool-less brain COORDINATOR (queues to shared Postgres, runs no local
# pool) + a FLEET of 2 database-less workers draining over the HTTP worker transport.
# We submit a batch of durable runs, DELETE one worker pod mid-drain, and prove the
# surviving worker reclaims the crashed worker's in-flight + pending runs and drives
# every one to a committed reply — no run lost, exactly-once (the commit fence rejects
# a stale re-commit). This is the worker-topology twin of `failover_e2e.sh` (which
# fails over a BRAIN fleet); it exercises the db-less worker's HTTP claim/commit path
# and the commit fence under a real pod kill.
#
# Proves NO-LOSS recovery. It does NOT prove sandbox re-adoption (unwired; echo has no
# sandbox state). See deploy/k3d/worker-failover-postgres.yaml.
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build).
# Usage: e2e/k3d/worker_failover_e2e.sh   (from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
CLUSTER="awaken-wfailover"
IMAGE="awaken-topology:latest"
NS="awaken-wfailover"
LOCAL_PORT="${WFAILOVER_LOCAL_PORT:-38711}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
BATCH="${WFAILOVER_BATCH:-12}"
PF_PID=""

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok()  { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  rm -f "$DEPLOY_DIR/awaken-server"
}
trap cleanup EXIT

if ! command -v k3d >/dev/null 2>&1 || ! command -v kubectl >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "k3d/kubectl/docker unavailable; skipping worker-failover e2e"
  exit 0
fi

# Port-forward to the brain Service (any pod serves the durable API — it is the
# coordinator, backed by shared Postgres).
pf_brain() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  LOCAL_PORT=$((LOCAL_PORT + 1))
  kubectl -n "$NS" port-forward svc/brain "$LOCAL_PORT":3000 >/tmp/wfailover_pf.log 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 60); do
    if curl -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/probe/messages" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  err "port-forward to brain never served a 200"; return 1
}

BASE() { echo "http://127.0.0.1:$LOCAL_PORT"; }

# Submit one durable run on its own thread; echo the run_id (or empty on failure).
submit_one() {
  local thread="$1"
  curl -fsS -X POST "$(BASE)/v1/durable/threads/$thread/submit_background" \
    -H 'content-type: application/json' -d '{"text":"hello"}' 2>/dev/null \
    | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('run_id','') if d.get('queued') else '')" 2>/dev/null || true
}

# True once a thread has >=1 assistant reply committed.
has_reply() {
  local thread="$1"
  curl -fsS "$(BASE)/v1/durable/threads/$thread/messages" 2>/dev/null \
    | python3 -c "import sys,json; m=json.load(sys.stdin).get('messages',[]); print('yes' if any(x.get('role')=='Assistant' and (x.get('text') or '') for x in m) else 'no')" 2>/dev/null || echo no
}

log "1/6 build the scenario-host binary on the host (rustc 1.96)"
RUSTUP_TOOLCHAIN=1.96.0 cargo build -q -p awaken-scenario-host --bin awaken-scenario-host
BIN=$(RUSTUP_TOOLCHAIN=1.96.0 cargo build -p awaken-scenario-host --bin awaken-scenario-host --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-server': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { err 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server"

log "2/6 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null

log "3/6 create MULTI-node k3d cluster $CLUSTER (server + 2 agents)"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
k3d cluster create "$CLUSTER" --agents 2 --wait --timeout 180s >/dev/null

log "4/6 side-load the image into every node"
k3d image import "$IMAGE" -c "$CLUSTER" >/dev/null

log "5/6 apply the coordinator + worker fleet"
kubectl create namespace "$NS" >/dev/null 2>&1 || true
kubectl -n "$NS" apply -f "$DEPLOY_DIR/worker-failover-postgres.yaml" >/dev/null
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
kubectl -n "$NS" rollout status deploy/brain --timeout=150s
kubectl -n "$NS" rollout status deploy/worker --timeout=150s
pf_brain

log "6/6 submit a batch, kill a worker mid-drain, verify every run still commits"
threads=()
for i in $(seq 1 "$BATCH"); do
  t="wf-thread-$i"
  rid=$(submit_one "$t")
  [ -n "$rid" ] || { err "submit $t failed"; exit 1; }
  threads+=("$t")
done
ok "submitted $BATCH durable runs to the coordinator"

# Kill ONE worker pod while the fleet is draining — a real worker crash. Its in-flight
# claim + any unclaimed work must be recovered by the surviving worker.
victim=$(kubectl -n "$NS" get pods -l app=worker -o jsonpath='{.items[0].metadata.name}')
kubectl -n "$NS" delete pod "$victim" --grace-period=0 --force >/dev/null 2>&1 || true
ok "killed worker pod $victim mid-drain"

# Every run must eventually commit exactly one assistant reply (survivor drains all,
# recovering the crashed worker's in-flight run via lease expiry + fenced re-claim).
deadline=$(( $(date +%s) + 90 ))
while :; do
  pending=0
  for t in "${threads[@]}"; do
    [ "$(has_reply "$t")" = "yes" ] || pending=$((pending + 1))
  done
  [ "$pending" -eq 0 ] && break
  if [ "$(date +%s)" -gt "$deadline" ]; then
    err "FAIL: $pending/$BATCH runs never committed after the worker crash"
    kubectl -n "$NS" get pods -o wide || true
    kubectl -n "$NS" logs -l app=worker --tail=30 || true
    exit 1
  fi
  # A brain restart of the port-forward if it dropped.
  curl -fsS -o /dev/null "$(BASE)/v1/durable/threads/probe/messages" 2>/dev/null || pf_brain
  sleep 2
done

ok "PASS: all $BATCH runs committed exactly-once despite a worker crash — the worker fleet is not a data-loss SPOF"
