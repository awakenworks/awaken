#!/usr/bin/env bash
# Cold-start migration-race e2e (Oversight issue #24) on a real k3d cluster.
#
# THE RACE (verified, still OPEN in the awaken-foundation dep): `awaken-scoped-migration`'s
# `ensure_ledger` creates the ledger tables with bare `CREATE TABLE IF NOT EXISTS`
# OUTSIDE the per-bundle advisory lock (chicken-and-egg: the lock is keyed on a ledger
# table that doesn't exist yet). Postgres `CREATE TABLE IF NOT EXISTS` is NOT
# concurrency-safe for the table's implicit rowtype, so N replicas cold-starting
# against a FRESH Postgres collide on `pg_type_typname_nsp_index` and a replica
# crash-loops (operation `postgres_migration_ledger_schema`).
#
# THE WORKAROUND (what this test PINS as a regression guard): seed ONE replica, let it
# migrate with no peer to race, THEN scale to N. `scaling_e2e.sh` already relies on it;
# this test makes it a first-class, isolated PASS/FAIL and ships a repro harness for #24.
#
# Two phases, each against a FRESH database (its own namespace + emptyDir Postgres):
#   PHASE 1  WORKAROUND / REGRESSION GUARD (hard PASS/FAIL): seed 1 brain → migrate →
#            scale to 3 → submit M concurrent durable runs → assert exactly-once in
#            Postgres (total messages, distinct threads, bad=0). THIS is the pass gate.
#   PHASE 2  NAIVE REPRO (diagnostic, NON-FATAL): fresh DB, 3 brain replicas 0→3 all at
#            once, no seeding → observe whether a replica crash-loops / logs the
#            `pg_type_typname_nsp_index` / `postgres_migration_ledger_schema` error
#            within a bounded wait. The race is timing-dependent, so a no-repro run is
#            NOT a failure — this phase documents/repros #24, it is not the gate.
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/cold_start_race_e2e.sh [M]   (default M=12, from repo root)
set -euo pipefail
export CARGO_CACHE_AUTOCLEAN=0

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
CLUSTER="awaken-cold-start"
IMAGE="awaken-topology:latest"
NS_GUARD="awaken-cold-start-guard"   # phase 1 — the workaround
NS_REPRO="awaken-cold-start-repro"   # phase 2 — the naive race
LOCAL_PORT="${COLD_START_LOCAL_PORT:-38731}"
M="${1:-12}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
MANIFEST="$DEPLOY_DIR/cold-start-postgres.yaml"
NODE="k3d-$CLUSTER-server-0"
DRIVER="$REPO_ROOT/e2e/k3d/scaling_driver.ts"
PF_PID=""

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok()  { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  rm -f "$DEPLOY_DIR/awaken-server-local"
}
trap cleanup EXIT

# psql on the postgres pod of a given namespace (authoritative truth); -tA = bare scalar.
psql_scalar() { kubectl -n "$1" exec deploy/postgres -- env PGPASSWORD=test psql -U postgres -d awaken -tAc "$2" 2>/dev/null | tr -d '[:space:]'; }

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

log "3/5 create SINGLE-node k3d cluster $CLUSTER (--agents 0, disk-lean)"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
# Relax kubelet disk-eviction: on a busy dev host (docker images + Rust target dir) the
# shared disk can sit past k3s's default nodefs/imagefs<10%, which taints the node
# DiskPressure and refuses to schedule pods. This is a test box, not a capacity test.
EVICT="eviction-hard=imagefs.available<2%,nodefs.available<2%"
k3d cluster create "$CLUSTER" --agents 0 --wait --timeout 180s \
  --k3s-arg "--kubelet-arg=$EVICT@server:*" >/dev/null

log "4/5 side-load images into the cluster (single-platform tars)"
PAUSE_IMG=$(docker exec "$NODE" sh -c 'grep -hoE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null)
PAUSE_IMG=${PAUSE_IMG:-rancher/mirrored-pause:3.6}
COREDNS_IMG=$(kubectl -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
COREDNS_IMG=${COREDNS_IMG:-rancher/mirrored-coredns-coredns:1.10.1}
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

# ============================================================================
log "5/5 PHASE 1 — WORKAROUND (regression guard): seed 1 → migrate → scale to 3"
# ============================================================================
kubectl create namespace "$NS_GUARD" >/dev/null 2>&1 || true
# Bring up Postgres FIRST (fresh emptyDir DB), then a SINGLE brain: the lone pod runs
# scoped-migration's `ensure_ledger` + migrations with no peer to race. THIS is the fix.
kubectl -n "$NS_GUARD" apply -f "$MANIFEST" -l app=postgres >/dev/null
echo "waiting for postgres (phase 1)..."
kubectl -n "$NS_GUARD" rollout status deploy/postgres --timeout=120s
echo "seed ONE brain to run migrations serially (born at replicas=1)..."
kubectl -n "$NS_GUARD" apply -f "$MANIFEST" -l app=brain >/dev/null
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
LOCAL_PORT=$((LOCAL_PORT + 1))
kubectl -n "$NS_GUARD" port-forward svc/brain "$LOCAL_PORT":3000 >/tmp/cold_start_pf.log 2>&1 &
PF_PID=$!
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
  GOT=$(psql_scalar "$NS_GUARD" "SELECT count(*) FROM runtime_message" || echo 0)
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
log "PHASE 2 — NAIVE REPRO (diagnostic, NON-FATAL): 3 replicas cold-start a FRESH DB"
# ============================================================================
REPRO_RESULT="race not triggered this run — it is timing-dependent"
kubectl create namespace "$NS_REPRO" >/dev/null 2>&1 || true
kubectl -n "$NS_REPRO" apply -f "$MANIFEST" -l app=postgres >/dev/null
echo "waiting for postgres (phase 2, fresh DB)..."
kubectl -n "$NS_REPRO" rollout status deploy/postgres --timeout=120s
# Create the brain Deployment but hold it at 0 so NO pod migrates first, then jump
# straight to 3: the ReplicaSet spawns all three at once against the fresh, ready,
# UNMIGRATED Postgres — the exact naive deployment that races `ensure_ledger`.
kubectl -n "$NS_REPRO" apply -f "$MANIFEST" -l app=brain >/dev/null
kubectl -n "$NS_REPRO" scale deploy/brain --replicas=0 >/dev/null
for _ in $(seq 1 30); do
  N=$(kubectl -n "$NS_REPRO" get pods -l app=brain --no-headers 2>/dev/null | wc -l) || true
  [ "${N:-1}" -eq 0 ] && break
  sleep 1
done
echo "launching 3 brain replicas ALL AT ONCE against the fresh DB (no seeding)..."
kubectl -n "$NS_REPRO" scale deploy/brain --replicas=3 >/dev/null

# Bounded observation window: watch for a crash-looping replica or the ledger error.
RACE=0
CRASH_POD=""
ERR_RE='pg_type_typname_nsp_index|postgres_migration_ledger_schema'
for _ in $(seq 1 45); do
  # Any replica with restarts and a CrashLoopBackOff/Error waiting reason?
  while read -r pod restarts reason || [ -n "$pod" ]; do
    [ -z "$pod" ] && continue
    if [ "${restarts:-0}" != "0" ] || [ "$reason" = "CrashLoopBackOff" ] || [ "$reason" = "Error" ]; then
      # Confirm it's the migration race (current OR previous container log).
      L=$(kubectl -n "$NS_REPRO" logs "$pod" --tail=200 2>/dev/null; kubectl -n "$NS_REPRO" logs "$pod" -p --tail=200 2>/dev/null) || true
      if echo "$L" | grep -Eq "$ERR_RE"; then RACE=1; CRASH_POD="$pod"; break; fi
    fi
  done < <(kubectl -n "$NS_REPRO" get pods -l app=brain \
             -o 'custom-columns=N:.metadata.name,R:.status.containerStatuses[0].restartCount,W:.status.containerStatuses[0].state.waiting.reason' \
             --no-headers 2>/dev/null || true)
  [ "$RACE" -eq 1 ] && break
  # Also stop early if the fleet went fully Ready without any race (nothing to see).
  READY=$(kubectl -n "$NS_REPRO" get deploy/brain -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo 0)
  [ "${READY:-0}" = "3" ] && break
  sleep 2
done

if [ "$RACE" -eq 1 ]; then
  REPRO_RESULT="RACE REPRODUCED — replica $CRASH_POD crash-looped on the unlocked ensure_ledger (issue #24)"
  err "PHASE 2: RACE REPRODUCED on pod $CRASH_POD"
  echo "---- crashing pod log evidence (matching lines) ----"
  { kubectl -n "$NS_REPRO" logs "$CRASH_POD" --tail=200 2>/dev/null; kubectl -n "$NS_REPRO" logs "$CRASH_POD" -p --tail=200 2>/dev/null; } \
    | grep -E "$ERR_RE" | head -8 || true
  echo "----------------------------------------------------"
  kubectl -n "$NS_REPRO" get pods -l app=brain -o wide || true
else
  ok "PHASE 2: $REPRO_RESULT"
  echo "(the seed-1-then-scale workaround pinned by phase 1 is what avoids this — see issue #24)"
fi
kubectl delete namespace "$NS_REPRO" --wait=false >/dev/null 2>&1 || true

# ============================================================================
log "RESULT"
echo "phase 1 (regression guard): $ASSERT_LINE"
echo "phase 2 (diagnostic):       $REPRO_RESULT"
ok "\nCOLD START E2E PASS: the seed-1-then-scale workaround for issue #24 held — a 3-pod fleet drained $M concurrent durable runs exactly-once (no loss, no double-drive). [phase 2 is diagnostic-only]"
exit 0
