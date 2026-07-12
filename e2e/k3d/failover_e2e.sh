#!/usr/bin/env bash
# Cross-node failover e2e (ADR-0022 D6 / ADR-0019) on a real multi-node k3d/k3s
# cluster. A fleet of BRAIN pods on DIFFERENT nodes shares one Postgres for the
# dispatch queue AND the commit history. We:
#   1. submit a durable run on pod A  → the fleet drives it, committing to Postgres
#   2. DELETE pod A (the node that took the submit)
#   3. read the thread on pod B (another node) → history is served from Postgres
#   4. submit a SECOND turn on pod B → the thread continues on a different node
# proving no per-node file pins a thread and no run is lost when a node dies.
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/failover_e2e.sh   (from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
CLUSTER="awaken-failover"
IMAGE="awaken-topology:latest"
NS="awaken-failover"
LOCAL_PORT="${FAILOVER_LOCAL_PORT:-38611}"
THREAD="failover-thread-1"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
NODE="k3d-$CLUSTER-server-0"
PF_PID=""

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok()  { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

CORDONED=""
cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  [ -n "$CORDONED" ] && kubectl uncordon "$CORDONED" >/dev/null 2>&1 || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  rm -f "$DEPLOY_DIR/awaken-server-local"
}
trap cleanup EXIT

# Open a port-forward to a SPECIFIC pod (not the Service) so we control which node
# handles a request; wait until the local port answers. Sets PF_PID.
pf_pod() {
  local pod="$1"
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  # A fresh port each call: a killed port-forward can leave the old local port in
  # TIME_WAIT, and reusing it races the new tunnel.
  LOCAL_PORT=$((LOCAL_PORT + 1))
  kubectl -n "$NS" port-forward "pod/$pod" "$LOCAL_PORT":3000 >/tmp/failover_pf.log 2>&1 &
  PF_PID=$!
  # Wait for a real HTTP 200 from the durable surface, not just a TCP accept: the
  # kubectl local listener accepts before the pod tunnel is ready, so an early fetch
  # would ECONNRESET. Polling an actual request proves the tunnel end-to-end.
  for _ in $(seq 1 60); do
    if curl -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/$THREAD/messages" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  err "port-forward to $pod never served a 200"; kubectl -n "$NS" logs "$pod" --tail=20 || true; return 1
}

# Drive the durable surface through the typed TS driver (test logic lives there,
# with its own connection retries); $1 = subcommand (submit|continue). Returns the
# driver's last line ("OK …" / "FAIL …"). `|| true` so a non-zero exit does not trip
# the caller's `set -e` before we can print the reason.
DRIVER="$REPO_ROOT/e2e/k3d/failover_driver.ts"
drive() {
  THREAD="$THREAD" node "$DRIVER" "$1" "http://127.0.0.1:$LOCAL_PORT" 2>&1 | tail -1
}

log "1/5 build the server binary on the host (rustc 1.96)"
RUSTUP_TOOLCHAIN=1.96.0 cargo build -q -p awaken-server-local --bin awaken-server-local
BIN=$(RUSTUP_TOOLCHAIN=1.96.0 cargo build -p awaken-server-local --bin awaken-server-local --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-server-local': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { echo 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server-local"

log "2/5 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null

log "3/5 create MULTI-node k3d cluster $CLUSTER (server + 2 agents)"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
# 2 agents so the two anti-affinity'd brain replicas land on distinct nodes.
k3d cluster create "$CLUSTER" --agents 2 --wait --timeout 180s >/dev/null

log "4/5 side-load images into the cluster (k3d image import → all nodes)"
# k3d image import handles the docker-save → containerd format across every node in
# one call (robust under docker's containerd image store, where a hand-rolled
# `docker save | ctr import` trips on unresolved config digests). The app image is
# never pullable (imagePullPolicy: Never); postgres/coredns/pause are pre-pulled to
# the host so an offline cluster node need not reach a registry.
PAUSE_IMG=$(docker exec "$NODE" sh -c 'grep -hoE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null)
PAUSE_IMG=${PAUSE_IMG:-rancher/mirrored-pause:3.6}
COREDNS_IMG=$(kubectl -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
COREDNS_IMG=${COREDNS_IMG:-rancher/mirrored-coredns-coredns:1.10.1}
# docker's containerd image store keeps the full multi-arch INDEX under a tag even
# after a --platform pull, and `docker save <tag>` exports that index — whose other
# platforms' (windows, unknown) config blobs are absent locally, so k3s's ctr fails
# with "content digest not found". `docker save --platform linux/amd64 -o file.tar`
# exports a single clean manifest; k3d then imports the tar into every node. The app
# image is local-only (never pullable); the rest are pre-pulled for an offline node.
for img in "$PAUSE_IMG" "$COREDNS_IMG" postgres:16; do
  docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
done
TARDIR=$(mktemp -d)
docker save --platform linux/amd64 -o "$TARDIR/app.tar"     "$IMAGE"
docker save --platform linux/amd64 -o "$TARDIR/pause.tar"   "$PAUSE_IMG"
docker save --platform linux/amd64 -o "$TARDIR/coredns.tar" "$COREDNS_IMG"
docker save --platform linux/amd64 -o "$TARDIR/pg.tar"      postgres:16
k3d image import "$TARDIR"/app.tar "$TARDIR"/pause.tar "$TARDIR"/coredns.tar "$TARDIR"/pg.tar -c "$CLUSTER" >/dev/null
rm -rf "$TARDIR"
kubectl -n kube-system delete pod -l k8s-app=kube-dns >/dev/null 2>&1 || true
kubectl -n kube-system rollout status deploy/coredns --timeout=90s
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "5/5 apply the fleet and drive the cross-node failover scenario"
kubectl -n "$NS" apply -f "$DEPLOY_DIR/failover-postgres.yaml" >/dev/null
echo "waiting for postgres..."
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
echo "waiting for the brain owner (readiness proves the shared PG backend is up)..."
if ! kubectl -n "$NS" rollout status deploy/brain --timeout=150s; then
  err "brain never became ready"
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" logs deploy/brain --tail=25 || true
  exit 1
fi

# The thread's single owner: its pod + node. (Shard-by-thread_id: one node serves a
# thread at a time; the Postgres commit projection is a per-process cache warm-loaded
# at connect, so the LIVE peer model is one owner + failover, not concurrent readers.)
read -r POD0 NODE0 < <(kubectl -n "$NS" get pods -l app=brain -o jsonpath='{.items[0].metadata.name}{" "}{.items[0].spec.nodeName}{"\n"}')
[ -n "$POD0" ] && [ -n "$NODE0" ] || { err "could not resolve the brain owner pod/node"; exit 1; }
echo "owner pod = $POD0 (node $NODE0)"

# 1. Submit a durable run on the owner; the run is driven and committed to Postgres.
log "submit a durable run on the owner ($POD0 / $NODE0)"
pf_pod "$POD0"
R1=$(drive submit) || true
[ "${R1%% *}" = "OK" ] || { err "submit on the owner failed: $R1"; kubectl -n "$NS" logs "$POD0" --tail=25 || true; exit 1; }
ok "owner: durable run committed to shared Postgres ($R1)"
kill "$PF_PID" 2>/dev/null || true; PF_PID=""

# 2. Kill the owner node: cordon it (so the replacement lands ELSEWHERE — a real
#    cross-node handoff) then force-delete the pod (a node/process failure).
log "cordon node $NODE0 and delete the owner pod $POD0 — simulate node failure"
kubectl cordon "$NODE0" >/dev/null
CORDONED="$NODE0"
kubectl -n "$NS" delete pod "$POD0" --grace-period=0 --force >/dev/null 2>&1 || true

# 3. Wait for a FRESH owner pod, rescheduled onto a DIFFERENT node, to become Ready.
#    Its new process connects to Postgres and warm-reloads the thread projection.
log "wait for a fresh owner on another node to warm-reload from Postgres"
POD1=""; NODE1=""
for _ in $(seq 1 60); do
  # `|| true`: until a replacement is Ready the query is empty and `read` hits EOF
  # (exit 1), which under `set -e` would abort the whole script mid-wait.
  p=""; n=""
  read -r p n < <(kubectl -n "$NS" get pods -l app=brain --field-selector=status.phase=Running \
    -o jsonpath='{range .items[?(@.status.containerStatuses[0].ready==true)]}{.metadata.name}{" "}{.spec.nodeName}{"\n"}{end}' 2>/dev/null | head -1) || true
  if [ -n "$p" ] && [ "$p" != "$POD0" ] && [ -n "$n" ]; then POD1="$p"; NODE1="$n"; break; fi
  sleep 2
done
[ -n "$POD1" ] || { err "no fresh owner pod became ready after failover"; kubectl -n "$NS" get pods -o wide || true; exit 1; }
echo "fresh owner pod = $POD1 (node $NODE1)"
[ "$NODE1" != "$NODE0" ] || { err "replacement landed on the SAME node ($NODE0) — cordon failed, not a cross-node test"; exit 1; }

# 4. On the fresh owner (another node): the thread history is served from Postgres,
#    and a SECOND turn continues the same thread — proving cross-node portability.
log "read + continue the thread on the fresh owner ($POD1 / $NODE1)"
pf_pod "$POD1"
R2=$(drive continue) || true
kill "$PF_PID" 2>/dev/null || true; PF_PID=""
if [ "${R2%% *}" = "OK" ]; then
  ok "\nK3D FAILOVER E2E PASS: node $NODE0 failed; a fresh owner on node $NODE1 warm-reloaded the thread from shared Postgres and continued it ($R2). No per-node file pinned the thread; no run was lost."
  exit 0
else
  err "\nK3D FAILOVER E2E FAIL: $R2"
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" logs "$POD1" --tail=25 || true
  exit 1
fi
