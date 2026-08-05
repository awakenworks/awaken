#!/usr/bin/env bash
# NATS-backed dispatch wake + consistency e2e (ADR-0019/0028) on a real k3d cluster.
#
# Proves the OTHER half of the distributed-dispatch wake seam: a fleet of brain pods
# draining ONE shared Postgres queue, but waking each other over a NATS broker
# (AWAKEN_DISPATCH_WAKE=nats) — pg_notify DISABLED. The durable STORE stays Postgres
# (the queue authority); only the best-effort cross-node wake hint moves to NATS. We
# fire M concurrent durable submissions and assert, AUTHORITATIVELY against Postgres,
# that every run was driven EXACTLY ONCE:
#   - no loss : M distinct threads each have committed messages
#   - no dup  : every thread has exactly 2 messages (1 user + 1 assistant echo)
#
# The host build MUST add `--features nats` so the image carries a nats-capable wake
# selector; without it the pod fails loudly at startup ("built without --features nats").
#
# Single-node (--agents 0) to save disk on a busy dev host; the SKIP-LOCKED claim race
# is still genuinely concurrent across three pools. Requires: k3d, kubectl, docker,
# rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/nats_wake_e2e.sh [M]   (default M=12, from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/e2e/k3d/harness.sh"
CLUSTER="awaken-nats-wake"
IMAGE="awaken-topology:latest"
NS="awaken-nats-wake"
LOCAL_PORT="${NATS_WAKE_LOCAL_PORT:-38651}"
M="${1:-12}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
DRIVER="$REPO_ROOT/e2e/k3d/scaling_driver.ts"   # identical submit logic; reused
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

# psql on the postgres pod (authoritative source of truth); -tA = bare scalar.
psql_scalar() { kubectl -n "$NS" exec deploy/postgres -- env PGPASSWORD=test psql -U postgres -d awaken -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }

export CARGO_CACHE_AUTOCLEAN=0

log "1/5 build the server binary on the host (rustc 1.96, --features nats)"
BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host --features nats)
[ -n "$BIN" ] || { echo 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server"

log "2/5 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile.server" "$DEPLOY_DIR" >/dev/null

log "3/5 create single-node k3d cluster $CLUSTER"
# Relax the kubelet disk-eviction thresholds: on a busy dev host the shared disk can
# sit past k3s's default nodefs/imagefs<10%, tainting the node DiskPressure and
# refusing to schedule pods. This is a test box, not a capacity test, so push to ~2%.
k3d_create_cluster "$CLUSTER" 0 2

log "4/5 side-load images into the cluster (single-platform tars → the node)"
k3d_import_images "$CLUSTER" "$IMAGE" postgres:16 nats:2
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "5/5 apply the fleet and drive the nats-wake + consistency scenario (M=$M)"
# S6 wake-path FMECA cause/effect graph:
# C1 Postgres authority is reachable; C2 NATS is reachable at process startup;
# C3 NATS remains reachable after enqueue; C4 each accepted run has a unique
# thread. Effects: E1 startup binds the selected adapter; E2 every accepted run
# reaches exactly one committed reply; E3 loss of the hint only delays drain to
# the bounded Postgres poll fallback; E4 no duplicate/lost durable rows.
#
# | Rule | C1 | C2 | C3 | C4 | Expected effect |
# | S6-1 | T  | T  | T  | T  | E1+E2+E4 through live NATS |
# | S6-2 | T  | T  | F  | T  | E2+E3+E4 after broker loss |
# | S6-3 | T  | F  | -  | -  | fail startup; no silent local selector |
# | S6-4 | F  | *  | *  | -  | durable authority unavailable; no success |
# S6-1/S6-2 execute below. Config/startup tests own S6-3; Postgres strict suites
# own S6-4 so this scenario never creates a second database-failure oracle.
kubectl -n "$NS" apply -k "$DEPLOY_DIR/nats-wake" >/dev/null
echo "waiting for nats + postgres..."
kubectl -n "$NS" rollout status deploy/nats --timeout=120s
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
echo "waiting for the concurrently starting brain fleet (3 replicas)..."
if ! kubectl -n "$NS" rollout status deploy/brain --timeout=150s; then
  err "brain fleet never became ready"; kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" logs deploy/brain --tail=25 || true; exit 1
fi

# Port-forward the load-balanced Service so submissions fan out across the fleet.
LOCAL_PORT=$(k3d_available_port "$((LOCAL_PORT + 1))")
PF_PID=$(k3d_start_port_forward "$NS" svc/brain "$LOCAL_PORT" 3000 /tmp/nats_wake_pf.log)
READY=""
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/probe/messages" 2>/dev/null; then READY=1; break; fi
  sleep 1
done
[ -n "$READY" ] || { err "port-forward never served HTTP 200"; cat /tmp/nats_wake_pf.log || true; exit 1; }

log "fire $M concurrent durable submissions across the fleet"
R1=$(THREAD_PREFIX=natswake node "$DRIVER" submit "http://127.0.0.1:$LOCAL_PORT" "$M" 2>&1 | tail -1) || true
[ "${R1%% *}" = "OK" ] || { err "concurrent submit failed: $R1"; kubectl -n "$NS" logs deploy/brain --tail=25 || true; exit 1; }
ok "all $M submissions accepted + enqueued ($R1)"

# Wait for the fleet to drain the queue: authoritative row count in Postgres.
log "wait for the fleet to drain (waking over NATS), then assert consistency in Postgres"
WANT=$((M * 2))   # one echo turn commits 2 messages: User + Assistant
GOT=0
for _ in $(seq 1 60); do
  GOT=$(psql_scalar "SELECT count(*) FROM runtime_message" || echo 0)
  [ "${GOT:-0}" -ge "$WANT" ] && break
  sleep 2
done

DISTINCT=$(psql_scalar "SELECT count(DISTINCT thread_id) FROM runtime_message")
BADROWS=$(psql_scalar "SELECT count(*) FROM (SELECT thread_id FROM runtime_message GROUP BY thread_id HAVING count(*) <> 2) t")
echo "postgres: total_messages=$GOT (want $WANT)  distinct_threads=$DISTINCT (want $M)  threads_with_wrong_count=$BADROWS (want 0)"

if [ "${GOT:-0}" != "$WANT" ] || [ "${DISTINCT:-0}" != "$M" ] || [ "${BADROWS:-1}" != "0" ]; then
  err "\nK3D NATS-WAKE E2E FAIL: total=$GOT/$WANT distinct=$DISTINCT/$M bad=$BADROWS/0"
  kubectl -n "$NS" logs deploy/brain --tail=30 || true
  exit 1
fi

log "remove NATS after startup; durable polling must still drain a second batch"
kubectl -n "$NS" scale deployment/nats --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod -l app=nats --timeout=60s
R2=$(THREAD_PREFIX=natspoll node "$DRIVER" submit "http://127.0.0.1:$LOCAL_PORT" "$M" 2>&1 | tail -1) || true
[ "${R2%% *}" = "OK" ] || { err "broker-loss submit failed: $R2"; exit 1; }

TOTAL_THREADS=$((M * 2))
TOTAL_MESSAGES=$((TOTAL_THREADS * 2))
GOT=0
for _ in $(seq 1 60); do
  GOT=$(psql_scalar "SELECT count(*) FROM runtime_message" || echo 0)
  [ "${GOT:-0}" -ge "$TOTAL_MESSAGES" ] && break
  sleep 2
done
DISTINCT=$(psql_scalar "SELECT count(DISTINCT thread_id) FROM runtime_message")
BADROWS=$(psql_scalar "SELECT count(*) FROM (SELECT thread_id FROM runtime_message GROUP BY thread_id HAVING count(*) <> 2) t")
if [ "${GOT:-0}" = "$TOTAL_MESSAGES" ] && [ "${DISTINCT:-0}" = "$TOTAL_THREADS" ] && [ "${BADROWS:-1}" = "0" ]; then
  ok "\nK3D NATS-WAKE E2E PASS: live hints and broker-loss polling each drained $M runs exactly once from Postgres."
  exit 0
fi
err "\nK3D NATS-WAKE BROKER-LOSS FAIL: total=$GOT/$TOTAL_MESSAGES distinct=$DISTINCT/$TOTAL_THREADS bad=$BADROWS/0"
kubectl -n "$NS" logs deploy/brain --tail=60 || true
exit 1
