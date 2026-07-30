#!/usr/bin/env bash
# Full-microservices e2e (ADR-0019 durable dispatch × ADR-0044/0045 手腦分離) on a
# real k3d/k3s cluster. Deploys brain + hand + postgres as THREE SEPARATE k8s
# Deployments and drives ONE durable run that must cross all three services:
#
#   submit ─→ [brain pod] ──durable ingress──→ [postgres pod] (dispatch queue + commits)
#                  │
#                  └── bash tool ──TCP:9000 (Service "hand")──→ [hand pod] ──marker──┘
#
# The run's `bash echo REMOTE-HAND-OK-9f31` executes in the SEPARATE hand pod; the
# marker round-trips and is committed to Postgres. We then assert AUTHORITATIVELY
# against Postgres (not a per-pod cached projection) that:
#   1. Durable path committed exactly once: one thread, one run, terminal phase.
#   2. The tool ran on the hand: the marker is present in a committed message.
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/microservices_e2e.sh   (from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/e2e/k3d/harness.sh"
CLUSTER="awaken-micro"
IMAGE="awaken-topology:latest"
NS="awaken-micro"
LOCAL_PORT="${MICRO_LOCAL_PORT:-38651}"
THREAD="micro-1"
MARKER="REMOTE-HAND-OK-9f31"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
DRIVER="$REPO_ROOT/e2e/k3d/microservices_driver.ts"
export CARGO_CACHE_AUTOCLEAN=0
PF_PID=""

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok()  { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d_delete_cluster "$CLUSTER"
  rm -f "$DEPLOY_DIR/awaken-server" "$DEPLOY_DIR/awaken-sandbox"
}
trap cleanup EXIT

# psql on the postgres pod (authoritative source of truth); -tA = bare scalar.
psql_scalar() { kubectl -n "$NS" exec deploy/postgres -- env PGPASSWORD=test psql -U postgres -d awaken -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }

log "1/5 build the brain + canonical hand binaries on the host (rustc 1.96)"
BRAIN_BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host)
HAND_BIN=$(resolve_cargo_executable awaken-sandbox awaken-sandbox --features hand)
[ -n "$BRAIN_BIN" ] && [ -n "$HAND_BIN" ] || { echo 'could not resolve binaries'; exit 1; }
cp "$BRAIN_BIN" "$DEPLOY_DIR/awaken-server"
cp "$HAND_BIN" "$DEPLOY_DIR/awaken-sandbox"

log "2/5 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null

log "3/5 create k3d cluster $CLUSTER (single node; 3 pods on one box)"
# Relax the kubelet disk-eviction thresholds: on a busy dev host (docker images +
# Rust target dir) the shared disk can sit past k3s's default nodefs/imagefs<10%,
# which taints the node DiskPressure and refuses to schedule pods. Test box, not a
# capacity test, so push eviction to ~2%.
k3d_create_cluster "$CLUSTER" 0 2

log "4/5 side-load images into the cluster (single-platform tars → all nodes)"
k3d_import_images "$CLUSTER" "$IMAGE" postgres:16
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "5/5 apply the microservices split (postgres + hand + brain) and drive one durable run"
# Cause/effect decision table for this composed scenario:
# M1 Postgres + canonical Direct topology + durable brain patch -> three Ready
# services and one exactly-once remote-hand commit; M2 hand cannot listen -> hand
# and then brain readiness fail with diagnostics; M3 Postgres cannot serve -> the
# brain cannot become Ready; M4 execution/commit fails -> authoritative SQL counts
# or the remote-hand marker assertion fails. These are terminal hard failures.
kubectl -n "$NS" apply -k "$DEPLOY_DIR/microservices" >/dev/null
echo "waiting for postgres..."
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
echo "waiting for the hand pod (its listen port proves the executor channel is up)..."
if ! kubectl -n "$NS" rollout status deploy/hand --timeout=120s; then
  err "hand never became ready"
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" get events --sort-by=.lastTimestamp | tail -30 || true
  kubectl -n "$NS" logs deploy/hand --tail=30 || true
  exit 1
fi
echo "waiting for the brain pod (readiness proves postgres + hand links are up)..."
if ! kubectl -n "$NS" rollout status deploy/brain --timeout=150s; then
  err "brain never became ready"; kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" logs deploy/brain --tail=30 || true
  kubectl -n "$NS" logs deploy/hand --tail=15 || true; exit 1
fi
echo "pod placement (three separate pods):"
kubectl -n "$NS" get pods -o custom-columns=POD:.metadata.name,APP:.metadata.labels.app,NODE:.spec.nodeName --no-headers || true

# Port-forward the brain's durable HTTP surface; poll a REAL 200 before driving.
LOCAL_PORT=$(k3d_available_port "$((LOCAL_PORT + 1))")
PF_PID=$(k3d_start_port_forward "$NS" svc/brain "$LOCAL_PORT" 3000 /tmp/micro_pf.log)
READY=0
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/probe/messages" 2>/dev/null; then READY=1; break; fi
  sleep 1
done
[ "$READY" = 1 ] || { err "brain durable ingress never answered 200 on the port-forward"; cat /tmp/micro_pf.log || true; exit 1; }

log "submit one durable run through the brain; it must route the bash tool to the hand pod"
R1=$(THREAD="$THREAD" MARKER="$MARKER" node "$DRIVER" run "http://127.0.0.1:$LOCAL_PORT" 2>&1 | tail -1) || true
if [ "${R1%% *}" != "OK" ]; then
  err "durable run failed: $R1"
  kubectl -n "$NS" logs deploy/brain --tail=30 || true
  kubectl -n "$NS" logs deploy/hand --tail=15 || true
  exit 1
fi
ok "durable run committed the remote-hand marker over HTTP ($R1)"

# ── Authoritative assertions against Postgres (not the per-pod projection) ──
log "assert exactly-once + terminal phase + marker-in-postgres against Postgres"
# Give the commit a beat to land (the HTTP poll already saw it, but read from the
# authoritative tables directly).
DISTINCT=0
for _ in $(seq 1 30); do
  DISTINCT=$(psql_scalar "SELECT count(DISTINCT thread_id) FROM runtime_message WHERE thread_id='$THREAD'" || echo 0)
  [ "${DISTINCT:-0}" -ge 1 ] && break
  sleep 1
done

RUNS=$(psql_scalar "SELECT count(*) FROM runtime_run_record WHERE thread_id='$THREAD'" || echo 0)
TERMINAL=$(psql_scalar "SELECT count(*) FROM runtime_run_record WHERE thread_id='$THREAD' AND phase::text LIKE '%Ended%' AND phase::text LIKE '%NaturalEnd%'" || echo 0)
MARKER_ROWS=$(psql_scalar "SELECT count(*) FROM runtime_message WHERE thread_id='$THREAD' AND data::text LIKE '%$MARKER%'" || echo 0)

echo "postgres: distinct_threads=$DISTINCT (want 1)  runs=$RUNS (want 1)  terminal_NaturalEnd=$TERMINAL (want 1)  messages_with_marker=$MARKER_ROWS (want >=1)"

if [ "${DISTINCT:-0}" = "1" ] && [ "${RUNS:-0}" = "1" ] && [ "${TERMINAL:-0}" = "1" ] && [ "${MARKER_ROWS:-0}" -ge 1 ]; then
  ok "\nK3D MICROSERVICES E2E PASS: brain + hand + postgres ran as three separate pods — a durable run submitted to the brain committed exactly once through Postgres, and its bash tool executed on the SEPARATE hand pod (marker '$MARKER' round-tripped into the commit log)."
  exit 0
else
  err "\nK3D MICROSERVICES E2E FAIL: distinct=$DISTINCT/1 runs=$RUNS/1 terminal=$TERMINAL/1 marker_rows=$MARKER_ROWS/>=1"
  kubectl -n "$NS" logs deploy/brain --tail=40 || true
  kubectl -n "$NS" logs deploy/hand --tail=20 || true
  exit 1
fi
