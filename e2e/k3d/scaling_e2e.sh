#!/usr/bin/env bash
# Horizontal scaling + consistency e2e (ADR-0019) on a real multi-node k3d cluster.
#
# NOTE: this test drove the fix for a real concurrency bug — a durable fleet used to
# drain only 1 of a concurrent burst because `PostgresCommitCoordinator` allocated the
# commit sequence from a per-process in-memory counter, so concurrent commits collided
# on `runtime_commit_pkey`. Now the sequence is DB-atomic (advisory-lock + MAX+1), so
# the fleet drains the whole burst exactly once. The script still scales the brain 1→N
# to dodge a SEPARATE, still-open issue: scoped-migration's unlocked `ensure_ledger`
# races on `pg_type_typname_nsp_index` when N pods cold-start a fresh DB together.
# A fleet of brain pods behind one Service drains ONE shared Postgres dispatch queue.
# We fire M concurrent durable submissions and then assert, AUTHORITATIVELY against
# Postgres (not a per-pod cached projection), that every run was driven EXACTLY ONCE:
#   - no loss : M distinct threads each have committed messages
#   - no dup  : every thread has exactly 2 messages (1 user + 1 assistant echo)
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/scaling_e2e.sh [M]   (default M=15, from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
CLUSTER="awaken-scaling"
IMAGE="awaken-topology:latest"
NS="awaken-scaling"
LOCAL_PORT="${SCALING_LOCAL_PORT:-38631}"
M="${1:-15}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
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
  rm -f "$DEPLOY_DIR/awaken-server"
}
trap cleanup EXIT

# psql on the postgres pod (authoritative source of truth); -tA = bare scalar.
psql_scalar() { kubectl -n "$NS" exec deploy/postgres -- env PGPASSWORD=test psql -U postgres -d awaken -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }

log "1/5 build the server binary on the host (rustc 1.96)"
RUSTUP_TOOLCHAIN=1.96.0 cargo build -q -p awaken-scenario-host --bin awaken-scenario-host
BIN=$(RUSTUP_TOOLCHAIN=1.96.0 cargo build -p awaken-scenario-host --bin awaken-scenario-host --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-server': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { echo 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server"

log "2/5 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null

log "3/5 create MULTI-node k3d cluster $CLUSTER (server + 2 agents)"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
# Relax the kubelet disk-eviction thresholds: on a busy dev host (docker images +
# Rust target dir) the shared disk can sit past k3s's default nodefs/imagefs<10%,
# which taints the node DiskPressure and refuses to schedule the postgres pod. This
# is a test box, not a capacity test, so push eviction to ~2%.
EVICT="eviction-hard=imagefs.available<2%,nodefs.available<2%"
k3d cluster create "$CLUSTER" --agents 2 --wait --timeout 180s \
  --k3s-arg "--kubelet-arg=$EVICT@server:*" \
  --k3s-arg "--kubelet-arg=$EVICT@agent:*" >/dev/null

log "4/5 side-load images into the cluster (single-platform tars → all nodes)"
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
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "5/5 apply the fleet and drive the scaling + consistency scenario (M=$M)"
kubectl -n "$NS" apply -f "$DEPLOY_DIR/scaling-postgres.yaml" >/dev/null
echo "waiting for postgres..."
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
# Serialize the cold-start migration: bring up ONE brain first (it creates the
# schema), THEN scale to the full fleet — otherwise N pods race in scoped-migration's
# unlocked ledger bootstrap (CREATE TABLE IF NOT EXISTS → pg_type_typname_nsp_index).
echo "seed one brain to run migrations, then scale to the fleet..."
kubectl -n "$NS" scale deploy/brain --replicas=1 >/dev/null
kubectl -n "$NS" rollout status deploy/brain --timeout=120s
kubectl -n "$NS" scale deploy/brain --replicas=3 >/dev/null
echo "waiting for the brain fleet (3 replicas)..."
if ! kubectl -n "$NS" rollout status deploy/brain --timeout=150s; then
  err "brain fleet never became ready"; kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" logs deploy/brain --tail=25 || true; exit 1
fi
echo "fleet placement (should span nodes):"
kubectl -n "$NS" get pods -l app=brain -o custom-columns=POD:.metadata.name,NODE:.spec.nodeName --no-headers || true

# Port-forward the load-balanced Service so submissions fan out across the fleet.
LOCAL_PORT=$((LOCAL_PORT + 1))
kubectl -n "$NS" port-forward svc/brain "$LOCAL_PORT":3000 >/tmp/scaling_pf.log 2>&1 &
PF_PID=$!
for _ in $(seq 1 60); do
  curl -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/probe/messages" 2>/dev/null && break
  sleep 1
done

log "fire $M concurrent durable submissions across the fleet"
R1=$(THREAD_PREFIX=scale node "$DRIVER" submit "http://127.0.0.1:$LOCAL_PORT" "$M" 2>&1 | tail -1) || true
[ "${R1%% *}" = "OK" ] || { err "concurrent submit failed: $R1"; kubectl -n "$NS" logs deploy/brain --tail=25 || true; exit 1; }
ok "all $M submissions accepted + enqueued ($R1)"

# Wait for the fleet to drain the queue: authoritative row count in Postgres.
log "wait for the fleet to drain, then assert consistency in Postgres"
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

if [ "${GOT:-0}" = "$WANT" ] && [ "${DISTINCT:-0}" = "$M" ] && [ "${BADROWS:-1}" = "0" ]; then
  ok "\nK3D SCALING E2E PASS: a 3-pod fleet drained $M concurrent durable runs from one shared Postgres queue — every thread committed exactly once (no loss, no double-drive)."
  exit 0
else
  err "\nK3D SCALING E2E FAIL: total=$GOT/$WANT distinct=$DISTINCT/$M bad=$BADROWS/0"
  kubectl -n "$NS" logs deploy/brain --tail=30 || true
  exit 1
fi
