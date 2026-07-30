#!/usr/bin/env bash
# Cold-start migration-race e2e (Oversight issue #24) on a real k3d cluster.
#
# THE REGRESSION: `awaken-scoped-migration` creates its ledger before its per-bundle
# lock exists. Concurrent first boots used to collide on Postgres's implicit rowtype
# (`pg_type_typname_nsp_index`). The composition root now owns a database-wide startup
# advisory lock around all repository migrations.
#
# The historical seed-one path remains a regression guard, while the direct 0→3 path
# is now a hard gate proving deployments no longer depend on that workaround.
#
# Two phases, each against a FRESH database (its own namespace + emptyDir Postgres):
#   PHASE 1  WORKAROUND / REGRESSION GUARD (hard PASS/FAIL): seed 1 brain → migrate →
#            scale to 3 → submit M concurrent durable runs → assert exactly-once in
#            Postgres (total messages, distinct threads, bad=0). THIS is the pass gate.
#   PHASE 2  CONCURRENT COLD START (hard PASS/FAIL): fresh DB, 3 brain replicas 0→3
#            at once, no seeding → all Ready, no migration error, no restart.
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/cold_start_race_e2e.sh [M]   (default M=12, from repo root)
set -euo pipefail
export CARGO_CACHE_AUTOCLEAN=0

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/e2e/k3d/harness.sh"
CLUSTER="awaken-cold-start"
IMAGE="awaken-topology:latest"
NS_GUARD="awaken-cold-start-guard"   # phase 1 — the workaround
NS_REPRO="awaken-cold-start-repro"   # phase 2 — the naive race
LOCAL_PORT="${COLD_START_LOCAL_PORT:-38731}"
M="${1:-12}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
MANIFEST="$DEPLOY_DIR/cold-start"
DRIVER="$REPO_ROOT/e2e/k3d/scaling_driver.ts"
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

# psql on the postgres pod of a given namespace (authoritative truth); -tA = bare scalar.
# Polling suppresses transient exec errors, while final assertions preserve stderr.
psql_scalar() { kubectl -n "$1" exec deploy/postgres -- env PGPASSWORD=test psql -U postgres -d awaken -tAc "$2" | tr -d '[:space:]'; }
psql_scalar_quiet() { psql_scalar "$1" "$2" 2>/dev/null; }

log "1/5 build the server binary on the host (rustc 1.96)"
BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host)
[ -n "$BIN" ] || { echo 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server"

log "2/5 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile.server" "$DEPLOY_DIR" >/dev/null

log "3/5 create SINGLE-node k3d cluster $CLUSTER (--agents 0, disk-lean)"
# Relax kubelet disk-eviction: on a busy dev host (docker images + Rust target dir) the
# shared disk can sit past k3s's default nodefs/imagefs<10%, which taints the node
# DiskPressure and refuses to schedule pods. This is a test box, not a capacity test.
k3d_create_cluster "$CLUSTER" 0 2

log "4/5 side-load images into the cluster (single-platform tars)"
k3d_import_images "$CLUSTER" "$IMAGE" postgres:16

# ============================================================================
log "5/5 PHASE 1 — WORKAROUND (regression guard): seed 1 → migrate → scale to 3"
# ============================================================================
kubectl create namespace "$NS_GUARD" >/dev/null 2>&1 || true
# Bring up Postgres FIRST (fresh emptyDir DB), then a SINGLE brain: the lone pod runs
# scoped-migration's `ensure_ledger` + migrations with no peer to race. This preserves
# the historical deployment path as a baseline; phase 2 proves it is no longer required.
kubectl -n "$NS_GUARD" apply -k "$MANIFEST" -l app=postgres >/dev/null
echo "waiting for postgres (phase 1)..."
kubectl -n "$NS_GUARD" rollout status deploy/postgres --timeout=120s
echo "seed ONE brain to run migrations serially (born at replicas=1)..."
kubectl -n "$NS_GUARD" apply -k "$MANIFEST" -l app=brain >/dev/null
if ! kubectl -n "$NS_GUARD" rollout status deploy/brain --timeout=120s; then
  err "seed brain never migrated / became ready"; kubectl -n "$NS_GUARD" get pods -o wide || true
  kubectl -n "$NS_GUARD" logs deploy/brain --tail=30 || true; exit 1
fi
echo "ledger migrated by the lone pod — now scale to the fleet (3 replicas)..."
kubectl -n "$NS_GUARD" scale deploy/brain --replicas=3 >/dev/null
if ! kubectl -n "$NS_GUARD" rollout status deploy/brain --timeout=150s; then
  err "brain fleet never became ready"; kubectl -n "$NS_GUARD" get pods -o wide || true
  kubectl -n "$NS_GUARD" logs deploy/brain --tail=30 || true; exit 1
fi

# Port-forward the load-balanced Service so submissions fan out across the fleet.
LOCAL_PORT=$(k3d_available_port "$((LOCAL_PORT + 1))")
PF_PID=$(k3d_start_port_forward "$NS_GUARD" svc/brain "$LOCAL_PORT" 3000 /tmp/cold_start_pf.log)
for _ in $(seq 1 60); do
  curl -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/probe/messages" 2>/dev/null && break
  sleep 1
done

log "phase 1: fire $M concurrent durable submissions across the fleet"
R1=$(THREAD_PREFIX=cold node "$DRIVER" submit "http://127.0.0.1:$LOCAL_PORT" "$M" 2>&1 | tail -1) || true
[ "${R1%% *}" = "OK" ] || { err "concurrent submit failed: $R1"; kubectl -n "$NS_GUARD" logs deploy/brain --tail=30 || true; exit 1; }
ok "all $M submissions accepted + enqueued ($R1)"

log "phase 1: wait for the fleet to drain, then assert exactly-once in Postgres"
WANT=$((M * 2))   # one echo turn commits 2 messages: User + Assistant
GOT=0
for _ in $(seq 1 60); do
  GOT=$(psql_scalar_quiet "$NS_GUARD" "SELECT count(*) FROM runtime_message" || echo 0)
  [ "${GOT:-0}" -ge "$WANT" ] && break
  sleep 2
done
DISTINCT=$(psql_scalar "$NS_GUARD" "SELECT count(DISTINCT thread_id) FROM runtime_message") || true
BADROWS=$(psql_scalar "$NS_GUARD" "SELECT count(*) FROM (SELECT thread_id FROM runtime_message GROUP BY thread_id HAVING count(*) <> 2) t") || true
ASSERT_LINE="postgres: total_messages=$GOT (want $WANT)  distinct_threads=$DISTINCT (want $M)  threads_with_wrong_count=$BADROWS (want 0)"
echo "$ASSERT_LINE"

if [ "${GOT:-0}" = "$WANT" ] && [ "${DISTINCT:-0}" = "$M" ] && [ "${BADROWS:-1}" = "0" ]; then
  ok "PHASE 1 PASS: seed-1-then-scale-to-3 workaround drained $M concurrent runs exactly-once."
else
  err "PHASE 1 FAIL: total=$GOT/$WANT distinct=$DISTINCT/$M bad=$BADROWS/0"
  kubectl -n "$NS_GUARD" logs deploy/brain --tail=40 || true
  exit 1
fi

# Free the seed fleet (port-forward + namespace) before the repro to keep disk/mem lean.
kill "$PF_PID" 2>/dev/null || true; PF_PID=""
kubectl delete namespace "$NS_GUARD" --wait=false >/dev/null 2>&1 || true

# ============================================================================
log "PHASE 2 — CONCURRENT COLD START: 3 replicas migrate a FRESH DB safely"
# ============================================================================
kubectl create namespace "$NS_REPRO" >/dev/null 2>&1 || true
kubectl -n "$NS_REPRO" apply -k "$MANIFEST" -l app=postgres >/dev/null
echo "waiting for postgres (phase 2, fresh DB)..."
kubectl -n "$NS_REPRO" rollout status deploy/postgres --timeout=120s
# Create the brain Deployment but hold it at 0 so NO pod migrates first, then jump
# straight to 3: the ReplicaSet spawns all three at once against the fresh, ready,
# UNMIGRATED Postgres — the formerly unsafe deployment now guarded at composition.
kubectl -n "$NS_REPRO" apply -k "$MANIFEST" -l app=brain >/dev/null
kubectl -n "$NS_REPRO" scale deploy/brain --replicas=0 >/dev/null
for _ in $(seq 1 30); do
  N=$(kubectl -n "$NS_REPRO" get pods -l app=brain --no-headers 2>/dev/null | wc -l) || true
  [ "${N:-1}" -eq 0 ] && break
  sleep 1
done
echo "launching 3 brain replicas ALL AT ONCE against the fresh DB (no seeding)..."
kubectl -n "$NS_REPRO" scale deploy/brain --replicas=3 >/dev/null

ERR_RE='pg_type_typname_nsp_index|postgres_migration_ledger_schema'
if ! kubectl -n "$NS_REPRO" rollout status deploy/brain --timeout=150s; then
  err "PHASE 2 FAIL: concurrent cold-start fleet never became ready"
  kubectl -n "$NS_REPRO" get pods -l app=brain -o wide || true
  kubectl -n "$NS_REPRO" logs -l app=brain --all-containers --prefix --tail=200 || true
  exit 1
fi

MIGRATION_ERROR=0
while read -r pod || [ -n "$pod" ]; do
  [ -z "$pod" ] && continue
  L=$(kubectl -n "$NS_REPRO" logs "$pod" --tail=200 2>/dev/null; kubectl -n "$NS_REPRO" logs "$pod" -p --tail=200 2>/dev/null) || true
  if echo "$L" | grep -Eq "$ERR_RE"; then
    err "PHASE 2 FAIL: migration catalog race appeared in $pod"
    echo "$L" | grep -E "$ERR_RE" | head -8 || true
    MIGRATION_ERROR=1
  fi
done < <(kubectl -n "$NS_REPRO" get pods -l app=brain -o name | cut -d/ -f2)

RESTARTS=$(kubectl -n "$NS_REPRO" get pods -l app=brain \
  -o jsonpath='{range .items[*]}{.status.containerStatuses[0].restartCount}{"\n"}{end}' \
  | awk '{sum += $1} END {print sum + 0}')
if [ "$MIGRATION_ERROR" -ne 0 ] || [ "$RESTARTS" -ne 0 ]; then
  err "PHASE 2 FAIL: migration_errors=$MIGRATION_ERROR container_restarts=$RESTARTS"
  kubectl -n "$NS_REPRO" get pods -l app=brain -o wide || true
  exit 1
fi
PHASE2_RESULT="all 3 replicas became Ready with 0 migration errors and 0 restarts"
ok "PHASE 2 PASS: $PHASE2_RESULT"
kubectl delete namespace "$NS_REPRO" --wait=false >/dev/null 2>&1 || true

# ============================================================================
log "RESULT"
echo "phase 1 (regression guard): $ASSERT_LINE"
echo "phase 2 (concurrent):       $PHASE2_RESULT"
ok "\nCOLD START E2E PASS: both seed-then-scale and direct 0→3 startup are safe; the fleet drained $M concurrent durable runs exactly-once and concurrent migrations completed without a catalog race."
exit 0
