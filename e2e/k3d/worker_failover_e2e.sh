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
# Proves NO-LOSS recovery. It does not prove sandbox re-adoption: this topology gives
# each pod a private Workdir, while continuity requires a provider whose durable handle
# is adoptable by the replacement. The production adoption seam is covered by the
# runtime-host stable-root test and container-provider integration suites.
#
# ADR-0056 role: this is the DRIVING-SCENARIO the ADR-0056 G-Y guardrail requires
# before Container-tier `adopt(handle)`/`process(pid)` reattach may merge — "SandboxPool
# (Container reuse) … may not merge without an accompanying failing driving-scenario
# test." Today it asserts no-loss over the worker HTTP path. A future shared-container
# k3d fixture should additionally assert the same handle and in-flight marker survive.
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build).
# Usage: e2e/k3d/worker_failover_e2e.sh   (from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/e2e/k3d/harness.sh"
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
  k3d_delete_cluster "$CLUSTER"
  rm -f "$DEPLOY_DIR/awaken-server"
}
trap cleanup EXIT

if ! k3d_require_tools; then
  echo "k3d/kubectl/docker unavailable; skipping worker-failover e2e"
  exit 0
fi

# Port-forward to the brain Service (any pod serves the durable API — it is the
# coordinator, backed by shared Postgres).
pf_brain() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  LOCAL_PORT=$((LOCAL_PORT + 1))
  PF_PID=$(k3d_start_port_forward "$NS" svc/brain "$LOCAL_PORT" 3000 /tmp/wfailover_pf.log)
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

# Number of committed assistant replies for one thread.
reply_count() {
  local thread="$1"
  curl -fsS "$(BASE)/v1/durable/threads/$thread/messages" 2>/dev/null \
    | python3 -c "import sys,json; m=json.load(sys.stdin).get('messages',[]); print(sum(1 for x in m if x.get('role')=='Assistant' and (x.get('text') or '')))" 2>/dev/null || echo 0
}

log "1/6 build the scenario-host binary on the host (rustc 1.96)"
BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host)
[ -n "$BIN" ] || { err 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server"

log "2/6 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile.server" "$DEPLOY_DIR" >/dev/null

log "3/6 create MULTI-node k3d cluster $CLUSTER (server + agent)"
# The server is schedulable, so one server plus one agent is the smallest topology
# that still proves cross-node worker replacement. Keeping this fixture minimal also
# avoids consuming a third k3s node's inotify/cAdvisor budget on shared CI hosts.
# This is a correctness gate rather than a capacity test. Busy developer hosts can
# have ample absolute space while falling below kubelet's default percentage-based
# eviction threshold, so use a conservative 1% floor on both node roles.
k3d_create_cluster "$CLUSTER" 1 1

log "4/6 side-load single-platform images into every node"
k3d_import_images "$CLUSTER" "$IMAGE" postgres:16

log "5/6 apply the coordinator + worker fleet"
kubectl create namespace "$NS" >/dev/null 2>&1 || true
kubectl -n "$NS" apply -k "$DEPLOY_DIR/worker-failover" >/dev/null
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
if ! kubectl -n "$NS" rollout status deploy/brain --timeout=150s; then
  err "brain never became ready"
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" get events --sort-by=.lastTimestamp | tail -30 || true
  kubectl -n "$NS" logs deploy/brain --tail=60 || true
  exit 1
fi
if ! kubectl -n "$NS" rollout status deploy/worker --timeout=150s; then
  err "worker fleet never became ready"
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" get events --sort-by=.lastTimestamp | tail -30 || true
  kubectl -n "$NS" logs -l app=worker --tail=60 || true
  exit 1
fi
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
    [ "$(reply_count "$t")" -ge 1 ] || pending=$((pending + 1))
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

for t in "${threads[@]}"; do
  count=$(reply_count "$t")
  [ "$count" -eq 1 ] || {
    err "FAIL: $t committed $count assistant replies; expected exactly one"
    exit 1
  }
done

ok "PASS: all $BATCH runs committed exactly-once despite a worker crash — the worker fleet is not a data-loss SPOF"
