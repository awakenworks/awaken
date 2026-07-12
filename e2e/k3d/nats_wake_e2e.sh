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
CLUSTER="awaken-nats-wake"
IMAGE="awaken-topology:latest"
NS="awaken-nats-wake"
LOCAL_PORT="${NATS_WAKE_LOCAL_PORT:-38651}"
M="${1:-12}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
NODE="k3d-$CLUSTER-server-0"
DRIVER="$REPO_ROOT/e2e/k3d/scaling_driver.ts"   # identical submit logic; reused
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

# psql on the postgres pod (authoritative source of truth); -tA = bare scalar.
psql_scalar() { kubectl -n "$NS" exec deploy/postgres -- env PGPASSWORD=test psql -U postgres -d awaken -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }

export CARGO_CACHE_AUTOCLEAN=0

log "1/5 build the server binary on the host (rustc 1.96, --features nats)"
RUSTUP_TOOLCHAIN=1.96.0 cargo build -q -p awaken-server-local --bin awaken-server-local --features nats
BIN=$(RUSTUP_TOOLCHAIN=1.96.0 cargo build -p awaken-server-local --bin awaken-server-local --features nats --message-format=json 2>/dev/null \
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

log "3/5 create single-node k3d cluster $CLUSTER"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
# Relax the kubelet disk-eviction thresholds: on a busy dev host the shared disk can
# sit past k3s's default nodefs/imagefs<10%, tainting the node DiskPressure and
# refusing to schedule pods. This is a test box, not a capacity test, so push to ~2%.
EVICT="eviction-hard=imagefs.available<2%,nodefs.available<2%"
k3d cluster create "$CLUSTER" --agents 0 --wait --timeout 180s \
  --k3s-arg "--kubelet-arg=$EVICT@server:*" >/dev/null

log "4/5 side-load images into the cluster (single-platform tars → the node)"
PAUSE_IMG=$(docker exec "$NODE" sh -c 'grep -hoE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null)
PAUSE_IMG=${PAUSE_IMG:-rancher/mirrored-pause:3.6}
COREDNS_IMG=$(kubectl -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
COREDNS_IMG=${COREDNS_IMG:-rancher/mirrored-coredns-coredns:1.10.1}
for img in "$PAUSE_IMG" "$COREDNS_IMG" postgres:16 nats:2; do
  docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
done
TARDIR=$(mktemp -d)
docker save --platform linux/amd64 -o "$TARDIR/app.tar"     "$IMAGE"
docker save --platform linux/amd64 -o "$TARDIR/pause.tar"   "$PAUSE_IMG"
docker save --platform linux/amd64 -o "$TARDIR/coredns.tar" "$COREDNS_IMG"
docker save --platform linux/amd64 -o "$TARDIR/pg.tar"      postgres:16
docker save --platform linux/amd64 -o "$TARDIR/nats.tar"    nats:2
k3d image import "$TARDIR"/app.tar "$TARDIR"/pause.tar "$TARDIR"/coredns.tar "$TARDIR"/pg.tar "$TARDIR"/nats.tar -c "$CLUSTER" >/dev/null
rm -rf "$TARDIR"
kubectl -n kube-system delete pod -l k8s-app=kube-dns >/dev/null 2>&1 || true
kubectl -n kube-system rollout status deploy/coredns --timeout=90s
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "5/5 apply the fleet and drive the nats-wake + consistency scenario (M=$M)"
kubectl -n "$NS" apply -f "$DEPLOY_DIR/nats-wake-postgres.yaml" >/dev/null
echo "waiting for nats + postgres..."
kubectl -n "$NS" rollout status deploy/nats --timeout=120s
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
# Serialize the cold-start migration: bring up ONE brain first (it creates the schema),
# THEN scale to the full fleet — otherwise N pods race in scoped-migration's unlocked
# ledger bootstrap (CREATE TABLE IF NOT EXISTS → pg_type_typname_nsp_index).
echo "seed one brain to run migrations, then scale to the fleet..."
kubectl -n "$NS" scale deploy/brain --replicas=1 >/dev/null
kubectl -n "$NS" rollout status deploy/brain --timeout=120s
kubectl -n "$NS" scale deploy/brain --replicas=3 >/dev/null
echo "waiting for the brain fleet (3 replicas)..."
if ! kubectl -n "$NS" rollout status deploy/brain --timeout=150s; then
  err "brain fleet never became ready"; kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" logs deploy/brain --tail=25 || true; exit 1
fi

# Port-forward the load-balanced Service so submissions fan out across the fleet.
LOCAL_PORT=$((LOCAL_PORT + 1))
kubectl -n "$NS" port-forward svc/brain "$LOCAL_PORT":3000 >/tmp/nats_wake_pf.log 2>&1 &
PF_PID=$!
READY=""
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/probe/messages" 2>/dev/null; then READY=1; break; fi
  sleep 1
done
[ -n "$READY" ] || { err "port-forward never served HTTP 200"; exit 1; }

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

if [ "${GOT:-0}" = "$WANT" ] && [ "${DISTINCT:-0}" = "$M" ] && [ "${BADROWS:-1}" = "0" ]; then
  ok "\nK3D NATS-WAKE E2E PASS: a 3-pod fleet drained $M concurrent durable runs from one shared Postgres queue, waking each other over NATS (pg-notify disabled) — every thread committed exactly once (no loss, no double-drive)."
  exit 0
else
  err "\nK3D NATS-WAKE E2E FAIL: total=$GOT/$WANT distinct=$DISTINCT/$M bad=$BADROWS/0"
  kubectl -n "$NS" logs deploy/brain --tail=30 || true
  exit 1
fi
