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
WORKER_TLS_DIR=""
export CARGO_CACHE_AUTOCLEAN=0

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok() { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

cleanup() {
  local status="$1"
  [ -z "$WORKER_TLS_DIR" ] || rm -rf -- "$WORKER_TLS_DIR"
  if [ "$status" -ne 0 ] && [ "${ADR71_KEEP_FAILED_CLUSTER:-0}" = "1" ]; then
    err "retaining failed k3d cluster $CLUSTER for diagnostics"
    return
  fi
  [ -z "$STOPPED_NODE" ] || docker start "$STOPPED_NODE" >/dev/null 2>&1 || true
  [ -z "$API_PF" ] || kill "$API_PF" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d_delete_cluster_and_remove \
    "$CLUSTER" \
    "$DEPLOY_DIR/awaken-control" \
    "$DEPLOY_DIR/awaken-coordinator" \
    "$DEPLOY_DIR/awaken-server" \
    "$DEPLOY_DIR/awaken-worker"
}

k3d_admit_or_exit "ADR-0071 distributed E2E"
k3d_install_exit_cleanup cleanup

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

initialize_role_installation() {
  local role="$1"
  local job="${role}-initialize"
  # First-install decision table: KFI1 a fresh cluster has no binding and the
  # ordinary rollout Job fails closed; KFI2 this explicit one-time Job carries
  # the fixture audit reference and initializes only the role-owned targets;
  # KFI3 every later rollout keeps using the manifest's ordinary exact-only
  # migration command. This helper reuses that same role binary, config Secret,
  # auth Secret, label/network boundary, and migration entry point.
  kubectl -n "$NS" apply -f - >/dev/null <<YAML
apiVersion: batch/v1
kind: Job
metadata: { name: $job }
spec:
  backoffLimit: 0
  template:
    metadata: { labels: { app: ${role}-migrate } }
    spec:
      restartPolicy: Never
      containers:
        - name: initialize
          image: $IMAGE
          imagePullPolicy: Never
          command: ["/usr/local/bin/awaken-$role", "database", "migrate", "--config", "/etc/awaken/$role.toml", "--initialize-installation", "--initialization-reference", "adr71-fresh-$role"]
          volumeMounts:
            - { name: config, mountPath: /etc/awaken, readOnly: true }
            - { name: auth, mountPath: /etc/awaken-auth, readOnly: true }
      volumes:
        - name: config
          secret: { secretName: adr71-role-config }
        - name: auth
          secret: { secretName: adr71-$role-auth }
YAML
  kubectl -n "$NS" wait --for=condition=complete "job/$job" --timeout=240s
}

log "1/10 build production roles and the protocol-only Provider fixture"
if [ "${ADR71_REUSE_IMAGE:-0}" != "1" ]; then
  CONTROL_BIN=$(resolve_cargo_executable awaken-cli awaken-control)
  COORDINATOR_BIN=$(resolve_cargo_executable awaken-cli awaken-coordinator)
  WORKER_BIN=$(resolve_cargo_executable awaken-worker awaken-worker --features container-k8s)
  SCENARIO_BIN=$(resolve_cargo_executable awaken-scenario-host awaken-scenario-host)
  [ -n "$CONTROL_BIN" ] && [ -n "$COORDINATOR_BIN" ] && [ -n "$WORKER_BIN" ] && [ -n "$SCENARIO_BIN" ] \
    || { err "could not resolve executables"; exit 1; }
  cp "$CONTROL_BIN" "$DEPLOY_DIR/awaken-control"
  cp "$COORDINATOR_BIN" "$DEPLOY_DIR/awaken-coordinator"
  cp "$WORKER_BIN" "$DEPLOY_DIR/awaken-worker"
  cp "$SCENARIO_BIN" "$DEPLOY_DIR/awaken-server"
  # Cargo's debug executables retain hundreds of MiB of symbols that are not
  # exercised inside the black-box cluster. Strip only the disposable image
  # copies so K3D does not need a second multi-GiB tar while importing the image.
  strip --strip-unneeded "$DEPLOY_DIR/awaken-control" "$DEPLOY_DIR/awaken-coordinator" "$DEPLOY_DIR/awaken-worker" "$DEPLOY_DIR/awaken-server"
fi

log "2/10 create three K3S worker nodes and import immutable test images"
"$REPO_ROOT/deploy/images/sandbox/build.sh" --ensure-hand awaken-sandbox:local ""
if [ "${ADR71_REUSE_IMAGE:-0}" = "1" ]; then
  docker image inspect "$IMAGE" >/dev/null 2>&1 \
    || { err "ADR71_REUSE_IMAGE requires an existing $IMAGE"; exit 1; }
else
  k3d_docker_build --load -q -t "$IMAGE" \
    --build-arg AUXILIARY_EXECUTABLE=awaken-control \
    -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null
fi
IMAGE_ID=$(docker image inspect "$IMAGE" --format '{{.Id}}')
echo "using immutable test image $IMAGE_ID"
# An absolute threshold keeps this nested K3d qualification independent of the
# host volume's total size. Imported role images live in each Docker-backed K3s
# node's small writable layer, so reserve 128 MiB there; the host runner owns
# the separate capacity preflight and cleanup policy.
k3d_create_cluster "$CLUSTER" 3 128Mi
k3d_import_images "$CLUSTER" "$IMAGE" awaken-sandbox:local postgres:16 nginx:1.27-alpine
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

# Worker transport TLS cause/effect decision table:
# C1 one explicit DNS SAN for the Coordinator Service + C2 the matching private
# CA is mounted into Workers + C3 the leaf/key volume is mounted only into the
# TLS sidecar container + C4 Service and NetworkPolicy route Workers only to
# sidecar port 3443 -> E1 WorkerUpstream admits HTTPS and validates the private
# cluster identity without a global TLS bypass. Any false cause -> E2 the static
# deployment contract fails for a missing input, or the existing TLS handshake
# fails before signed registration for a mismatched CA/SAN.
# K1 tls_identity_fixture.mjs is the sole E2E certificate generator; production
# trust admission remains owned by WorkerUpstream. D1=C1+C2+C3+C4=>E1;
# D2=!C1||!C2||!C3||!C4=>E2. The live rollout below is the positive DNS-SAN/
# sidecar rule; the credential-materialization E2E reuses the fixture for IP SAN.
WORKER_TLS_DIR=$(mktemp -d "${TMPDIR:-/tmp}/adr71-worker-tls.XXXXXX")
node "$REPO_ROOT/e2e/fixtures/tls_identity_fixture.mjs" \
  "$WORKER_TLS_DIR" "Awaken ADR71 Worker CA" coordinator DNS:coordinator
kubectl -n "$NS" create configmap adr71-worker-upstream-ca \
  --from-file=ca.crt="$WORKER_TLS_DIR/worker-test-ca.pem" >/dev/null
kubectl -n "$NS" create secret tls adr71-worker-upstream-tls \
  --cert="$WORKER_TLS_DIR/worker-test-server.pem" \
  --key="$WORKER_TLS_DIR/worker-test-server.key" >/dev/null

# Kube-API egress cause/effect decision table: A1 a CNI evaluates the Service
# ClusterIP before DNAT -> a ClusterIP rule may work but is not portable; A2 it
# evaluates the backend after DNAT (K3S) -> that same rule silently blocks every
# Session Pod operation; A3 the harness resolves the authoritative Endpoint and
# admits only its exact IP/port -> the Worker K8s adapter can operate while all
# other HTTPS egress remains denied. The namespace-scoped ServiceAccount remains
# the independent authorization fence. Invalid coordinates fail before apply.
KUBE_API_IP=$(kubectl get endpoints kubernetes -o jsonpath='{.subsets[0].addresses[0].ip}')
KUBE_API_PORT=$(kubectl get endpoints kubernetes -o jsonpath='{.subsets[0].ports[0].port}')
[[ "$KUBE_API_IP" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] \
  && [[ "$KUBE_API_PORT" =~ ^[0-9]+$ ]] \
  || { err "invalid Kubernetes API endpoint ${KUBE_API_IP}:${KUBE_API_PORT}"; exit 1; }
kubectl -n "$NS" apply -f - >/dev/null <<YAML
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: worker-kube-api }
spec:
  podSelector: { matchLabels: { app: worker } }
  policyTypes: [Egress]
  egress:
    - to: [{ ipBlock: { cidr: "${KUBE_API_IP}/32" } }]
      ports: [{ protocol: TCP, port: ${KUBE_API_PORT} }]
YAML

log "3/10 deploy replicated authorities, real Workers, and PostgreSQL standby"
# Cause/effect decision table:
# K1 delayed primary Service + bounded base-backup reconnect -> standby retries
# the same replication source and becomes ready within the rollout deadline;
# K2 primary + streaming standby + bounded per-replica pools -> all four owner
# databases migrate without exhausting the shared server connection budget;
# K3 two anti-affined role replicas -> one Pod/node loss leaves an endpoint;
# K4 Worker template + identity-specific signer + private-CA HTTPS edge +
# namespace-scoped K8s runtime -> real Worker starts without seal key/DB,
# creates an isolated Session Pod, and resolves exact Credential/File/Memory/
# Skill projections there; Worker egress cannot bypass TLS on port 3001;
# K5 one Ingress -> public paths route by owner and every private path stays absent.
kubectl -n "$NS" apply -k "$DEPLOY_DIR/distributed-control" >/dev/null
kubectl -n "$NS" rollout status statefulset/postgres-primary --timeout=180s
kubectl -n "$NS" rollout status statefulset/postgres-standby --timeout=180s
initialize_role_installation control
initialize_role_installation coordinator
kubectl -n "$NS" patch job/control-migrate --type=merge -p '{"spec":{"suspend":false}}' >/dev/null
kubectl -n "$NS" patch job/coordinator-migrate --type=merge -p '{"spec":{"suspend":false}}' >/dev/null
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
# Materialization-location cause/effect table: C1 a Managed File path is
# normalized below the one live-input root; C2 a Memory mount preserves the
# Managed absolute `/mnt` path; C3 a multipart filesystem Skill owns the opaque
# catalog id returned by bootstrap; C4 an instruction-only Skill has no
# filesystem effect. E1 C1+C2+C3 project the three exact markers; E2 C4 cannot
# prove materialization. K1 K8s creation awaits File and Memory projection;
# K2 the later awaited Skill wiring atomically commits `.skills` before inference,
# so the completed stage-4 Provider marker is downstream readiness for all three;
# K3 the runtime projector alone maps the returned Skill id to `.skills` and this
# probe must not recompute it. D1 selects C1+C2+C3 and requires each
# canonical path independently; D2 excludes C4. Missing content fails without a
# second polling authority or accepting a marker from another Resource tree.
SESSION_PODS=$(kubectl -n "$NS" get pods -l app=awaken-sandbox \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')
[ -n "$SESSION_PODS" ] || { err "no isolated Session Pod realized the run"; diagnostics; exit 1; }
while IFS='|' read -r kind path marker; do
  found=0
  while IFS= read -r pod; do
    if kubectl -n "$NS" exec "$pod" -c agent -- /bin/sh -ec \
      'test -f "$1" && grep -Fqx -- "$2" "$1"' \
      awaken-materialization-probe "$path" "$marker"; then
      found=1
      break
    fi
  done <<<"$SESSION_PODS"
  [ "$found" = "1" ] \
    || { err "no Session Pod contained the pinned ${kind} marker"; diagnostics; exit 1; }
done <<MATERIALIZATION_CASES
FILE|/mnt/session/uploads/inputs/adr71-input.txt|ADR71-FILE-MATERIALIZED
MEMORY|/mnt/memory/fact.md|ADR71-MEMORY-MATERIALIZED
SKILL|/workspace/.skills/${SKILL_ID}/SKILL.md|ADR71-SKILL-MATERIALIZED
MATERIALIZATION_CASES
ok "Workers have no DB socket/config/seal; the Session Pod owns every exact Resource projection"

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
# C1 one Control/Coordinator Pod removed -> the peer serves the same durable IDs;
# C2 peer Coordinator commits a Run -> the request owner reads the canonical
# recovery snapshot, including the cumulative usage cell, and projects exactly
# one assistant response (never idle-only or a process-local usage rewind);
# C3 both Provider replicas are stable + Worker dies after claims -> leases are
# reclaimed, old epochs are fenced, and every accepted marker reaches one and
# only one terminal Provider response;
# C4 Worker recovery is complete + one Provider Pod is removed -> the surviving
# Provider replica completes exactly one new turn. Ordering C3 before C4 is part
# of the decision table: Pod readiness cannot erase an already-open/stale HTTP
# connection in every surviving Worker pool, so reversing them silently creates
# a compound Provider-transport + Worker fault while asserting single-fault
# success. Compound dependency exhaustion has its own typed terminal semantics.
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=control -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-CONTROL-LOSS
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=coordinator -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-COORDINATOR-LOSS
wait_roles || { diagnostics; exit 1; }
node "$DRIVER" batch "$API_URL" "$SESSION_ID" "$ENVIRONMENT_ID" ADR71-CHAOS-SLOW 12 &
BATCH_PID=$!
sleep 1
# A Pod deletion first withdraws Kubernetes networking and may leave userspace
# briefly alive, which can turn an impending process crash into a genuine
# provider transport error committed by the still-current claim. That tests a
# compound network-partition semantic, not C3's hard Worker-process crash.
# Stop the exact CRI container instead: execution stops before it can author
# another fact, the Pod identity remains observable, and the durable
# lease/restart path must reclaim the unfinished Run. Container PID namespaces
# protect PID 1 from a sibling `kubectl exec` process, so the owning k3d node is
# the only reliable hard-crash injection boundary.
WORKER_ZERO_NODE=$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.spec.nodeName}')
WORKER_ZERO_CONTAINER=$(kubectl -n "$NS" get pod worker-0 \
  -o jsonpath='{.status.containerStatuses[0].containerID}')
WORKER_ZERO_CONTAINER=${WORKER_ZERO_CONTAINER#containerd://}
WORKER_ZERO_RESTARTS_BEFORE=$(kubectl -n "$NS" get pod worker-0 \
  -o jsonpath='{.status.containerStatuses[0].restartCount}')
[[ "$WORKER_ZERO_NODE" == "k3d-${CLUSTER}-"* ]] \
  && [[ "$WORKER_ZERO_CONTAINER" =~ ^[0-9a-f]{64}$ ]] \
  && [[ "$WORKER_ZERO_RESTARTS_BEFORE" =~ ^[0-9]+$ ]] \
  || { err "invalid worker-0 CRI crash target"; exit 1; }
docker exec "$WORKER_ZERO_NODE" crictl stop --timeout 0 "$WORKER_ZERO_CONTAINER" >/dev/null
for _ in $(seq 1 90); do
  WORKER_ZERO_RESTARTS_AFTER=$(kubectl -n "$NS" get pod worker-0 \
    -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || true)
  [[ "$WORKER_ZERO_RESTARTS_AFTER" =~ ^[0-9]+$ ]] \
    && [ "$WORKER_ZERO_RESTARTS_AFTER" -gt "$WORKER_ZERO_RESTARTS_BEFORE" ] && break
  sleep 1
done
[ "${WORKER_ZERO_RESTARTS_AFTER:-0}" -gt "$WORKER_ZERO_RESTARTS_BEFORE" ] \
  || { err "worker-0 container did not restart after hard process crash"; exit 1; }
wait "$BATCH_PID"
wait_roles || { diagnostics; exit 1; }
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=provider -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-PROVIDER-LOSS
wait_roles || { diagnostics; exit 1; }
ok "Pod loss and an in-flight Worker crash preserved exactly-once terminal responses"

log "8/10 stop one K3D agent node and verify public continuity"
# Cause/effect decision table:
# N1 hard node stop + Ready still True -> no business write during the detection window;
# N2 Ready False/Unknown -> failed role endpoints are evicted; N2b a StatefulSet
# Worker or Session-owned sandbox object can remain apparently Ready/Terminating
# while its kubelet is unreachable, so after the physical node fence proves those
# processes cannot execute, force-removing those stale API objects permits the same
# lease-fenced Worker ordinal and durable Session environment to restart on a
# healthy node;
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
# Session environments are deterministic, standalone Pods rather than a
# Deployment/StatefulSet. A stopped kubelet cannot acknowledge their ordinary
# deletion, and their stale Ready bit would otherwise make adoption repeatedly
# exec into a process that is physically gone. Delete only Pods observed on the
# already-fenced node; the durable Session binding and realization digest remain
# the authority used to rebuild the exact environment on the next claim.
while read -r failed_sandbox failed_node; do
  [ "$failed_node" != "$STOPPED_NODE" ] || kubectl -n "$NS" delete pod "$failed_sandbox" \
    --grace-period=0 --force >/dev/null 2>&1
done < <(kubectl -n "$NS" get pod -l app=awaken-sandbox \
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

log "9/10 hard-crash the synchronous PostgreSQL writer and promote its standby"
# Database failover cause/effect graph:
# D1 the API acknowledged the preceding durable marker; D2 PostgreSQL reports
# exactly one synchronous remote_apply standby; D3 the primary container dies
# without a graceful database shutdown; D4 the standby is promoted; D5 its
# inherited requirement for the now-absent synchronous peer is removed before
# the stable Service selects it. Effects: E1 every acknowledged fact is present
# after promotion (RPO=0); E2 application pools reconnect through the same
# database name; E3 no request is replayed merely because a transport response
# was ambiguous; E4 a synchronous checkpoint joins promotion before exposure;
# E5 new writes continue in explicit single-replica degraded mode. Rules:
# F1=D1+D2+D3+D4+D5=>E1-E5; F2=!D2 fails before the crash because asynchronous
# replication cannot support the pre-failure RPO; F3=D4+!D5 must remain
# unselected because every commit would wait forever for a dead standby.
SYNC_STANDBYS=$(kubectl -n "$NS" exec postgres-primary-0 -- psql -U postgres -d awaken -tAc \
  "SELECT count(*) FROM pg_stat_replication WHERE application_name = 'awaken_standby' AND sync_state = 'sync'" \
  | tr -d '[:space:]')
[ "$SYNC_STANDBYS" = "1" ] \
  || { err "expected one synchronous remote_apply standby, got ${SYNC_STANDBYS:-none}"; exit 1; }
PRIMARY_NODE=$(kubectl -n "$NS" get pod postgres-primary-0 -o jsonpath='{.spec.nodeName}')
PRIMARY_CONTAINER=$(kubectl -n "$NS" get pod postgres-primary-0 \
  -o jsonpath='{.status.containerStatuses[0].containerID}')
PRIMARY_CONTAINER=${PRIMARY_CONTAINER#containerd://}
[[ "$PRIMARY_NODE" == "k3d-${CLUSTER}-"* ]] \
  && [[ "$PRIMARY_CONTAINER" =~ ^[0-9a-f]{64}$ ]] \
  || { err "invalid PostgreSQL CRI crash target"; exit 1; }
docker exec "$PRIMARY_NODE" crictl stop --timeout 0 "$PRIMARY_CONTAINER" >/dev/null
kubectl -n "$NS" scale statefulset/postgres-primary --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod/postgres-primary-0 --timeout=120s
kubectl -n "$NS" exec postgres-standby-0 -- \
  gosu postgres pg_ctl promote -D /var/lib/postgresql/data/pgdata -w
# Promotion preserves the old primary's synchronous-standby policy. Publishing
# that state would accept connections whose commits can never finish. The
# failover controller therefore establishes one explicit degraded writer before
# changing the existing Service selector; applications retain one database URL.
kubectl -n "$NS" exec postgres-standby-0 -- psql -v ON_ERROR_STOP=1 \
  -U postgres -d awaken \
  -c "ALTER SYSTEM SET synchronous_standby_names = '';" \
  -c 'SELECT pg_reload_conf();' \
  -c 'CHECKPOINT' >/dev/null
PROMOTED_STATE=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d awaken -Atc \
  "SELECT (NOT pg_is_in_recovery())::text || ':' || current_setting('synchronous_standby_names')")
[ "$PROMOTED_STATE" = "true:" ] \
  || { err "promoted writer did not enter explicit single-replica mode: $PROMOTED_STATE"; exit 1; }
kubectl -n "$NS" label pod postgres-standby-0 database-role=primary --overwrite >/dev/null
PROMOTED_IP=$(kubectl -n "$NS" get pod postgres-standby-0 -o jsonpath='{.status.podIP}')
for _ in $(seq 1 30); do
  POSTGRES_ENDPOINTS=$(kubectl -n "$NS" get endpoints postgres \
    -o jsonpath='{range .subsets[*].addresses[*]}{.ip}{"\n"}{end}')
  [ "$POSTGRES_ENDPOINTS" = "$PROMOTED_IP" ] && break
  sleep 1
done
[ "${POSTGRES_ENDPOINTS:-}" = "$PROMOTED_IP" ] \
  || { err "stable postgres Service did not select only the promoted writer"; exit 1; }
wait_roles || { diagnostics; exit 1; }
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-DATABASE-FAILOVER

log "10/10 run concurrent pressure through the same endpoint and audit final truth"
node "$DRIVER" load "$API_URL" "$ENVIRONMENT_ID"
DATABASES=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d awaken -tAc \
  "SELECT count(*) FROM pg_database WHERE datname IN ('control','credentials','runtime','resources')" | tr -d '[:space:]')
[ "$DATABASES" = "4" ] || { err "expected four isolated owner databases, got $DATABASES"; exit 1; }
kubectl -n "$NS" get pods -o custom-columns=POD:.metadata.name,APP:.metadata.labels.app,NODE:.spec.nodeName --no-headers

ok "K3D/K3S FULL E2E PASS: one public API covered configuration, resources, credentials, publication, deployment, real Worker execution and response; tier, Worker, node, provider, and PostgreSQL faults recovered without duplicate terminal output; concurrent load met the public latency/error gates."
