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
CLUSTER="awaken-wfailover"
IMAGE="awaken-topology:latest"
NS="awaken-wfailover"
LOCAL_PORT="${WFAILOVER_LOCAL_PORT:-38711}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
NODE="k3d-$CLUSTER-server-0"
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

# Number of committed assistant replies for one thread.
reply_count() {
  local thread="$1"
  curl -fsS "$(BASE)/v1/durable/threads/$thread/messages" 2>/dev/null \
    | python3 -c "import sys,json; m=json.load(sys.stdin).get('messages',[]); print(sum(1 for x in m if x.get('role')=='Assistant' and (x.get('text') or '')))" 2>/dev/null || echo 0
}

log "1/6 build the scenario-host binary on the host (rustc 1.96)"
RUSTUP_TOOLCHAIN=1.96.0 cargo build -q -p awaken-scenario-host --bin awaken-scenario-host
BIN=$(RUSTUP_TOOLCHAIN=1.96.0 cargo build -p awaken-scenario-host --bin awaken-scenario-host --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-scenario-host': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { err 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server"

log "2/6 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile.server" "$DEPLOY_DIR" >/dev/null

log "3/6 create MULTI-node k3d cluster $CLUSTER (server + agent)"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
# The server is schedulable, so one server plus one agent is the smallest topology
# that still proves cross-node worker replacement. Keeping this fixture minimal also
# avoids consuming a third k3s node's inotify/cAdvisor budget on shared CI hosts.
# This is a correctness gate rather than a capacity test. Busy developer hosts can
# have ample absolute space while falling below kubelet's default percentage-based
# eviction threshold, so use a conservative 1% floor on both node roles.
EVICT="eviction-hard=imagefs.available<1%,nodefs.available<1%"
k3d cluster create "$CLUSTER" --agents 1 --wait --timeout 180s \
  --k3s-arg "--kubelet-arg=$EVICT@server:*" \
  --k3s-arg "--kubelet-arg=$EVICT@agent:*" >/dev/null

log "4/6 side-load single-platform images into every node"
# Reuse the repository's established k3d import path. Docker's containerd image
# store retains a multi-arch index under pulled tags; importing that tag directly
# can reference absent configs and fail with "content digest not found". Explicit
# linux/amd64 archives contain only materialized manifests. Preloading k3s system
# images also keeps the application phase independent of registry availability.
PAUSE_IMG=$(docker exec "$NODE" sh -c 'grep -hoE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null)
PAUSE_IMG=${PAUSE_IMG:-rancher/mirrored-pause:3.6}
COREDNS_IMG=$(kubectl -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
COREDNS_IMG=${COREDNS_IMG:-rancher/mirrored-coredns-coredns:1.10.1}
for img in "$PAUSE_IMG" "$COREDNS_IMG" postgres:16; do
  docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
done
TARDIR=$(mktemp -d)
docker save --platform linux/amd64 -o "$TARDIR/app.tar" "$IMAGE"
docker save --platform linux/amd64 -o "$TARDIR/pause.tar" "$PAUSE_IMG"
docker save --platform linux/amd64 -o "$TARDIR/coredns.tar" "$COREDNS_IMG"
docker save --platform linux/amd64 -o "$TARDIR/pg.tar" postgres:16
k3d image import "$TARDIR"/app.tar "$TARDIR"/pause.tar "$TARDIR"/coredns.tar "$TARDIR"/pg.tar -c "$CLUSTER" >/dev/null
rm -rf "$TARDIR"
kubectl -n kube-system delete pod -l k8s-app=kube-dns >/dev/null 2>&1 || true
kubectl -n kube-system rollout status deploy/coredns --timeout=90s

log "5/6 apply the coordinator + worker fleet"
kubectl create namespace "$NS" >/dev/null 2>&1 || true
kubectl -n "$NS" apply -f "$DEPLOY_DIR/worker-failover-postgres.yaml" >/dev/null
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
