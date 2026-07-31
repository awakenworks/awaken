#!/usr/bin/env bash
# ADR-0071 production-role E2E on a real multi-node k3d/k3s cluster.
#
# Control runs the production authoring/publication/Deployment router with only
# its model-publication SPI made deterministic. Coordinator is the shipped
# `awaken coordinator` process. A database-less Worker uses the canonical
# registration/claim/commit lifecycle. PostgreSQL separates Control,
# Credential, Coordinator runtime, and Resource data into distinct databases.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/e2e/k3d/harness.sh"
CLUSTER="awaken-adr71"
IMAGE="awaken-roles:latest"
NS="awaken-adr71"
CONTROL_PORT="${ADR71_CONTROL_PORT:-38801}"
COORDINATOR_PORT="${ADR71_COORDINATOR_PORT:-38851}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
DRIVER="$REPO_ROOT/e2e/k3d/distributed_control_driver.ts"
CONTROL_PF=""
COORDINATOR_PF=""
export CARGO_CACHE_AUTOCLEAN=0

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok() { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

cleanup() {
  [ -n "$CONTROL_PF" ] && kill "$CONTROL_PF" 2>/dev/null || true
  [ -n "$COORDINATOR_PF" ] && kill "$COORDINATOR_PF" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d_delete_cluster "$CLUSTER"
  rm -f "$DEPLOY_DIR/awaken" "$DEPLOY_DIR/awaken-server"
}
trap cleanup EXIT

if ! k3d_require_tools; then
  echo "k3d/kubectl/docker unavailable; skipping ADR-0071 distributed E2E"
  exit 0
fi

restart_forwards() {
  [ -n "$CONTROL_PF" ] && kill "$CONTROL_PF" 2>/dev/null || true
  [ -n "$COORDINATOR_PF" ] && kill "$COORDINATOR_PF" 2>/dev/null || true
  CONTROL_PORT=$(k3d_available_port "$((CONTROL_PORT + 1))")
  COORDINATOR_PORT=$(k3d_available_port "$((COORDINATOR_PORT + 1))")
  CONTROL_PF=$(k3d_start_port_forward "$NS" svc/control "$CONTROL_PORT" 3000 /tmp/adr71_control_pf.log)
  COORDINATOR_PF=$(k3d_start_port_forward "$NS" svc/coordinator "$COORDINATOR_PORT" 3000 /tmp/adr71_coordinator_pf.log)
  for _ in $(seq 1 60); do
    if curl -fsS -o /dev/null "http://127.0.0.1:$CONTROL_PORT/readyz" \
      && curl -fsS -o /dev/null "http://127.0.0.1:$COORDINATOR_PORT/readyz"; then
      return 0
    fi
    sleep 1
  done
  err "role port-forwards never became ready"
  cat /tmp/adr71_control_pf.log /tmp/adr71_coordinator_pf.log 2>/dev/null || true
  return 1
}

diagnostics() {
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" get events --sort-by=.lastTimestamp | tail -40 || true
  kubectl -n "$NS" logs deploy/control --tail=80 || true
  kubectl -n "$NS" logs deploy/coordinator --tail=80 || true
  kubectl -n "$NS" logs -l app=worker --tail=60 || true
}

log "1/7 build production and deterministic-edge executables"
AWAKEN_BIN=$(resolve_cargo_executable awaken-cli awaken)
SCENARIO_BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host)
[ -n "$AWAKEN_BIN" ] && [ -n "$SCENARIO_BIN" ] || { err "could not resolve executables"; exit 1; }
cp "$AWAKEN_BIN" "$DEPLOY_DIR/awaken"
cp "$SCENARIO_BIN" "$DEPLOY_DIR/awaken-server"

log "2/7 build and import the role image"
docker build --load -q -t "$IMAGE" \
  --build-arg AUXILIARY_EXECUTABLE=awaken \
  -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null
k3d_create_cluster "$CLUSTER" 1 1
k3d_import_images "$CLUSTER" "$IMAGE" postgres:16
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "3/7 apply isolated databases, migrations, Control, Coordinator, and Workers"
# Cause/effect decision table:
# K1 four component databases + two migrations -> every production role verifies
# existing schemas; K2 wrong private token -> 401/no mutation; K3 available
# Coordinator -> publication and launch acknowledged; K4 Coordinator outage after
# publication persistence -> retryable 503; K5 restore and retry -> one projection;
# K6 Control/Coordinator restart -> Deployment, Session, and response survive;
# K7 Worker receives no authority configuration and every private request is signed.
kubectl -n "$NS" apply -k "$DEPLOY_DIR/distributed-control" >/dev/null
kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
kubectl -n "$NS" wait --for=condition=complete job/control-migrate --timeout=180s
kubectl -n "$NS" wait --for=condition=complete job/coordinator-migrate --timeout=180s
for deployment in control coordinator worker; do
  if ! kubectl -n "$NS" rollout status "deploy/$deployment" --timeout=180s; then
    err "$deployment never became ready"
    diagnostics
    exit 1
  fi
done
restart_forwards

CONTROL_URL="http://127.0.0.1:$CONTROL_PORT"
COORDINATOR_URL="http://127.0.0.1:$COORDINATOR_PORT"

log "4/7 publish, launch a Deployment, and complete a remote Worker response"
BOOTSTRAP=$(node "$DRIVER" bootstrap "$CONTROL_URL" "$COORDINATOR_URL" | tail -1)
[ "${BOOTSTRAP%% *}" = "OK" ] || { err "bootstrap failed: $BOOTSTRAP"; diagnostics; exit 1; }
read -r _ DEPLOYMENT_ID SESSION_ID ENVIRONMENT_ID <<<"$BOOTSTRAP"
ok "publication -> registration -> Deployment launch -> Session response passed"

log "5/7 remove Coordinator and prove publication remains durably retryable"
kubectl -n "$NS" scale deploy/coordinator --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod -l app=coordinator --timeout=90s
node "$DRIVER" unavailable-publication "$CONTROL_URL" "$COORDINATOR_URL"
kubectl -n "$NS" scale deploy/coordinator --replicas=1 >/dev/null
kubectl -n "$NS" rollout status deploy/coordinator --timeout=180s
restart_forwards
CONTROL_URL="http://127.0.0.1:$CONTROL_PORT"
COORDINATOR_URL="http://127.0.0.1:$COORDINATOR_PORT"
node "$DRIVER" retry-publication "$CONTROL_URL" "$COORDINATOR_URL"
ok "unavailable registration failed retryably and recovered through the same port"

log "6/7 restart both authorities and verify durable truth plus execution"
kubectl -n "$NS" delete pod -l app=control --grace-period=0 --force >/dev/null 2>&1
kubectl -n "$NS" delete pod -l app=coordinator --grace-period=0 --force >/dev/null 2>&1
kubectl -n "$NS" rollout status deploy/control --timeout=180s
kubectl -n "$NS" rollout status deploy/coordinator --timeout=180s
restart_forwards
CONTROL_URL="http://127.0.0.1:$CONTROL_PORT"
COORDINATOR_URL="http://127.0.0.1:$COORDINATOR_PORT"
node "$DRIVER" verify-restart "$CONTROL_URL" "$COORDINATOR_URL" "$DEPLOYMENT_ID" "$SESSION_ID"

log "7/7 assert placement and database ownership evidence"
kubectl -n "$NS" get pods -o custom-columns=POD:.metadata.name,APP:.metadata.labels.app,NODE:.spec.nodeName --no-headers
DATABASES=$(kubectl -n "$NS" exec deploy/postgres -- env PGPASSWORD=test psql -U postgres -d awaken -tAc \
  "SELECT count(*) FROM pg_database WHERE datname IN ('control','credentials','runtime','resources')" | tr -d '[:space:]')
[ "$DATABASES" = "4" ] || { err "expected four isolated component databases, got $DATABASES"; exit 1; }
WORKER_ENV=$(kubectl -n "$NS" get deploy worker -o json)
if printf '%s' "$WORKER_ENV" | grep -Eq 'DATABASE_URL|postgres://|seal-key|registration-token|deployment-launch-token'; then
  err "Worker deployment received authority configuration"
  exit 1
fi

ok "K3D ADR-0071 E2E PASS: production Control and Coordinator crossed authenticated registration and launch boundaries, database-less Workers completed responses, outage retry and role restart preserved authoritative truth, and four component databases stayed isolated."
