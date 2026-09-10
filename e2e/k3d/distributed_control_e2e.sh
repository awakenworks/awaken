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
CONTINUATION_VOLUME=""
CLUSTER_CREATED=0
NATIVE_CRASH_OLD="native-crash-old"
NATIVE_CRASH_NEXT="native-crash-next"
NATIVE_CRASH_MARKER="ADR71-NATIVE-HARD-CRASH"
export CARGO_CACHE_AUTOCLEAN=0

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok() { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

coordinator_worker_log_count() {
  local pattern="$1"
  local pod count total=0
  while IFS= read -r pod; do
    count=$(kubectl -n "$NS" logs "$pod" -c worker-tls --since=10m \
      | grep -Ec "$pattern" || true)
    total=$((total + count))
  done < <(kubectl -n "$NS" get pods -l app=coordinator -o name)
  printf '%s\n' "$total"
}

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
    "$DEPLOY_DIR/awaken-worker" || return "$?"
  # Allocated by this invocation only; retained together with a failed cluster.
  [ -z "$CONTINUATION_VOLUME" ] || docker volume rm "$CONTINUATION_VOLUME" >/dev/null
}

k3d_admit_or_exit "ADR-0071 distributed E2E"
k3d_install_exit_cleanup cleanup

# The public driver intentionally uses the repository-pinned official SDK.
# Fail before building four binaries and a cluster when that committed E2E
# dependency tree has not been installed; package-lock.json remains its only
# version authority.
(cd "$REPO_ROOT/e2e" && node --input-type=module -e "import('@anthropic-ai/sdk')") \
  || { err "E2E dependencies are missing; run: npm ci --prefix e2e"; exit 1; }

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
  # Diagnostic decision table: C1 the exact cluster was created; C2 its API is
  # observable within the harness deadline. D1 C1+C2 emits only this context's
  # resources; D2 !C1 performs no kubectl call after a host build failure; D3
  # C1+!C2 stops at the deadline and preserves the original ERR status.
  [ "$CLUSTER_CREATED" = "1" ] || return 0
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" get pods -o wide || true
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" get pods -l app=awaken-sandbox \
    -o 'custom-columns=NAME:.metadata.name,UID:.metadata.uid,SCOPE:.metadata.annotations.awaken\.dev/sandbox-scope,CLAIM:.spec.volumes[*].persistentVolumeClaim.claimName' || true
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" get pvc \
    -o 'custom-columns=NAME:.metadata.name,UID:.metadata.uid,STATUS:.status.phase' || true
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" get events \
    --sort-by=.lastTimestamp | tail -60 || true
  for app in control control-migrate coordinator coordinator-migrate worker provider postgres-primary postgres-standby; do
    k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" logs -l "app=$app" \
      --all-containers --tail=100 --prefix || true
  done
  return 0
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

create_native_worker_pod() {
  local pod_name="$1"
  # One canonical Pod template covers predecessor and successor. The projected
  # coordinate is authenticated independently by TokenReview; restartPolicy
  # Never, the retained finalizer and a private non-privileged process namespace
  # are prerequisites for exact termination evidence, not test conveniences.
  kubectl -n "$NS" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: ${pod_name}
  labels: { app: worker, awaken.dev/native-crash-fixture: "true" }
  finalizers: [awaken.dev/worker-process-evidence]
spec:
  serviceAccountName: worker-runtime
  automountServiceAccountToken: true
  restartPolicy: Never
  terminationGracePeriodSeconds: 5
  securityContext:
    fsGroup: 10001
    fsGroupChangePolicy: OnRootMismatch
  containers:
    - name: worker
      image: ${IMAGE}
      imagePullPolicy: Never
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: { drop: ["ALL"] }
        runAsNonRoot: true
        runAsUser: 10001
        runAsGroup: 10001
        seccompProfile: { type: RuntimeDefault }
      command: ["/bin/sh", "-ec"]
      args:
        - |
          sed -e '/^worker_id =/d' \
              -e '/^worker_request_credential_file =/d' \
              -e '/^worker_max_concurrent =/d' \
              /etc/awaken-template/worker.toml > /tmp/worker.toml
          printf '\nworker_kubernetes_identity_directory = "/var/run/awaken/identity"\nworker_max_concurrent = 1\n' >> /tmp/worker.toml
          exec /usr/local/bin/awaken-worker --server https://coordinator:3443 --config /tmp/worker.toml
      ports: [{ containerPort: 9090 }]
      readinessProbe:
        httpGet: { path: /readyz, port: 9090 }
        initialDelaySeconds: 2
        periodSeconds: 2
      resources:
        requests: { cpu: 250m, memory: 256Mi }
        limits: { cpu: "2", memory: 2Gi }
      volumeMounts:
        - { name: template, mountPath: /etc/awaken-template, readOnly: true }
        - { name: upstream-tls, mountPath: /etc/awaken-worker-tls, readOnly: true }
        - { name: material, mountPath: /var/run/awaken/credentials, readOnly: true }
        - { name: identity, mountPath: /var/run/awaken/identity, readOnly: true }
        - { name: data, mountPath: /var/lib/awaken }
  volumes:
    - name: template
      secret:
        secretName: adr71-role-config
        items: [{ key: worker.toml, path: worker.toml }]
    - name: upstream-tls
      configMap:
        name: adr71-worker-upstream-ca
        items: [{ key: ca.crt, path: ca.crt }]
    - name: material
      secret:
        secretName: adr71-worker-material
        items:
          - key: provider-secret
            path: envelopes/61647237312d656e76656c6f7065/7368613235363a61647237312d70726f76696465722d7061796c6f6164/61647237312d70726f76696465722d63726564656e7469616c/1/776f726b73706163652d6164723731/666e76316136343a37616138653836326333363034333234/secret
          - key: payload-fingerprint
            path: envelopes/61647237312d656e76656c6f7065/7368613235363a61647237312d70726f76696465722d7061796c6f6164/61647237312d70726f76696465722d63726564656e7469616c/1/776f726b73706163652d6164723731/666e76316136343a37616138653836326333363034333234/payload_fingerprint
    - name: identity
      projected:
        sources:
          - serviceAccountToken:
              path: token
              audience: awaken-worker
              expirationSeconds: 600
          - downwardAPI:
              items:
                - { path: namespace, fieldRef: { fieldPath: metadata.namespace } }
                - { path: name, fieldRef: { fieldPath: metadata.name } }
                - { path: uid, fieldRef: { fieldPath: metadata.uid } }
    - name: data
      emptyDir: {}
YAML
}

log "1/11 build production roles and the protocol-only Provider fixture"
# The canonical Cargo/Docker caches own incremental reuse. An existing tag
# alone cannot establish that any of these four binaries matches this tree.
CONTROL_BIN=$(resolve_cargo_executable awaken-cli awaken-control)
COORDINATOR_BIN=$(resolve_cargo_executable awaken-cli awaken-coordinator --features container-k8s)
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

log "2/11 create three K3S worker nodes and import immutable test images"
"$REPO_ROOT/deploy/images/sandbox/build.sh" awaken-sandbox:local ""
k3d_docker_build --load -q -t "$IMAGE" \
  --build-arg AUXILIARY_EXECUTABLE=awaken-control \
  -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null
IMAGE_ID=$(docker image inspect "$IMAGE" --format '{{.Id}}')
echo "using immutable test image $IMAGE_ID"
# Permission decision table P1: host umask 077/022 can yield 0700/0755 inputs;
# root-owned COPY + non-root Worker must produce the same executable behavior.
# P2 every role artifact => mode 0755, unchanged bytes, executable by the deployed
# UID; production CLIs also execute --help. The scenario host has no help command
# and its actual startup is checked by the rollout below. The image owns normalization; do not chmod host inputs or
# relax the Pod identity to hide a packaging regression before cluster startup.
for executable in awaken-control awaken-coordinator awaken-worker awaken-server; do
  expected_hash=$(sha256sum "$DEPLOY_DIR/$executable" | cut -d ' ' -f 1)
  docker run --rm --network none --read-only --cap-drop ALL --user 10001:10001 \
    --entrypoint /bin/sh "$IMAGE_ID" -ec \
    'test "$(stat -c %a "/usr/local/bin/$1")" = 755
     test "$(sha256sum "/usr/local/bin/$1" | cut -d " " -f 1)" = "$2"
     test -x "/usr/local/bin/$1"
     if [ "$1" != awaken-server ]; then "/usr/local/bin/$1" --help >/dev/null; fi' \
    awaken-image-permission-probe "$executable" "$expected_hash"
done
ok "all role artifacts execute as UID 10001 with normalized mode and unchanged bytes"
# Reuse the shared nested-node reserve and actual capacity admission.
k3d_ensure_images "$IMAGE" awaken-sandbox:local postgres:16 nginx:1.27-alpine
CONTINUATION_VOLUME=$(docker volume create --label "awaken.e2e.cluster=$CLUSTER")
k3d_create_cluster "$CLUSTER" 3 "" "" --shared-volume "$CONTINUATION_VOLUME" "$IMAGE" awaken-sandbox:local postgres:16 nginx:1.27-alpine
CLUSTER_CREATED=1
# The installed local-path provisioner owns PVC/PV lifecycle. Its shared-filesystem
# mode removes node affinity because this exact Docker volume is mounted on every
# node. This is a node-crash fixture, not a proof of production storage durability.
kubectl -n kube-system patch configmap local-path-config --type=merge -p \
  '{"data":{"config.json":"{\"nodePathMap\":[],\"sharedFileSystemPath\":\"/var/lib/awaken-continuation\"}"}}' >/dev/null
# Storage cause/effect rule S5: shared filesystem + K3s default `local` PV
# type => API rejection (local requires node affinity); the same exact shared
# directory with `hostPath` => a relocatable PV. Select the provisioner's existing
# type contract explicitly rather than relying on its distribution-specific default.
kubectl annotate storageclass local-path defaultVolumeType=hostPath --overwrite >/dev/null
k3d_import_storage_images "$CLUSTER" "$IMAGE" awaken-sandbox:local postgres:16 nginx:1.27-alpine
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
# admits only its exact IP/port -> Worker Sandbox operations and Coordinator
# TokenReview/process-stop observations can operate while all other HTTPS egress
# remains denied. Their distinct ServiceAccounts remain the authorization fence.
# Invalid coordinates fail before apply.
KUBE_API_IP=$(kubectl get endpoints kubernetes -o jsonpath='{.subsets[0].addresses[0].ip}')
KUBE_API_PORT=$(kubectl get endpoints kubernetes -o jsonpath='{.subsets[0].ports[0].port}')
[[ "$KUBE_API_IP" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] \
  && [[ "$KUBE_API_PORT" =~ ^[0-9]+$ ]] \
  || { err "invalid Kubernetes API endpoint ${KUBE_API_IP}:${KUBE_API_PORT}"; exit 1; }
kubectl -n "$NS" apply -f - >/dev/null <<YAML
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: runtime-kube-api }
spec:
  podSelector:
    matchExpressions:
      - { key: app, operator: In, values: [worker, coordinator] }
  policyTypes: [Egress]
  egress:
    - to: [{ ipBlock: { cidr: "${KUBE_API_IP}/32" } }]
      ports: [{ protocol: TCP, port: ${KUBE_API_PORT} }]
YAML

log "3/11 deploy replicated authorities, real Workers, and PostgreSQL standby"
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
# Live RBAC decision table: C1 the Worker identity may create the retained claim;
# C2 it may get the exact UID during adoption/rebuild; C3 it may delete only at
# fenced disposal. E1 every required verb is admitted by the namespace Role.
# The static contract independently proves no wider PVC verbs are rendered.
for pvc_verb in create get delete; do
  [ "$(kubectl auth can-i "$pvc_verb" persistentvolumeclaims \
    --as="system:serviceaccount:${NS}:worker-runtime" -n "$NS")" = "yes" ] \
    || { err "worker-runtime cannot ${pvc_verb} continuation PVCs"; exit 1; }
done
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

# N0: place the target Session on a known non-database agent. Otherwise node
# chaos can miss its Sandbox entirely and pass without exercising reconstruction.
DB_NODES=$(kubectl -n "$NS" get pod -l 'database-role in (primary,standby)' -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}')
CHAOS_NODE=""
while IFS= read -r candidate; do
  if ! grep -qx "$candidate" <<<"$DB_NODES"; then CHAOS_NODE="$candidate"; break; fi
done < <(kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' \
  | grep "^k3d-${CLUSTER}-agent-" || true)
[ -n "$CHAOS_NODE" ] || { err "no non-database agent node available for node chaos"; exit 1; }
CORDONED_NODES=()
while IFS= read -r candidate; do
  if [ "$candidate" != "$CHAOS_NODE" ]; then
    kubectl cordon "$candidate" >/dev/null
    CORDONED_NODES+=("$candidate")
  fi
done < <(kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')

log "4/11 exercise the complete public configuration-to-response path"
BOOTSTRAP=$(node "$DRIVER" bootstrap "$API_URL" | tail -1)
[ "${BOOTSTRAP%% *}" = "OK" ] || { err "bootstrap failed: $BOOTSTRAP"; diagnostics; exit 1; }
read -r _ DEPLOYMENT_ID SESSION_ID ENVIRONMENT_ID FILE_ID MEMORY_ID SKILL_ID <<<"$BOOTSTRAP"
kubectl uncordon "${CORDONED_NODES[@]}" >/dev/null
ok "one endpoint completed Credential/File/Memory/Skill/Agent/Deployment/Session/Provider flow"

log "5/11 prove Worker process isolation and material realization"
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
SESSION_POD=$(k3d_sandbox_pod_for_session "$NS" "$SESSION_ID")
while IFS='|' read -r kind path marker; do
  kubectl -n "$NS" exec "$SESSION_POD" -c agent -- /bin/sh -ec \
    'test -f "$1" && grep -Fqx -- "$2" "$1"' \
    awaken-materialization-probe "$path" "$marker" \
    || { err "the exact Session Pod lacks the pinned ${kind} marker"; diagnostics; exit 1; }
done <<MATERIALIZATION_CASES
FILE|/mnt/session/uploads/inputs/adr71-input.txt|ADR71-FILE-MATERIALIZED
MEMORY|/mnt/memory/fact.md|ADR71-MEMORY-MATERIALIZED
SKILL|/workspace/.skills/${SKILL_ID}/SKILL.md|ADR71-SKILL-MATERIALIZED
MATERIALIZATION_CASES
ok "Workers have no DB socket/config/seal; the Session Pod owns every exact Resource projection"

log "6/11 remove the entire Coordinator tier, then recover the same publication"
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

log "7/11 inject single-Pod faults and concurrent durable traffic"
# Cause/effect decision table:
# C1 one Control/Coordinator Pod removed -> the peer serves the same durable IDs;
# C2 peer Coordinator commits a Run -> the request owner reads the canonical
# recovery snapshot, including the cumulative usage cell, and projects exactly
# one assistant response (never idle-only or a process-local usage rewind);
# C3 both Provider replicas and the fresh Worker incarnations from step 6 are
# stable -> concurrent independent Sessions complete exactly once across both
# replicas;
# C4 the concurrent batch is complete + one Provider Pod is removed -> the surviving
# Provider replica completes exactly one new turn. Ordering C3 before C4 is part
# of the decision table: Pod readiness cannot erase an already-open/stale HTTP
# connection in every surviving Worker pool, so reversing them silently creates
# a compound Provider-transport + Worker fault while asserting single-fault
# success. An in-flight hard Worker crash is intentionally not asserted here:
# ADR-0024 requires an authoritative provider terminal receipt before replaying
# an opaque remote request, and the physical-attempt suites own that fail-closed
# rule. Step 6 already proves real Worker container replacement and generation
# fencing without inventing remote-effect quiescence.
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=control -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-CONTROL-LOSS
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=coordinator -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-COORDINATOR-LOSS
wait_roles || { diagnostics; exit 1; }
BATCH_COUNT=12
BATCH_OUTPUT=$(node "$DRIVER" batch "$API_URL" "$SESSION_ID" "$ENVIRONMENT_ID" ADR71-CHAOS "$BATCH_COUNT")
printf '%s\n' "$BATCH_OUTPUT"
BATCH_LINE=${BATCH_OUTPUT##*$'\n'}
[[ "$BATCH_LINE" == "OK batch ADR71-CHAOS $BATCH_COUNT auxiliary="* ]] \
  || { err "batch did not publish its temporary Session cleanup identities"; exit 1; }
BATCH_AUXILIARY=${BATCH_LINE#* auxiliary=}
IFS=',' read -r -a BATCH_AUXILIARY_IDS <<<"$BATCH_AUXILIARY"
[ "${#BATCH_AUXILIARY_IDS[@]}" = "$((BATCH_COUNT - 1))" ] \
  || { err "batch published ${#BATCH_AUXILIARY_IDS[@]} temporary Sessions, expected $((BATCH_COUNT - 1))"; exit 1; }
# Fixture-capacity decision table: C1 Stage 7 has proved all twelve independent
# Session effects; C2 stages 8/9 depend only on the original durable Session;
# C3 public DELETE commits each auxiliary terminal intent; C4 its exact Pod and
# retained PVC are both gone. E1 C1+C2 permits disposal of only the eleven
# auxiliary Sessions; E2 C3+C4 releases their physical capacity before Stage 10;
# E3 any missing identity, rejected delete, or residual object fails the fixture
# rather than weakening the later 12-way cold-start bound. Snapshot the exact
# backend identities before DELETE because that command correctly returns after
# durable intent while the one lifecycle owner completes physical cleanup.
BATCH_AUXILIARY_POD_TARGETS=()
BATCH_AUXILIARY_CLAIM_TARGETS=()
for auxiliary_session in "${BATCH_AUXILIARY_IDS[@]}"; do
  auxiliary_pod=$(k3d_sandbox_pod_for_session "$NS" "$auxiliary_session")
  auxiliary_claim=$(kubectl -n "$NS" get pod "$auxiliary_pod" \
    -o jsonpath='{.spec.volumes[*].persistentVolumeClaim.claimName}')
  [ -n "$auxiliary_claim" ] \
    || { err "temporary Session $auxiliary_session has no retained claim"; exit 1; }
  BATCH_AUXILIARY_POD_TARGETS+=("pod/$auxiliary_pod")
  BATCH_AUXILIARY_CLAIM_TARGETS+=("pvc/$auxiliary_claim")
done
node "$DRIVER" dispose-batch "$API_URL" "$BATCH_AUXILIARY"
kubectl -n "$NS" wait --for=delete "${BATCH_AUXILIARY_POD_TARGETS[@]}" --timeout=120s
kubectl -n "$NS" wait --for=delete "${BATCH_AUXILIARY_CLAIM_TARGETS[@]}" --timeout=120s
wait_roles || { diagnostics; exit 1; }
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=provider -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-PROVIDER-LOSS
wait_roles || { diagnostics; exit 1; }
ok "Pod loss and concurrent traffic preserved exactly-once terminal responses"

log "8/11 stop one K3D agent node and verify public continuity"
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
# N6 decision rule: known target Pod + bound retained claim + matching live UID
# + mutable bytes written after materialization => reconstruction must change Pod
# UID and node, preserve PVC UID, and read the exact bytes. Any missing prerequisite
# fails before fault injection. A resource re-projection cannot recreate this file.
# Earlier role replacement may change the physical object. Re-observe the
# Session-owned Pod now; the bootstrap Pod name is not a durable identity.
CONTINUITY_POD=$(k3d_sandbox_pod_for_session "$NS" "$SESSION_ID")
POD_UID_BEFORE=$(kubectl -n "$NS" get pod "$CONTINUITY_POD" -o jsonpath='{.metadata.uid}')
POD_NODE_BEFORE=$(kubectl -n "$NS" get pod "$CONTINUITY_POD" -o jsonpath='{.spec.nodeName}')
[ "$POD_NODE_BEFORE" = "$CHAOS_NODE" ] || { err "target Session left the selected chaos node"; exit 1; }
CLAIM_NAME=$(kubectl -n "$NS" get pod "$CONTINUITY_POD" -o jsonpath='{.spec.volumes[*].persistentVolumeClaim.claimName}')
[ -n "$CLAIM_NAME" ] || { err "target Session has no continuation claim"; exit 1; }
CLAIM_UID_BEFORE=$(kubectl -n "$NS" get pvc "$CLAIM_NAME" -o jsonpath='{.metadata.uid}')
[ -n "$CLAIM_UID_BEFORE" ] && \
  [ "$(kubectl -n "$NS" get pod "$CONTINUITY_POD" -o jsonpath='{.metadata.annotations.awaken\.dev/continuation-claim-uid}')" = "$CLAIM_UID_BEFORE" ] \
  || { err "target Pod does not attest the exact live continuation UID"; exit 1; }
[ "$(kubectl -n "$NS" get pvc "$CLAIM_NAME" -o jsonpath='{.status.phase}')" = "Bound" ] \
  || { err "continuation claim is not Bound"; exit 1; }
CONTINUITY_PV=$(kubectl -n "$NS" get pvc "$CLAIM_NAME" -o jsonpath='{.spec.volumeName}')
[ -z "$(kubectl get pv "$CONTINUITY_PV" -o jsonpath='{.spec.nodeAffinity}')" ] \
  || { err "continuation PV cannot relocate across nodes"; exit 1; }
CONTINUITY_MARKER="ADR71-MUTABLE-$POD_UID_BEFORE"
kubectl -n "$NS" exec "$CONTINUITY_POD" -c agent -- /bin/sh -ec \
  'printf %s "$1" > /workspace/adr71-continuity; sync' awaken-continuity "$CONTINUITY_MARKER"
STOPPED_NODE="$CHAOS_NODE"
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
# N7: public success alone cannot establish filesystem continuity. Verify all
# three independent effects before the stopped node (and its old bytes) returns.
CONTINUITY_POD_AFTER=$(k3d_sandbox_pod_for_session "$NS" "$SESSION_ID")
POD_UID_AFTER=$(kubectl -n "$NS" get pod "$CONTINUITY_POD_AFTER" -o jsonpath='{.metadata.uid}')
[ -n "$POD_UID_AFTER" ] || { diagnostics; exit 1; }
[ "$POD_UID_AFTER" != "$POD_UID_BEFORE" ] \
  || { err "node loss did not reconstruct the target Pod"; exit 1; }
[ "$(kubectl -n "$NS" get pod "$CONTINUITY_POD_AFTER" -o jsonpath='{.spec.nodeName}')" != "$STOPPED_NODE" ] \
  || { err "replacement Pod remained on the stopped node"; exit 1; }
[ "$(kubectl -n "$NS" get pvc "$CLAIM_NAME" -o jsonpath='{.metadata.uid}')" = "$CLAIM_UID_BEFORE" ] && \
  [ "$(kubectl -n "$NS" get pod "$CONTINUITY_POD_AFTER" -o jsonpath='{.metadata.annotations.awaken\.dev/continuation-claim-uid}')" = "$CLAIM_UID_BEFORE" ] \
  || { err "reconstruction changed continuation volume identity"; diagnostics; exit 1; }
kubectl -n "$NS" exec "$CONTINUITY_POD_AFTER" -c agent -- /bin/sh -ec \
  'test "$(wc -c < /workspace/adr71-continuity)" -eq "${#1}" && test "$(cat /workspace/adr71-continuity)" = "$1"' awaken-continuity "$CONTINUITY_MARKER"
ok "node loss changed Pod UID/node, retained PVC UID and exact mutable workspace bytes"
docker start "$STOPPED_NODE" >/dev/null
RESTORED_NODE="$STOPPED_NODE"
STOPPED_NODE=""
kubectl wait --for=condition=Ready "node/$RESTORED_NODE" --timeout=180s

log "9/11 hard-crash the synchronous PostgreSQL writer and promote its standby"
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
# Public-transport cause/effect table: P1 database failover preserves every API
# Pod -> the existing port-forward remains usable; P2 one API Pod reconnects by
# restart -> a port-forward pinned to that Pod exits even though the Service has
# another Ready endpoint. In both cases E1 joins the existing public-endpoint
# owner before new writes; P2 must rebind, while P1 is an idempotent refresh.
# This repairs only the black-box test transport and never retries an accepted
# business request.
start_public_endpoint
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-DATABASE-FAILOVER

log "10/11 run concurrent pressure through the same endpoint and audit final truth"
node "$DRIVER" load "$API_URL" "$ENVIRONMENT_ID"
DATABASES=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d awaken -tAc \
  "SELECT count(*) FROM pg_database WHERE datname IN ('control','credentials','runtime','resources')" | tr -d '[:space:]')
[ "$DATABASES" = "4" ] || { err "expected four isolated owner databases, got $DATABASES"; exit 1; }
kubectl -n "$NS" get pods -o custom-columns=POD:.metadata.name,APP:.metadata.labels.app,NODE:.spec.nodeName --no-headers

log "11/11 hard-crash a Pod-bound Native Worker and recover without model replay"
# Native takeover cause/effect decision table:
# C1 only one non-restarting Pod-bound Worker is Ready; C2 a tool/Skill/Resource-
# free Native Run commits exactly one terminal Provider response; C3 the TLS
# fixture rejects only that predecessor's exact finish ACK; C4 its CRI container
# is hard-killed and the retained Pod reports the same terminated container;
# C5 the old Worker and Run leases expire; C6 a differently named/UID successor
# retries the existing claim. Effects: E1 the predecessor slot remains occupied
# until C4+C5; E2 the independent Coordinator closes and observes the exact Pod,
# reads the original ThreadCommit, then invokes only Dispatch's exact finish;
# E3 the successor completes the same receipt with no second Provider request;
# E4 the successor Pod remains live and the old deletion evidence stays retained.
# Rules K1 C1+C2+C3=>E1; K2 C4+C5+C6=>E2; K3 E2+C6=>E3+E4. Missing evidence,
# stale UID, opaque execution history or response timeout must retain the slot.
AWAKEN_K8S_E2E=1 AWAKEN_K3D_CLUSTER="$CLUSTER" AWAKEN_K8S_FIXTURE_IMAGE="$IMAGE" \
  cargo test -p awaken-cli --features container-k8s --lib \
  real_pod_identity_survives_reopen_and_complete_evidence_retires_exact_attempt
kubectl -n "$NS" scale statefulset/worker --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod/worker-0 pod/worker-1 --timeout=180s

COORDINATOR_CONFIG=$(kubectl -n "$NS" get secret adr71-role-config \
  -o jsonpath='{.data.coordinator\.toml}' | base64 -d)
grep -q '^worker_trust_credentials_file = ' <<<"$COORDINATOR_CONFIG" \
  || { err "initial Coordinator transport is not the expected signed authority"; exit 1; }
COORDINATOR_CONFIG=$(sed '/^worker_trust_credentials_file = /d' <<<"$COORDINATOR_CONFIG")
COORDINATOR_CONFIG+=$'\n[worker_kubernetes]\nnamespace = "awaken-adr71"\nservice_account = "worker-runtime"\naudience = "awaken-worker"\nprovider_id = "k3d-adr71"\n'
COORDINATOR_CONFIG_B64=$(printf '%s' "$COORDINATOR_CONFIG" | base64 | tr -d '\n')
kubectl -n "$NS" patch secret adr71-role-config --type=merge \
  -p "{\"data\":{\"coordinator.toml\":\"${COORDINATOR_CONFIG_B64}\"}}" >/dev/null
COORDINATOR_SUBJECT="system:serviceaccount:${NS}:coordinator-runtime"
[ "$(kubectl auth can-i create tokenreviews.authentication.k8s.io --as="$COORDINATOR_SUBJECT")" = "yes" ] \
  || { err "coordinator-runtime cannot create TokenReviews"; exit 1; }
for pod_verb in get delete; do
  [ "$(kubectl auth can-i "$pod_verb" pods --as="$COORDINATOR_SUBJECT" -n "$NS")" = "yes" ] \
    || { err "coordinator-runtime cannot ${pod_verb} Pods"; exit 1; }
done
kubectl -n "$NS" rollout restart deployment/coordinator >/dev/null
kubectl -n "$NS" rollout status deployment/coordinator --timeout=240s
start_public_endpoint

create_native_worker_pod "$NATIVE_CRASH_OLD"
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_CRASH_OLD" --timeout=240s
NATIVE_OLD_UID=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_OLD" -o jsonpath='{.metadata.uid}')
[[ "$NATIVE_OLD_UID" =~ ^[0-9a-f-]+$ ]] \
  || { err "old Native Worker has no exact Pod UID"; exit 1; }

NATIVE_PREPARE=$(node "$DRIVER" prepare-native-crash \
  "$API_URL" "$ENVIRONMENT_ID" "$NATIVE_CRASH_MARKER" | tail -1)
[ "${NATIVE_PREPARE%% *}" = "OK" ] \
  || { err "Native crash preparation failed: $NATIVE_PREPARE"; exit 1; }
read -r _ NATIVE_SESSION_ID NATIVE_RECEIPT_ID <<<"$NATIVE_PREPARE"
[ -n "$NATIVE_SESSION_ID" ] && [ -n "$NATIVE_RECEIPT_ID" ] \
  || { err "Native crash preparation returned incomplete identities"; exit 1; }

FINISH_REJECTED=0
for _ in $(seq 1 60); do
  # `kubectl logs -l` may select only one replica. Join every named Coordinator
  # Pod so a request handled by either TLS sidecar is observable; absence from
  # one replica is not evidence that the distributed boundary was not crossed.
  FINISH_REJECTED=$(coordinator_worker_log_count \
    'POST /v1/worker/dispatch/attempt/finish.* 503 ')
  [ "$FINISH_REJECTED" -gt 0 ] && break
  sleep 1
done
[ "$FINISH_REJECTED" -gt 0 ] \
  || { err "old Native Worker never reached the held finish ACK boundary"; exit 1; }
NATIVE_PROVIDER_BEFORE_CRASH=$(kubectl -n "$NS" logs -l app=provider -c provider --since=30m \
  | grep -Fc "ADR71_PROVIDER_REQUEST \"${NATIVE_CRASH_MARKER}\"" || true)
[ "$NATIVE_PROVIDER_BEFORE_CRASH" = "1" ] \
  || { err "Native crash Run reached Provider ${NATIVE_PROVIDER_BEFORE_CRASH} times before crash"; exit 1; }

NATIVE_DISPATCH=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -Atc \
  "SELECT run_id || '|' || active_attempt_owner || '|' || active_attempt_epoch::text
     FROM runtime_dispatch
    WHERE request::text LIKE '%${NATIVE_SESSION_ID}%'
      AND active_attempt_owner LIKE 'k8s:${NS}:${NATIVE_CRASH_OLD}:%'")
[ "$(grep -c . <<<"$NATIVE_DISPATCH")" = "1" ] \
  || { err "expected one exact occupied Native dispatch, got: $NATIVE_DISPATCH"; exit 1; }
IFS='|' read -r NATIVE_RUN_ID NATIVE_OLD_OWNER NATIVE_OLD_EPOCH <<<"$NATIVE_DISPATCH"
[ -n "$NATIVE_RUN_ID" ] && [ -n "$NATIVE_OLD_OWNER" ] && [[ "$NATIVE_OLD_EPOCH" =~ ^[0-9]+$ ]] \
  || { err "occupied Native dispatch coordinates are malformed"; exit 1; }

NATIVE_OLD_NODE=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_OLD" -o jsonpath='{.spec.nodeName}')
NATIVE_OLD_CONTAINER=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_OLD" \
  -o jsonpath='{.status.containerStatuses[0].containerID}')
NATIVE_OLD_CONTAINER=${NATIVE_OLD_CONTAINER#containerd://}
[[ "$NATIVE_OLD_NODE" == "k3d-${CLUSTER}-"* ]] \
  && [[ "$NATIVE_OLD_CONTAINER" =~ ^[0-9a-f]{64}$ ]] \
  || { err "invalid Native Worker CRI crash target"; exit 1; }
docker exec "$NATIVE_OLD_NODE" crictl stop --timeout 0 "$NATIVE_OLD_CONTAINER" >/dev/null
for _ in $(seq 1 90); do
  NATIVE_OLD_PHASE=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_OLD" -o jsonpath='{.status.phase}')
  NATIVE_TERMINATED_CONTAINER=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_OLD" \
    -o jsonpath='{.status.containerStatuses[0].state.terminated.containerID}')
  [ "$NATIVE_OLD_PHASE" = "Failed" ] \
    && [ "$NATIVE_TERMINATED_CONTAINER" = "containerd://${NATIVE_OLD_CONTAINER}" ] \
    && break
  sleep 1
done
[ "$NATIVE_OLD_PHASE" = "Failed" ] \
  && [ "$NATIVE_TERMINATED_CONTAINER" = "containerd://${NATIVE_OLD_CONTAINER}" ] \
  || { err "Kubernetes did not retain the exact hard-crashed Worker process"; exit 1; }

create_native_worker_pod "$NATIVE_CRASH_NEXT"
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_CRASH_NEXT" --timeout=240s
node "$DRIVER" verify-native-crash \
  "$API_URL" "$NATIVE_SESSION_ID" "$NATIVE_RECEIPT_ID" "$NATIVE_CRASH_MARKER"

NATIVE_REMAINING=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -Atc \
  "SELECT count(*) FROM runtime_dispatch WHERE run_id = '${NATIVE_RUN_ID}'" | tr -d '[:space:]')
[ "$NATIVE_REMAINING" = "0" ] \
  || { err "recovered Native dispatch was not settled"; exit 1; }
NATIVE_OLD_DELETION=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_OLD" \
  -o jsonpath='{.metadata.deletionTimestamp}')
NATIVE_OLD_FINALIZERS=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_OLD" \
  -o jsonpath='{.metadata.finalizers[*]}')
NATIVE_NEXT_DELETION=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_NEXT" \
  -o jsonpath='{.metadata.deletionTimestamp}')
[ -n "$NATIVE_OLD_DELETION" ] \
  && grep -qw 'awaken.dev/worker-process-evidence' <<<"$NATIVE_OLD_FINALIZERS" \
  && [ -z "$NATIVE_NEXT_DELETION" ] \
  || { err "exact predecessor deletion evidence or successor isolation was lost"; exit 1; }
NATIVE_PROVIDER_REQUESTS=$(kubectl -n "$NS" logs -l app=provider -c provider --since=30m \
  | grep -Fc "ADR71_PROVIDER_REQUEST \"${NATIVE_CRASH_MARKER}\"" || true)
[ "$NATIVE_PROVIDER_REQUESTS" = "1" ] \
  || { err "Native crash Run reached Provider ${NATIVE_PROVIDER_REQUESTS} times"; exit 1; }
ok "Pod-bound Native hard crash retired only the exact predecessor and settled without model replay"

ok "K3D/K3S FULL E2E PASS: one public API covered configuration, resources, credentials, publication, deployment, real Worker execution and response; tier, Worker, node, provider, PostgreSQL and Pod-bound Native hard-crash faults recovered without duplicate terminal output; concurrent load met the public latency/error gates."
