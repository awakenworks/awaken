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
source "$REPO_ROOT/e2e/k3d/harness.sh"
CLUSTER="awaken-failover"
IMAGE="awaken-topology:latest"
NS="awaken-failover"
LOCAL_PORT="${FAILOVER_LOCAL_PORT:-38611}"
THREAD="failover-thread-1"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
PF_PID=""

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok()  { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

CORDONED=""
cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  [ -n "$CORDONED" ] && kubectl uncordon "$CORDONED" >/dev/null 2>&1 || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d_delete_cluster "$CLUSTER"
  rm -f "$DEPLOY_DIR/awaken-server"
}
trap cleanup EXIT

# Open a port-forward to a SPECIFIC pod (not the Service) so we control which node
# handles a request; wait until the local port answers. Sets PF_PID.
pf_pod() {
  local pod="$1"
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  # A fresh port each call: a killed port-forward can leave the old local port in
  # TIME_WAIT, and reusing it races the new tunnel.
  LOCAL_PORT=$(k3d_available_port "$((LOCAL_PORT + 1))")
  PF_PID=$(k3d_start_port_forward "$NS" "pod/$pod" "$LOCAL_PORT" 3000 /tmp/failover_pf.log)
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
BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host)
[ -n "$BIN" ] || { echo 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server"

log "2/5 build the topology image (copy-in, no in-container rust build)"
k3d_docker_build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile.server" "$DEPLOY_DIR" >/dev/null

log "3/5 create MULTI-node k3d cluster $CLUSTER (server + 2 agents)"
# 2 agents so the two anti-affinity'd brain replicas land on distinct nodes.
k3d_create_cluster "$CLUSTER" 2 2

log "4/5 side-load images into the cluster (k3d image import → all nodes)"
k3d_import_images "$CLUSTER" "$IMAGE" postgres:16
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "5/5 apply the fleet and drive the cross-node failover scenario"
kubectl -n "$NS" apply -k "$DEPLOY_DIR/failover" >/dev/null
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
