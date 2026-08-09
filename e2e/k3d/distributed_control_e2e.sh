#!/usr/bin/env bash
# Full black-box K3D/K3S validation for the split production roles. Tests know
# one API endpoint only. kubectl/docker are used solely to deploy, inject faults,
# and collect isolation evidence; no assertion calls a role Service directly.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/e2e/k3d/harness.sh"
CLUSTER="awaken-adr71"
IMAGE="awaken-roles:latest"
NS="awaken-adr71"
API_PORT="${ADR71_API_PORT:-38801}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
DRIVER="$REPO_ROOT/e2e/k3d/distributed_control_driver.ts"
API_PF_LOG="${TMPDIR:-/tmp}/adr71_api_pf.log"
API_PF=""
STOPPED_NODE=""
export CARGO_CACHE_AUTOCLEAN=0

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok() { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

cleanup() {
  local status=$?
  if [ "$status" -ne 0 ] && [ "${ADR71_KEEP_FAILED_CLUSTER:-0}" = "1" ]; then
    err "retaining failed k3d cluster $CLUSTER for diagnostics"
    return
  fi
  [ -z "$STOPPED_NODE" ] || docker start "$STOPPED_NODE" >/dev/null 2>&1 || true
  [ -z "$API_PF" ] || kill "$API_PF" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d_delete_cluster "$CLUSTER"
  rm -f "$DEPLOY_DIR/awaken" "$DEPLOY_DIR/awaken-server"
}
trap cleanup EXIT

k3d_admit_or_exit "ADR-0071 distributed E2E"

start_public_endpoint() {
  if [ -n "$API_PF" ]; then
    kill "$API_PF" 2>/dev/null || true
    wait "$API_PF" 2>/dev/null || true
    API_PF=""
  fi
  API_PORT=$(k3d_available_port "$API_PORT")
  API_URL="http://127.0.0.1:$API_PORT"
  for _ in $(seq 1 90); do
    if [ -z "$API_PF" ] || ! kill -0 "$API_PF" 2>/dev/null; then
      [ -z "$API_PF" ] || wait "$API_PF" 2>/dev/null || true
      API_PF=$(k3d_start_port_forward "$NS" svc/awaken-api "$API_PORT" 8080 "$API_PF_LOG")
    fi
    curl -fsS -o /dev/null "$API_URL/readyz" && return 0
    sleep 1
  done
  err "single public API endpoint never became ready"
  cat "$API_PF_LOG" 2>/dev/null || true
  return 1
}

diagnostics() {
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" get events --sort-by=.lastTimestamp | tail -60 || true
  for app in control coordinator worker provider postgres-primary postgres-standby; do
    kubectl -n "$NS" logs -l "app=$app" --all-containers --tail=100 --prefix || true
  done
}
trap diagnostics ERR

wait_roles() {
  kubectl -n "$NS" rollout status deploy/control --timeout=240s
  kubectl -n "$NS" rollout status deploy/coordinator --timeout=240s
  kubectl -n "$NS" rollout status deploy/provider --timeout=240s
  kubectl -n "$NS" rollout status deploy/awaken-api --timeout=240s
  kubectl -n "$NS" rollout status statefulset/worker --timeout=240s
}

log "1/10 build production roles and the protocol-only Provider fixture"
if [ "${ADR71_REUSE_IMAGE:-0}" != "1" ]; then
  AWAKEN_BIN=$(resolve_cargo_executable awaken-cli awaken)
  SCENARIO_BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host)
  [ -n "$AWAKEN_BIN" ] && [ -n "$SCENARIO_BIN" ] \
    || { err "could not resolve executables"; exit 1; }
  cp "$AWAKEN_BIN" "$DEPLOY_DIR/awaken"
  cp "$SCENARIO_BIN" "$DEPLOY_DIR/awaken-server"
  # Cargo's debug executables retain hundreds of MiB of symbols that are not
  # exercised inside the black-box cluster. Strip only the disposable image
  # copies so K3D does not need a second multi-GiB tar while importing the image.
  strip --strip-unneeded "$DEPLOY_DIR/awaken" "$DEPLOY_DIR/awaken-server"
fi

log "2/10 create three K3S worker nodes and import immutable test images"
if [ "${ADR71_REUSE_IMAGE:-0}" = "1" ]; then
  docker image inspect "$IMAGE" >/dev/null 2>&1 \
    || { err "ADR71_REUSE_IMAGE requires an existing $IMAGE"; exit 1; }
else
  docker build --load -q -t "$IMAGE" \
    --build-arg AUXILIARY_EXECUTABLE=awaken \
    -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null
fi
IMAGE_ID=$(docker image inspect "$IMAGE" --format '{{.Id}}')
echo "using immutable test image $IMAGE_ID"
k3d_create_cluster "$CLUSTER" 3 1
k3d_import_images "$CLUSTER" "$IMAGE" postgres:16 nginx:1.27-alpine
# Cause/effect decision table: D1 one default CoreDNS replica + its node stops ->
# replacement application Pods cannot resolve authority Services; D2 two replicas
# + the built-in hostname spread constraint -> one node loss retains DNS; D3 fewer
# than two distinct ready placements -> reject the topology before business tests.
kubectl -n kube-system scale deployment/coredns --replicas=2 >/dev/null
kubectl -n kube-system rollout status deployment/coredns --timeout=120s
DNS_NODES=$(kubectl -n kube-system get pod -l k8s-app=kube-dns \
  -o jsonpath='{range .items[?(@.status.containerStatuses[0].ready==true)]}{.spec.nodeName}{"\n"}{end}' \
  | sort -u | grep -c .)
[ "$DNS_NODES" = "2" ] || { err "CoreDNS is not ready on two distinct nodes"; exit 1; }
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "3/10 deploy replicated authorities, real Workers, and PostgreSQL standby"
# Cause/effect decision table:
# K1 delayed primary Service + bounded base-backup reconnect -> standby retries
# the same replication source and becomes ready within the rollout deadline;
# K2 primary + streaming standby + bounded per-replica pools -> all four owner
# databases migrate without exhausting the shared server connection budget;
# K3 two anti-affined role replicas -> one Pod/node loss leaves an endpoint;
# K4 Worker template + identity-specific signer + exact CSI-style projection ->
# real Worker starts without seal key/DB and can resolve Credential/File/Memory/Skill;
# K5 one Ingress -> public paths route by owner and every private path stays absent.
kubectl -n "$NS" apply -k "$DEPLOY_DIR/distributed-control" >/dev/null
kubectl -n "$NS" rollout status statefulset/postgres-primary --timeout=180s
kubectl -n "$NS" rollout status statefulset/postgres-standby --timeout=180s
kubectl -n "$NS" wait --for=condition=complete job/control-migrate --timeout=240s
kubectl -n "$NS" wait --for=condition=complete job/coordinator-migrate --timeout=240s
wait_roles || { diagnostics; exit 1; }
start_public_endpoint

log "4/10 exercise the complete public configuration-to-response path"
BOOTSTRAP=$(node "$DRIVER" bootstrap "$API_URL" | tail -1)
[ "${BOOTSTRAP%% *}" = "OK" ] || { err "bootstrap failed: $BOOTSTRAP"; diagnostics; exit 1; }
read -r _ DEPLOYMENT_ID SESSION_ID ENVIRONMENT_ID FILE_ID MEMORY_ID SKILL_ID <<<"$BOOTSTRAP"
ok "one endpoint completed Credential/File/Memory/Skill/Agent/Deployment/Session/Provider flow"

log "5/10 prove Worker process isolation and material realization"
WORKER_SPEC=$(kubectl -n "$NS" get statefulset worker -o json)
if printf '%s' "$WORKER_SPEC" | grep -Eq 'postgres://|DATABASE_URL|seal-key|control-auth|coordinator-auth'; then
  err "Worker workload received authority database or seal configuration"
  exit 1
fi
for pod in worker-0 worker-1; do
  kubectl -n "$NS" exec "$pod" -- /bin/sh -ec \
    '! grep -q ":1538 " /proc/net/tcp /proc/net/tcp6 2>/dev/null'
done
REALIZED=$(kubectl -n "$NS" exec worker-0 -- /bin/sh -ec \
  "grep -R -l -E 'ADR71-(FILE|MEMORY|SKILL)-MATERIALIZED' /var/lib/awaken 2>/dev/null || true")
REALIZED+=$(kubectl -n "$NS" exec worker-1 -- /bin/sh -ec \
  "grep -R -l -E 'ADR71-(FILE|MEMORY|SKILL)-MATERIALIZED' /var/lib/awaken 2>/dev/null || true")
[ -n "$REALIZED" ] || { err "no Worker sandbox contained the pinned Resource markers"; diagnostics; exit 1; }
ok "Workers have no DB socket/config/seal and realized snapshot-pinned resources"

log "6/10 remove the entire Coordinator tier, then recover the same publication"
# C1 Coordinator unavailable -> public publication fails retryably and Workers
# revoke their local claim admission; C2 Coordinator restored -> durable
# publication remains available, but drained Worker incarnations terminate;
# C3 RestartPolicy replacement -> fresh incarnations register and become Ready.
WORKER_CONTAINERS_BEFORE=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].containerID}{"\n"}{end}')
kubectl -n "$NS" scale deploy/coordinator --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod -l app=coordinator --timeout=120s
node "$DRIVER" unavailable-publication "$API_URL"
for _ in $(seq 1 90); do
  DRAINED=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].ready}{"\n"}{end}' | grep -c '^false$' || true)
  [ "$DRAINED" = "2" ] && break
  sleep 1
done
[ "${DRAINED:-0}" = "2" ] || { err "Workers did not fail closed after losing Coordinator authority"; exit 1; }
kubectl -n "$NS" scale deploy/coordinator --replicas=2 >/dev/null
kubectl -n "$NS" rollout status deploy/coordinator --timeout=240s
node "$DRIVER" retry-publication "$API_URL"
kubectl -n "$NS" rollout status statefulset/worker --timeout=240s
WORKER_CONTAINERS_AFTER=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].containerID}{"\n"}{end}')
[ "$WORKER_CONTAINERS_BEFORE" != "$WORKER_CONTAINERS_AFTER" ] || { err "drained Worker containers were reused"; exit 1; }
ok "Coordinator outage failed closed; RestartPolicy restored fresh Worker incarnations"

log "7/10 inject single-Pod and in-flight Worker chaos"
# Cause/effect decision table:
# C1 one authority Pod removed -> the other replica serves the same durable IDs;
# C2 peer Coordinator commits a Run -> the request owner reads the canonical
# recovery snapshot and projects exactly one assistant response (never idle-only);
# C3 Worker dies after claims -> leases are reclaimed, old epochs are fenced, and
# every accepted marker reaches one and only one terminal Provider response.
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=control -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-CONTROL-LOSS
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=coordinator -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-COORDINATOR-LOSS
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=provider -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-PROVIDER-LOSS
node "$DRIVER" batch "$API_URL" "$SESSION_ID" "$ENVIRONMENT_ID" ADR71-CHAOS-SLOW 12 &
BATCH_PID=$!
sleep 1
kubectl -n "$NS" delete pod worker-0 --grace-period=0 --force >/dev/null 2>&1
wait "$BATCH_PID"
wait_roles || { diagnostics; exit 1; }
ok "Pod loss and an in-flight Worker crash preserved exactly-once terminal responses"

log "8/10 stop one K3D agent node and verify public continuity"
# Cause/effect decision table:
# N1 hard node stop + Ready still True -> no business write during the detection window;
# N2 Ready False/Unknown -> failed role endpoints are evicted; N2b a StatefulSet
# Worker object can remain Terminating while its kubelet is unreachable, so after
# the node fence proves that process cannot execute, force-removing only that stale
# API object permits the same lease-fenced ordinal to restart on a healthy node;
# N3 the surviving spread CoreDNS replica resolves replacement dependencies and
# every application role becomes Ready again;
# N4 stable public GETs -> issue the non-idempotent Event exactly once;
# N5 surviving/replaced Provider and Worker -> one terminal response, never an
# error hidden by retrying the accepted Event.
DB_NODES=$(kubectl -n "$NS" get pod -l 'database-role in (primary,standby)' -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}')
# K3D agent nodes intentionally do not rely on the optional
# `node-role.kubernetes.io/agent` label. Select from the cluster-owned node-name
# namespace, then exclude both database placements.
while IFS= read -r candidate; do
  if ! grep -qx "$candidate" <<<"$DB_NODES"; then STOPPED_NODE="$candidate"; break; fi
done < <(kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' \
  | grep "^k3d-${CLUSTER}-agent-" || true)
[ -n "$STOPPED_NODE" ] || { err "no non-database agent node available for node chaos"; exit 1; }
docker stop "$STOPPED_NODE" >/dev/null
# A hard node stop has a bounded detection window: Pods on that node may remain
# Ready and therefore routable until the node controller changes Ready away
# from True. An unreachable node reports Unknown, while a reachable unhealthy
# node reports False, so both states fence traffic.
for _ in $(seq 1 120); do
  NODE_READY=$(kubectl get node "$STOPPED_NODE" \
    -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')
  [ "$NODE_READY" != "True" ] && break
  sleep 1
done
[ "${NODE_READY:-True}" != "True" ] \
  || { err "stopped node remained Ready=True beyond its detection deadline"; exit 1; }
# A Deployment can create a differently named replacement while the unreachable
# Pod object terminates. A StatefulSet cannot create the same ordinal until that
# object disappears. This is safe only after the Node Ready fence above: the old
# Worker process is physically stopped, and any later kubelet return is also
# rejected by the durable Worker generation/Run lease before effects.
while read -r failed_worker failed_node; do
  [ "$failed_node" != "$STOPPED_NODE" ] || kubectl -n "$NS" delete pod "$failed_worker" \
    --grace-period=0 --force >/dev/null 2>&1
done < <(kubectl -n "$NS" get pod -l app=worker \
  -o custom-columns=NAME:.metadata.name,NODE:.spec.nodeName --no-headers)
# Wait for that canonical Kubernetes state transition before reconnecting the
# black-box client; the public API assertions below still use no private route.
# `kubectl port-forward service/...` selects one backing Pod when it starts. If
# that Pod was on the stopped node, rebind the same public port to the surviving
# Service endpoint after Kubernetes has fenced the failed node, before issuing
# a new business write. This repairs only the test transport; it never replays
# an ambiguous request.
start_public_endpoint
for _ in $(seq 1 60); do
  DNS_SURVIVORS=$(kubectl -n kube-system get pod -l k8s-app=kube-dns \
    -o custom-columns=NODE:.spec.nodeName,READY:.status.containerStatuses[0].ready --no-headers \
    | grep -v "^${STOPPED_NODE} " | grep -c ' true$' || true)
  [ "$DNS_SURVIVORS" -ge 1 ] && break
  sleep 1
done
[ "${DNS_SURVIVORS:-0}" -ge 1 ] \
  || { err "no Ready CoreDNS replica survived the stopped node"; exit 1; }
wait_roles || { diagnostics; exit 1; }
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-NODE-LOSS
docker start "$STOPPED_NODE" >/dev/null
RESTORED_NODE="$STOPPED_NODE"
STOPPED_NODE=""
kubectl wait --for=condition=Ready "node/$RESTORED_NODE" --timeout=180s

log "9/10 promote the replayed PostgreSQL standby behind the stable service"
# D1 standby has replayed the observed writer LSN -> promotion is lossless for
# accepted durable facts; D2 writer Pod removed + Service atomically retargeted ->
# clients reconnect through the same DB name; D3 API write/read -> no stale truth.
PRIMARY_LSN=$(kubectl -n "$NS" exec postgres-primary-0 -- psql -U postgres -d awaken -tAc 'SELECT pg_current_wal_lsn()' | tr -d '[:space:]')
for _ in $(seq 1 120); do
  REPLAYED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d awaken -tAc \
    "SELECT COALESCE(pg_last_wal_replay_lsn() >= '$PRIMARY_LSN'::pg_lsn, false)" | tr -d '[:space:]')
  [ "$REPLAYED" = "t" ] && break
  sleep 1
done
[ "${REPLAYED:-f}" = "t" ] || { err "standby did not replay writer LSN $PRIMARY_LSN"; exit 1; }
kubectl -n "$NS" scale statefulset/postgres-primary --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod/postgres-primary-0 --timeout=120s
kubectl -n "$NS" exec postgres-standby-0 -- \
  gosu postgres pg_ctl promote -D /var/lib/postgresql/data/pgdata -w
kubectl -n "$NS" patch service postgres --type merge -p '{"spec":{"selector":{"database-role":"standby"}}}' >/dev/null
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-DATABASE-FAILOVER

log "10/10 run concurrent pressure through the same endpoint and audit final truth"
node "$DRIVER" load "$API_URL" "$ENVIRONMENT_ID"
DATABASES=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d awaken -tAc \
  "SELECT count(*) FROM pg_database WHERE datname IN ('control','credentials','runtime','resources')" | tr -d '[:space:]')
[ "$DATABASES" = "4" ] || { err "expected four isolated owner databases, got $DATABASES"; exit 1; }
kubectl -n "$NS" get pods -o custom-columns=POD:.metadata.name,APP:.metadata.labels.app,NODE:.spec.nodeName --no-headers

ok "K3D/K3S FULL E2E PASS: one public API covered configuration, resources, credentials, publication, deployment, real Worker execution and response; tier, Worker, node, provider, and PostgreSQL faults recovered without duplicate terminal output; concurrent load met the public latency/error gates."
