#!/usr/bin/env bash
# Full black-box K3D/K3S validation for the split production roles. Tests know
# one API endpoint only. kubectl/docker are used solely to deploy, inject faults,
# and collect isolation evidence; no assertion calls a role Service directly.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/e2e/k3d/harness.sh"

CLUSTER="${AWAKEN_K3D_CLUSTER:-awaken-adr71}"
# Validate before tool admission and registration of destructive cleanup.
k3d_validate_name "$CLUSTER" || exit 2
IMAGE="awaken-roles:$CLUSTER"
SANDBOX_BUILD_IMAGE="${ADR71_SANDBOX_BUILD_IMAGE:-awaken-sandbox:local}"
NS="awaken-adr71"
API_PORT="${ADR71_API_PORT:-38801}"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
DRIVER="$REPO_ROOT/e2e/k3d/distributed_control_driver.ts"
API_PF_LOG="${TMPDIR:-/tmp}/${CLUSTER}_api_pf.log"
API_PF=""
PROVIDER_PF=""
CAPACITY_SAMPLER=""
STOPPED_NODE=""
STOPPED_CONTAINER=""
PARTITIONED_NODE=""
PARTITIONED_NODE_RECEIPT=""
PARTITION_DIRECTION=both
WORKER_TLS_DIR=""
ROLE_OVERLAY=""
CONTINUATION_VOLUME=""
CLUSTER_CREATED=0
KUBECONFIG_FILE=""
NATIVE_CRASH_OLD="native-crash-old"
NATIVE_CRASH_NEXT="native-crash-next"
NATIVE_CRASH_MARKER="ADR71-NATIVE-HARD-CRASH"
NATIVE_INFLIGHT_NEXT="native-inflight-next"
NATIVE_INFLIGHT_MARKER="ADR71-NATIVE-INFLIGHT-CRASH"
BATCH_CLEANUP_TIMEOUT_SECONDS="${ADR71_BATCH_CLEANUP_TIMEOUT_SECONDS:-120}"
export CARGO_CACHE_AUTOCLEAN=0

# Test budget decision table: C1 no diagnostic override -> retain the original
# 120-second physical cleanup gate; C2 a positive override -> change only the
# observation deadline, never the exact Pod/PVC deletion assertions. Invalid
# values fail before any cluster mutation. R1=C1->120s, R2=C2->that deadline.
k3d_validate_deadline "$BATCH_CLEANUP_TIMEOUT_SECONDS" || {
  echo "ADR71_BATCH_CLEANUP_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
}

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }

# Placed below log(), which is exactly where distributed_control_contract_test.sh
# stops evaluating this file's prefix: a guard above that line would inspect the
# contract test's own positional instead of this runner's arguments.
# --self-test is the only argument; anything else used to fall through to a real
# five-role k3d rollout, which is minutes of cluster build for a run the caller
# did not ask for. Refuse before the cluster name is even validated.
case "${1:-}" in
  ""|--self-test) ;;
  *) echo "usage: e2e/k3d/distributed_control_e2e.sh [--self-test]" >&2; exit 2 ;;
esac
if [ "$#" -gt 1 ]; then
  echo "usage: e2e/k3d/distributed_control_e2e.sh [--self-test]" >&2
  exit 2
fi
ok() { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

wait_roles() {
  local resource
  for resource in deploy/control deploy/coordinator deploy/provider deploy/awaken-api statefulset/worker; do
    # Callers use an OR-list, which disables Bash errexit inside this function.
    # Every role must succeed; a later Ready Worker cannot erase an API timeout.
    kubectl -n "$NS" rollout status "$resource" --timeout=240s || return "$?"
  done
}

batch_cleanup_wait_until() {
  local deadline="$1" remaining; shift
  remaining=$((deadline - SECONDS))
  if ((remaining <= 0)); then
    err "auxiliary Session Pod/PVC cleanup exceeded the shared ${BATCH_CLEANUP_TIMEOUT_SECONDS}s deadline"
    return 1
  fi
  kubectl -n "$NS" wait --for=delete "$@" --timeout="${remaining}s"
}

sample_capacity() {
  # Observations never change claim state. Exact current Run/owner/epoch tuples
  # count execution claims; idle Sessions and queued inputs are excluded. The
  # same DB query observes connection/wait/WAL costs without another metric owner.
  while true; do
    timeout 15 kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
      "SELECT json_build_object('observed_at',clock_timestamp(),
        'claims',(SELECT COALESCE(json_agg(json_build_object('run_id',run_id,'owner',lease_owner,'epoch',lease_epoch)), '[]'::json)
          FROM runtime_dispatch WHERE status='running'
          AND lease_until > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)),
        'connections',(SELECT count(*) FROM pg_stat_activity),
        'lock_waiters',(SELECT count(*) FROM pg_stat_activity WHERE wait_event_type='Lock'),
        'wal_bytes',(SELECT wal_bytes FROM pg_stat_wal))" || return "$?"
    sleep 5
  done
}

if [[ "${1:-}" = "--self-test" ]]; then
  # Cause/effect decision table: R0 all five roles Ready => success and five
  # observations; R1..R5 failure at each respective role => that exact status,
  # no later observation. Execute in the caller's real OR-list context, so this
  # regression catches the original false green even with `set -e` enabled.
  for failed_role in 0 1 2 3 4 5; do
    calls=0
    kubectl() {
      calls=$((calls + 1))
      [[ "$calls" != "$failed_role" ]] || return 37
    }
    result=0
    wait_roles || result=$?
    if [[ "$failed_role" = 0 ]]; then
      [[ "$result" = 0 && "$calls" = 5 ]]
    else
      [[ "$result" = 37 && "$calls" = "$failed_role" ]]
    fi
  done
  ok "role rollout failures propagate at every position"
  # Cleanup deadline decision table: C1 a public disposal starts with one
  # 100-second budget; C2 the Pod wait consumes 30 seconds; C3 the shared
  # deadline expires. Effects: E1 the Pod wait receives the full budget;
  # E2 the PVC wait receives only the remaining budget; E3 no third kubectl
  # call occurs after expiry. R1=C1->E1, R2=C1+C2->E2, R3=C1+C3->E3.
  start_seconds=$SECONDS
  cleanup_deadline=$((SECONDS + 100))
  calls=0
  observed_timeouts=()
  kubectl() {
    local argument
    calls=$((calls + 1))
    for argument in "$@"; do
      [[ "$argument" != --timeout=* ]] || observed_timeouts+=("${argument#--timeout=}")
    done
    [[ "$calls" != 1 ]] || SECONDS=$((SECONDS + 30))
  }
  batch_cleanup_wait_until "$cleanup_deadline" pod/test
  batch_cleanup_wait_until "$cleanup_deadline" pvc/test
  [[ "$calls" = 2 && "${observed_timeouts[0]}" = 100s && "${observed_timeouts[1]}" = 70s ]]
  SECONDS=$cleanup_deadline
  result=0
  batch_cleanup_wait_until "$cleanup_deadline" pod/expired >/dev/null 2>&1 || result=$?
  [[ "$result" = 1 && "$calls" = 2 ]]
  SECONDS=$start_seconds
  ok "Pod and PVC waits share one physical cleanup deadline"
  # The deployment contract test is hermetic (it stubs docker/k3d/kubectl and
  # evaluates this file's own initialization prefix), and the formal/obligations ledger
  # names it as the evidence for the realization-scope requirement. Evidence a
  # lane never runs is a claim nobody earned, so it runs here, beside the rollout
  # table, in the same self-test check-all.sh already selects.
  bash "$REPO_ROOT/e2e/k3d/distributed_control_contract_test.sh" || exit "$?"
  exit 0
fi

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
  if [[ -n "$CAPACITY_SAMPLER" ]]; then
    kill "$CAPACITY_SAMPLER" 2>/dev/null || true
    wait "$CAPACITY_SAMPLER" 2>/dev/null || true
    CAPACITY_SAMPLER=""
  fi
  local status="$1"
  [ -z "$WORKER_TLS_DIR" ] || rm -rf -- "$WORKER_TLS_DIR"
  [ -z "$ROLE_OVERLAY" ] || rm -rf -- "$ROLE_OVERLAY"
  if [ "$status" -ne 0 ] && [ "${ADR71_KEEP_FAILED_CLUSTER:-0}" = "1" ]; then
    err "retaining failed k3d cluster $CLUSTER and kubeconfig $KUBECONFIG_FILE for diagnostics"
    return
  fi
  [ -z "$PARTITIONED_NODE" ] || k3d_set_node_network_partition \
    "$CLUSTER" "$PARTITIONED_NODE" "$PARTITIONED_NODE_RECEIPT" open "$PARTITION_DIRECTION" >/dev/null 2>&1 || true
  [ -z "$STOPPED_CONTAINER" ] || docker start "$STOPPED_CONTAINER" >/dev/null 2>&1 || true
  [ -z "$API_PF" ] || kill "$API_PF" 2>/dev/null || true
  [ -z "$PROVIDER_PF" ] || kill "$PROVIDER_PF" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d_delete_cluster_and_remove \
    "$CLUSTER" \
    "$DEPLOY_DIR/awaken-control" \
    "$DEPLOY_DIR/awaken-coordinator" \
    "$DEPLOY_DIR/awaken-server" \
    "$DEPLOY_DIR/awaken-worker" \
    "$KUBECONFIG_FILE" || return "$?"
  # Allocated by this invocation only; retained together with a failed cluster.
  [ -z "$CONTINUATION_VOLUME" ] || docker volume rm "$CONTINUATION_VOLUME" >/dev/null
}

k3d_admit_or_exit "ADR-0071 distributed E2E"
# The shared default kubectl context can move when another worktree creates a
# cluster. Keep every unqualified fault-injection command on this run's exact
# cluster without adding a second Kubernetes observer or changing production.
KUBECONFIG_FILE=$(mktemp)
k3d_use_private_kubeconfig "$KUBECONFIG_FILE"
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
          sed -e '/^worker_max_concurrent =/d' \
              /etc/awaken-template/worker.toml > /tmp/worker.toml
          printf '\nworker_max_concurrent = 1\n' >> /tmp/worker.toml
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

hard_crash_native_worker() {
  local pod="$1" expected_uid="$2" facts uid node container phase observed_container observed_uid observed_exit
  # One API observation binds UID, node and CRI id before the destructive
  # injection. Completion joins the exact retained UID/container, never a bare
  # name/404. Both Native fault phases reuse this one physical injection owner.
  facts=$(kubectl -n "$NS" get pod "$pod" \
    -o jsonpath='{.metadata.uid}|{.spec.nodeName}|{.status.containerStatuses[0].containerID}')
  IFS='|' read -r uid node container <<<"$facts"
  container=${container#containerd://}
  [ "$uid" = "$expected_uid" ] && [[ "$node" == "k3d-${CLUSTER}-"* ]] \
    && [[ "$container" =~ ^[0-9a-f]{64}$ ]] \
    || { err "invalid exact Native Worker CRI crash target"; return 1; }
  k3d_hard_stop_cri_container "$CLUSTER" "$node" "$container" || return "$?"
  for _ in $(seq 1 90); do
    facts=$(kubectl -n "$NS" get pod "$pod" \
      -o jsonpath='{.metadata.uid}|{.status.phase}|{.status.containerStatuses[0].state.terminated.containerID}|{.status.containerStatuses[0].state.terminated.exitCode}')
    IFS='|' read -r observed_uid phase observed_container observed_exit <<<"$facts"
    [ "$observed_uid" = "$expected_uid" ] && [ "$phase" = "Failed" ] \
      && [ "$observed_container" = "containerd://${container}" ] && [ "$observed_exit" = 137 ] && return 0
    sleep 1
  done
  err "Kubernetes did not retain the exact hard-crashed Worker process"
  return 1
}

log "1/11 build production roles and the protocol-only Provider fixture"
# The canonical Cargo/Docker caches own incremental reuse. An existing tag
# alone cannot establish that any of these four binaries matches this tree.
# Build the Sandbox before staging the four disposable role binaries inside the
# same Docker context. Those role bytes are not Sandbox inputs; staging them
# first invalidates BuildKit's otherwise reusable environment layers.
"$REPO_ROOT/deploy/images/sandbox/build.sh" --ensure-current "$SANDBOX_BUILD_IMAGE" ""
SANDBOX_IMAGE=$(k3d_image_digest "$SANDBOX_BUILD_IMAGE")
echo "using immutable Sandbox image $SANDBOX_IMAGE"
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
k3d_ensure_images "$IMAGE" "$SANDBOX_IMAGE" postgres:16 nginx:1.27-alpine
CONTINUATION_VOLUME=$(docker volume create --label "awaken.e2e.cluster=$CLUSTER")
k3d_create_cluster "$CLUSTER" 3 "" "" --shared-volume "$CONTINUATION_VOLUME" "$IMAGE" "$SANDBOX_IMAGE" postgres:16 nginx:1.27-alpine
CLUSTER_CREATED=1
k3d_bind_private_kubeconfig "$CLUSTER"
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
k3d_import_storage_images "$CLUSTER" "$IMAGE" "$SANDBOX_IMAGE" postgres:16 nginx:1.27-alpine
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
# fails before Pod-token registration for a mismatched CA/SAN.
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
# K4 Worker template + Pod-bound token/UID + private-CA HTTPS edge +
# namespace-scoped K8s runtime -> real Worker starts without seal key/DB,
# creates an isolated Session Pod, and resolves exact Credential/File/Memory/
# Skill projections there; Worker egress cannot bypass TLS on port 3001;
# K5 one Ingress -> public paths route by owner and every private path stays absent.
# Reuse the canonical topology through Kustomize's image transform. Concurrent
# clusters need distinct role tags; the default tag must not import another
# worktree's build between artifact verification and cluster startup.
ROLE_OVERLAY=$(mktemp -d "$DEPLOY_DIR/.adr71-overlay-XXXXXX")
cat >"$ROLE_OVERLAY/kustomization.yaml" <<YAML
resources: [../distributed-control]
images:
  - name: awaken-roles
    newName: awaken-roles
    newTag: $CLUSTER
YAML
# The canonical Create-control Namespace is distinct from the runtime
# Namespace. Apply its focused overlay without a namespace override before
# the Worker/Coordinator roles, so kubectl cannot redirect or reject its CAS
# Role while the first Sandbox Create route is coming up.
kubectl apply -k "$DEPLOY_DIR/distributed-control/create-control" >/dev/null
k3d_render_sandbox_image "$ROLE_OVERLAY" "$SANDBOX_IMAGE" \
  | kubectl -n "$NS" apply -f - >/dev/null
# Brokered Create/RBAC decision table: C1 the exact Pod-bound Worker has only
# read/UID-fenced disposal; C2 the Coordinator alone creates each of six
# protected Kinds; C3 its separate control Namespace accepts only the exact
# Coordinator ledger writer. Effects: E1 every direct Worker Create is denied;
# E2 each Coordinator Create and exact ledger CAS is allowed. Rules RB1=C1+C2
# +C3->E1+E2; any absent cause fails before the first public WorkUnit.
WORKER_SUBJECT="system:serviceaccount:${NS}:worker-runtime"
COORDINATOR_SUBJECT="system:serviceaccount:${NS}:coordinator-runtime"
for resource in pods persistentvolumeclaims configmaps secrets jobs daemonsets; do
  [ "$(kubectl auth can-i create "$resource" --as="$WORKER_SUBJECT" -n "$NS")" = "no" ] \
    || { err "worker-runtime can bypass the ${resource} Create broker"; exit 1; }
  [ "$(kubectl auth can-i create "$resource" --as="$COORDINATOR_SUBJECT" -n "$NS")" = "yes" ] \
    || { err "Coordinator cannot broker ${resource} Create"; exit 1; }
done
for pvc_verb in get delete; do
  [ "$(kubectl auth can-i "$pvc_verb" persistentvolumeclaims \
    --as="$WORKER_SUBJECT" -n "$NS")" = "yes" ] \
    || { err "worker-runtime cannot ${pvc_verb} the exact continuation PVC"; exit 1; }
done
[ "$(kubectl auth can-i create configmaps --as="$COORDINATOR_SUBJECT" -n awaken-create-control)" = "yes" ] \
  || { err "Coordinator cannot CAS the independent Create ledger"; exit 1; }
[ "$(kubectl auth can-i create configmaps --as="$WORKER_SUBJECT" -n awaken-create-control)" = "no" ] \
  || { err "Worker can bypass the independent Create ledger"; exit 1; }
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
# C3 the drained Worker requests exact Pod deletion before exit; StatefulSet
# replacement gets a new UID, then a fresh incarnation can become Ready.
WORKER_CONTAINERS_BEFORE=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].containerID}{"\n"}{end}')
WORKER_0_UID_BEFORE=$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.metadata.uid}')
WORKER_1_UID_BEFORE=$(kubectl -n "$NS" get pod worker-1 -o jsonpath='{.metadata.uid}')
kubectl -n "$NS" scale deploy/coordinator --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod -l app=coordinator --timeout=120s
node "$DRIVER" unavailable-publication "$API_URL"
for _ in $(seq 1 90); do
  DRAINED=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].ready}{"\n"}{end}' | grep -c '^false$' || true)
  [ "$DRAINED" = "2" ] && break
  sleep 1
done
[ "${DRAINED:-0}" = "2" ] || { err "Workers did not fail closed after losing Coordinator authority"; exit 1; }
# C1 unready can precede hard lease expiry: transport failure only revokes
# admission temporarily. Restoring Coordinator at that point legitimately
# preserves the process. C2 the persisted Registry leases expire with every
# Coordinator absent => the existing lifecycle must drain that incarnation.
# Observe C2 from the same Registry authority before asserting replacement;
# readiness alone cannot manufacture an authority-loss fault.
for _ in $(seq 1 90); do
  LIVE_WORKER_LEASES=$(kubectl -n "$NS" exec postgres-primary-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
    "SELECT count(*) FROM worker_registry_worker WHERE expires_at_ms > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)")
  [[ "$LIVE_WORKER_LEASES" =~ ^[0-9]+$ ]] || { err "Worker lease observation is invalid"; exit 1; }
  [ "$LIVE_WORKER_LEASES" = "0" ] && break
  sleep 1
done
[ "$LIVE_WORKER_LEASES" = "0" ] || { err "Coordinator outage did not expire Worker authority"; exit 1; }
kubectl -n "$NS" scale deploy/coordinator --replicas=2 >/dev/null
kubectl -n "$NS" rollout status deploy/coordinator --timeout=240s
node "$DRIVER" retry-publication "$API_URL"
kubectl -n "$NS" rollout status statefulset/worker --timeout=240s
WORKER_CONTAINERS_AFTER=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].containerID}{"\n"}{end}')
[ "$WORKER_CONTAINERS_BEFORE" != "$WORKER_CONTAINERS_AFTER" ] || { err "drained Worker containers were reused"; exit 1; }
# Worker Pod replacement decision table: W1 authority lost + exact local
# drain + retained UID deletion intent => Kubernetes may replace the Pod;
# W2 same UID container restart => the immutable Create ledger cannot bind a
# second process and must remain unready. E1 both ordinals acquire new UIDs
# with zero container restarts; E2 either unchanged UID fails this gate. The
# provider still owns the old exit receipt independently of DELETE acceptance.
[ "$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.metadata.uid}')" != "$WORKER_0_UID_BEFORE" ] \
  || { err "worker-0 reused its immutable Pod UID"; exit 1; }
[ "$(kubectl -n "$NS" get pod worker-1 -o jsonpath='{.metadata.uid}')" != "$WORKER_1_UID_BEFORE" ] \
  || { err "worker-1 reused its immutable Pod UID"; exit 1; }
for pod in worker-0 worker-1; do
  [ "$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.status.containerStatuses[0].restartCount}')" = 0 ] \
    || { err "$pod restarted inside its immutable Pod UID"; exit 1; }
done
ok "Coordinator outage failed closed; exact Pod deletion created fresh Worker UIDs"

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
BATCH_CLEANUP_STARTED=$SECONDS
BATCH_CLEANUP_DEADLINE=$((SECONDS + BATCH_CLEANUP_TIMEOUT_SECONDS))
node "$DRIVER" dispose-batch "$API_URL" "$BATCH_AUXILIARY"
batch_cleanup_wait_until "$BATCH_CLEANUP_DEADLINE" "${BATCH_AUXILIARY_POD_TARGETS[@]}"
batch_cleanup_wait_until "$BATCH_CLEANUP_DEADLINE" "${BATCH_AUXILIARY_CLAIM_TARGETS[@]}"
ok "all auxiliary Pod/PVC objects disappeared in $((SECONDS - BATCH_CLEANUP_STARTED))s of one ${BATCH_CLEANUP_TIMEOUT_SECONDS}s budget"
wait_roles || { diagnostics; exit 1; }
kubectl -n "$NS" delete pod "$(kubectl -n "$NS" get pod -l app=provider -o jsonpath='{.items[0].metadata.name}')" --grace-period=0 --force >/dev/null 2>&1
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-PROVIDER-LOSS
wait_roles || { diagnostics; exit 1; }
ok "Pod loss and concurrent traffic preserved exactly-once terminal responses"

log "8/11 partition, then stop one K3D agent node and verify public continuity"
# Cause/effect decision table:
# N0 exact non-database node + observed underlay -> atomically drop that node
# interface's incoming/outgoing packets, retaining its container/IP/MAC/ifindex
# receipt before mutation; malformed, foreign and already-blocked targets fail;
# N1 network partition or hard node stop + Ready still True -> no business write
# during the detection window;
# N2 Ready False/Unknown evicts failed endpoints, but that observation is not
# an exit receipt for the three original Sandbox containers. N2b the retained
# Create-result guard and exact PVC UID must keep the old Session bound while
# Kubernetes reports synthetic Unknown exits. No force deletion is permitted.
# N3 a surviving CoreDNS/Worker path may execute an independently created
# Session after the node returns; it cannot reuse the old Session's attempt or
# PVC. N4 the later DB failover stage uses that independent identity. N5 the
# original Session can resume only after a provider supplies real terminal
# evidence, which this injected k3d node stop intentionally does not provide.
# N6 before the fault, pin the exact Pod/PVC identity so retention and any
# later replacement cannot be inferred from a shared name alone.
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

# Network-partition rule N7: unlike the hard stop below, this fault leaves every
# process and network interface running but blocks every underlay packet.
# While its executor is unobservable, the driver performs reads only. Removing
# the exact owned filter must recover the same Pod and PVC before the first new
# business Event, which is then submitted once through the ordinary oracle.
# N11 each one-way direction independently crosses detection, read-only truth,
# exact restoration and same-executor continuity. No profile may borrow another
# profile's success. The prepared direction remains available to cleanup.
for PARTITION_DIRECTION in ingress egress both; do
  PARTITIONED_NODE="$CHAOS_NODE"
  PARTITIONED_NODE_RECEIPT=$(k3d_node_network_identity "$CLUSTER" "$PARTITIONED_NODE")
  ok "prepared node network identity direction=$PARTITION_DIRECTION receipt=$PARTITIONED_NODE_RECEIPT"
  PARTITIONED_INTERFACE=$(cut -d '|' -f 2 <<<"$PARTITIONED_NODE_RECEIPT")
  PARTITIONED_ROUTES=$(docker exec "$PARTITIONED_NODE" ip -4 route show dev "$PARTITIONED_INTERFACE" | sort)
  k3d_set_node_network_partition "$CLUSTER" "$PARTITIONED_NODE" "$PARTITIONED_NODE_RECEIPT" blocked "$PARTITION_DIRECTION"
  for _ in $(seq 1 120); do
    NODE_READY=$(kubectl get node "$PARTITIONED_NODE" \
      -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')
    [ "$NODE_READY" != "True" ] && break
    sleep 1
  done
  [ "${NODE_READY:-True}" != "True" ] \
    || { err "partitioned node remained Ready=True beyond its detection deadline"; exit 1; }
  start_public_endpoint
  node "$DRIVER" read-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-PROVIDER-LOSS
  k3d_set_node_network_partition "$CLUSTER" "$PARTITIONED_NODE" "$PARTITIONED_NODE_RECEIPT" open "$PARTITION_DIRECTION"
  # N8 removing the fault must preserve the original underlay routes as well as
  # the receipt's container, interface, ifindex, IP and MAC. An interface reset
  # can preserve an address while silently losing its gateway.
  [ "$(docker exec "$PARTITIONED_NODE" ip -4 route show dev "$PARTITIONED_INTERFACE" | sort)" = "$PARTITIONED_ROUTES" ] \
    || { err "network fault changed the node's underlay routes"; exit 1; }
  PARTITIONED_NODE=""
  PARTITIONED_NODE_RECEIPT=""
  kubectl wait --for=condition=Ready "node/$CHAOS_NODE" --timeout=180s
  wait_roles || { diagnostics; exit 1; }
  [ "$(kubectl -n "$NS" get pod "$CONTINUITY_POD" -o jsonpath='{.metadata.uid}')" = "$POD_UID_BEFORE" ] \
    || { err "transient network partition replaced the retained Session Pod"; exit 1; }
  [ "$(kubectl -n "$NS" get pvc "$CLAIM_NAME" -o jsonpath='{.metadata.uid}')" = "$CLAIM_UID_BEFORE" ] \
    || { err "transient network partition changed continuation volume identity"; exit 1; }
  node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" "ADR71-AFTER-NETWORK-$PARTITION_DIRECTION"
  ok "node $PARTITION_DIRECTION network partition retained committed truth and recovered the same Session executor"
done

STOPPED_NODE="$CHAOS_NODE"
STOPPED_CONTAINER=$(docker inspect --format '{{.Id}}' -- "$STOPPED_NODE")
# N9 abrupt namespace loss must have exact SIGKILL/137 evidence. Docker stop
# permits a graceful K3S shutdown and cannot qualify this power-loss simulation.
k3d_hard_stop_node "$CLUSTER" "$STOPPED_NODE" "$STOPPED_CONTAINER"
ok "node SIGKILL receipt container=$STOPPED_CONTAINER running=false exit=137"
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
# Hard-node decision table: H1 Node Unknown + original Pod UID + retained
# Create-result guard + original PVC UID => no provider terminal receipt;
# H2 synthetic Unknown container exits after node return => H1 still holds;
# H3 an unrelated Session has a different identity and claim. Effects:
# E1 H1/H2 retain old evidence and never force-delete a Pod; E2 committed old
# history stays readable; E3 H3 progresses without reopening the old attempt.
# Neither a Node condition nor a deleted Pod name proves physical closure.
[ "$(kubectl -n "$NS" get pod "$CONTINUITY_POD" -o jsonpath='{.metadata.uid}')" = "$POD_UID_BEFORE" ] \
  || { err "node loss replaced the retained Session Pod without proof"; exit 1; }
[ "$(kubectl -n "$NS" get pvc "$CLAIM_NAME" -o jsonpath='{.metadata.uid}')" = "$CLAIM_UID_BEFORE" ] \
  || { err "node loss changed the retained continuation claim"; exit 1; }
kubectl -n "$NS" get pod "$CONTINUITY_POD" \
  -o jsonpath='{.metadata.finalizers[*]}' | grep -Fq 'awaken.dev/create-result-guard' \
  || { err "node loss released the Sandbox Create evidence guard"; exit 1; }
# The public client remains read-only while the original process is Unknown.
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
node "$DRIVER" read-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-NETWORK-both
[ "$(kubectl -n "$NS" get pod "$CONTINUITY_POD" -o jsonpath='{.metadata.uid}')" = "$POD_UID_BEFORE" ] \
  || { err "the original Sandbox Pod advanced without physical proof"; exit 1; }
kubectl -n "$NS" get pod "$CONTINUITY_POD" \
  -o jsonpath='{.metadata.finalizers[*]}' | grep -Fq 'awaken.dev/create-result-guard' \
  || { err "the original Sandbox guard was released without physical proof"; exit 1; }
ok "hard node loss kept the old Session and PVC fenced while history remained readable"
docker start "$STOPPED_CONTAINER" >/dev/null
STOPPED_CONTAINER=""
RESTORED_NODE="$STOPPED_NODE"
STOPPED_NODE=""
kubectl wait --for=condition=Ready "node/$RESTORED_NODE" --timeout=180s
[ "$(kubectl -n "$NS" get pvc "$CLAIM_NAME" -o jsonpath='{.metadata.uid}')" = "$CLAIM_UID_BEFORE" ] \
  || { err "restored node changed the old Session PVC identity"; exit 1; }
kubectl -n "$NS" get pod "$CONTINUITY_POD" \
  -o jsonpath='{.metadata.finalizers[*]}' | grep -Fq 'awaken.dev/create-result-guard' \
  || { err "synthetic node return released the old Create guard"; exit 1; }
wait_roles || { diagnostics; exit 1; }
INDEPENDENT_OUTPUT=$(node "$DRIVER" create-independent-session "$API_URL" "$ENVIRONMENT_ID" | tail -1)
[[ "$INDEPENDENT_OUTPUT" == 'OK independent='* ]] \
  || { err "independent Session admission failed: $INDEPENDENT_OUTPUT"; exit 1; }
SESSION_ID=${INDEPENDENT_OUTPUT#OK independent=}
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-INDEPENDENT-AFTER-NODE-LOSS
ok "independent Session progressed; original Unknown execution stayed fenced"

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
k3d_hard_stop_cri_container "$CLUSTER" "$PRIMARY_NODE" "$PRIMARY_CONTAINER"
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
if [[ -n "${ADR71_EVIDENCE_DIR:-}" ]]; then
  mkdir -p "$ADR71_EVIDENCE_DIR"
  export ADR71_LOAD_REPORT_FILE="$ADR71_EVIDENCE_DIR/load.json"
  kubectl -n "$NS" get deployment,statefulset -o json >"$ADR71_EVIDENCE_DIR/topology.json"
  python3 - "$REPO_ROOT" "$IMAGE_ID" "$SANDBOX_IMAGE" "$ADR71_EVIDENCE_DIR/provenance.json" <<'PY'
import json, pathlib, subprocess, sys
repo, image, sandbox, target = sys.argv[1:]
revision = subprocess.check_output(['git','-C',repo,'rev-parse','HEAD'],text=True).strip()
pathlib.Path(target).write_text(json.dumps(dict(source_revision=revision, role_image_id=image,
    sandbox_image=sandbox, environment='local-k3d', provider_mode='fixture'),sort_keys=True)+'\n')
PY
  sample_capacity >"$ADR71_EVIDENCE_DIR/database.jsonl" 2>"$ADR71_EVIDENCE_DIR/database-sampler.log" &
  CAPACITY_SAMPLER=$!
fi
node "$DRIVER" load "$API_URL" "$ENVIRONMENT_ID"
if [[ -n "$CAPACITY_SAMPLER" ]]; then
  # A failed observer is an evidence failure, not an empty successful sample.
  kill -0 "$CAPACITY_SAMPLER" || { wait "$CAPACITY_SAMPLER"; exit 1; }
  kill "$CAPACITY_SAMPLER"
  wait "$CAPACITY_SAMPLER" 2>/dev/null || true
  CAPACITY_SAMPLER=""
  [[ -s "$ADR71_EVIDENCE_DIR/database.jsonl" ]]
fi
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
TRANSPORT_TEST=worker_transport::kubernetes::tests::live::real_pod_identity_survives_reopen_and_complete_evidence_retires_exact_attempt
# T1 shared transport moved from CLI to Coordinator; a stale filter otherwise
# exits successfully with zero tests. Enumerate this exact feature/target first;
# only the one current test enters the existing real Kubernetes qualification.
TRANSPORT_TESTS=$(cargo test --locked -p awaken-coordinator --features container-k8s --lib \
  "$TRANSPORT_TEST" -- --list --exact)
grep -Fqx "$TRANSPORT_TEST: test" <<<"$TRANSPORT_TESTS" \
  || { err "exact Kubernetes transport qualification test was not selected"; exit 1; }
AWAKEN_K8S_E2E=1 AWAKEN_K3D_CLUSTER="$CLUSTER" AWAKEN_K8S_FIXTURE_IMAGE="$IMAGE" \
  cargo test --locked -p awaken-coordinator --features container-k8s --lib \
  "$TRANSPORT_TEST" -- --exact --nocapture
kubectl -n "$NS" scale statefulset/worker --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod/worker-0 pod/worker-1 --timeout=180s

COORDINATOR_CONFIG=$(kubectl -n "$NS" get secret adr71-role-config \
  -o jsonpath='{.data.coordinator\.toml}' | base64 -d)
grep -q '^\[worker_kubernetes\]$' <<<"$COORDINATOR_CONFIG" \
  || { err "the original Coordinator lacked its Pod-bound Create authority"; exit 1; }
! grep -q '^worker_trust_credentials_file = ' <<<"$COORDINATOR_CONFIG" \
  || { err "Coordinator transport changed authority during recovery"; exit 1; }
[ "$(kubectl auth can-i create tokenreviews.authentication.k8s.io --as="$COORDINATOR_SUBJECT")" = "yes" ] \
  || { err "coordinator-runtime cannot create TokenReviews"; exit 1; }
for pod_verb in get delete; do
  [ "$(kubectl auth can-i "$pod_verb" pods --as="$COORDINATOR_SUBJECT" -n "$NS")" = "yes" ] \
    || { err "coordinator-runtime cannot ${pod_verb} Pods"; exit 1; }
done
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
NATIVE_PROVIDER_BEFORE_CRASH=$(kubectl -n "$NS" logs -l app=provider -c provider --tail=-1 --since=30m \
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

hard_crash_native_worker "$NATIVE_CRASH_OLD" "$NATIVE_OLD_UID"

create_native_worker_pod "$NATIVE_CRASH_NEXT"
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_CRASH_NEXT" --timeout=240s
node "$DRIVER" verify-native-crash \
  "$API_URL" "$NATIVE_SESSION_ID" "$NATIVE_RECEIPT_ID" "$NATIVE_CRASH_MARKER"

NATIVE_REMAINING=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -Atc \
  "SELECT count(*) FROM runtime_dispatch WHERE run_id = '${NATIVE_RUN_ID}'" | tr -d '[:space:]')
[ "$NATIVE_REMAINING" = "0" ] \
  || { err "recovered Native dispatch was not settled"; exit 1; }
kubectl -n "$NS" wait --for=delete "pod/$NATIVE_CRASH_OLD" --timeout=120s
NATIVE_NEXT_DELETION=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_NEXT" \
  -o jsonpath='{.metadata.deletionTimestamp}')
[ -z "$NATIVE_NEXT_DELETION" ] \
  || { err "exact predecessor evidence release affected the successor"; exit 1; }
NATIVE_PROVIDER_REQUESTS=$(kubectl -n "$NS" logs -l app=provider -c provider --tail=-1 --since=30m \
  | grep -Fc "ADR71_PROVIDER_REQUEST \"${NATIVE_CRASH_MARKER}\"" || true)
[ "$NATIVE_PROVIDER_REQUESTS" = "1" ] \
  || { err "Native crash Run reached Provider ${NATIVE_PROVIDER_REQUESTS} times"; exit 1; }
ok "Pod-bound Native hard crash retired only the exact predecessor and settled without model replay"

log "11b/11 retain an opaque in-flight attempt after actual Worker death"
# U1 Provider is a separate Pod and its owned task has started; U2 the exact
# Worker dies before ModelOperationReturned; U3 its Run/Registry leases expire;
# U4 the Provider later completes while that HTTP caller is gone; U5 a fresh
# successor handles independent work. Effects: original journal remains Entered,
# original physical slot remains occupied, one accepted Provider invocation,
# no synthesized successful output, and independent receipt settles. Provider
# fixture query/release is an injection oracle only; no production exact-query
# capability is invented. This is the negative complement of K1-K3 above.
NATIVE_INFLIGHT_UID=$(kubectl -n "$NS" get pod "$NATIVE_CRASH_NEXT" -o jsonpath='{.metadata.uid}')
NATIVE_PREPARE=$(node "$DRIVER" prepare-native-crash \
  "$API_URL" "$ENVIRONMENT_ID" "$NATIVE_INFLIGHT_MARKER" | tail -1)
read -r NATIVE_PREPARED NATIVE_INFLIGHT_SESSION NATIVE_INFLIGHT_RECEIPT <<<"$NATIVE_PREPARE"
[ "$NATIVE_PREPARED" = "OK" ] && [ -n "$NATIVE_INFLIGHT_SESSION" ] && [ -n "$NATIVE_INFLIGHT_RECEIPT" ] \
  || { err "in-flight Native preparation did not return exact identities"; exit 1; }
# Provider has two independent replicas. The arrival's prefixed log identifies
# the ORIGINAL process; a Service forwarder could query the other replica and
# falsely report absence. No replicated mock store or second model route.
for _ in $(seq 1 60); do
  PROVIDER_ARRIVAL=$(kubectl -n "$NS" logs -l app=provider -c provider --prefix --tail=-1 --since=10m \
    | grep -F "ADR71_PROVIDER_REQUEST \"${NATIVE_INFLIGHT_MARKER}\"" || true)
  [ "$(grep -c . <<<"$PROVIDER_ARRIVAL" || true)" = "1" ] && break
  sleep 1
done
[ "$(grep -c . <<<"$PROVIDER_ARRIVAL" || true)" = "1" ] \
  || { err "in-flight Provider arrival was absent or duplicated"; exit 1; }
PROVIDER_ORIGINAL_POD=$(sed -E 's/^\[pod\/([^/]*)\/provider\].*/\1/' <<<"$PROVIDER_ARRIVAL")
[[ "$PROVIDER_ORIGINAL_POD" =~ ^provider-[a-z0-9-]+$ ]] \
  || { err "in-flight arrival has no exact Provider process"; exit 1; }
PROVIDER_ORIGINAL_UID=$(kubectl -n "$NS" get pod "$PROVIDER_ORIGINAL_POD" -o jsonpath='{.metadata.uid}')
PROVIDER_PORT=$(k3d_available_port "$((API_PORT + 1))")
PROVIDER_URL="http://127.0.0.1:${PROVIDER_PORT}"
PROVIDER_PF=$(k3d_start_port_forward "$NS" "pod/$PROVIDER_ORIGINAL_POD" "$PROVIDER_PORT" 3000 \
  "${TMPDIR:-/tmp}/${CLUSTER}_provider_pf.log")
node "$DRIVER" observe-provider-inflight "$PROVIDER_URL" "$NATIVE_INFLIGHT_MARKER"
NATIVE_INFLIGHT_DISPATCH=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -Atc \
  "SELECT run_id || '|' || active_attempt_owner || '|' || active_attempt_epoch::text || '|' || lease_until::text
     FROM runtime_dispatch WHERE request::text LIKE '%${NATIVE_INFLIGHT_SESSION}%'
      AND active_attempt_owner LIKE 'k8s:${NS}:${NATIVE_CRASH_NEXT}:%'")
[ "$(grep -c . <<<"$NATIVE_INFLIGHT_DISPATCH")" = "1" ] \
  || { err "expected one exact in-flight dispatch"; exit 1; }
IFS='|' read -r NATIVE_INFLIGHT_RUN NATIVE_INFLIGHT_OWNER NATIVE_INFLIGHT_EPOCH NATIVE_INFLIGHT_LEASE <<<"$NATIVE_INFLIGHT_DISPATCH"
[[ "$NATIVE_INFLIGHT_EPOCH" =~ ^[0-9]+$ ]] && [[ "$NATIVE_INFLIGHT_LEASE" =~ ^[0-9]+$ ]] \
  || { err "in-flight authority coordinates are malformed"; exit 1; }
hard_crash_native_worker "$NATIVE_CRASH_NEXT" "$NATIVE_INFLIGHT_UID"
node "$DRIVER" observe-provider-inflight "$PROVIDER_URL" "$NATIVE_INFLIGHT_MARKER"
for _ in $(seq 1 90); do
  # U3 observes the exact WorkerIdentity::lease_owner vocabulary: worker id,
  # generation and incarnation together. Comparing the composite dispatch
  # owner with worker_id alone can never prove expiry and falsely times out.
  NATIVE_INFLIGHT_EXPIRED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -Atc \
    "SELECT count(*) FROM worker_registry_worker
       WHERE worker_id || ':' || generation::text || ':' || incarnation_id = '${NATIVE_INFLIGHT_OWNER}'
       AND expires_at_ms < FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)::bigint
       AND EXISTS(SELECT 1 FROM runtime_dispatch WHERE run_id='${NATIVE_INFLIGHT_RUN}'
         AND active_attempt_owner='${NATIVE_INFLIGHT_OWNER}' AND active_attempt_epoch=${NATIVE_INFLIGHT_EPOCH}
         AND (lease_until IS NULL OR lease_until < FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)::bigint))")
  [ "$NATIVE_INFLIGHT_EXPIRED" = "1" ] && break
  sleep 1
done
[ "$NATIVE_INFLIGHT_EXPIRED" = "1" ] || { err "exact old Registry and Run leases did not expire"; exit 1; }
create_native_worker_pod "$NATIVE_INFLIGHT_NEXT"
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_INFLIGHT_NEXT" --timeout=240s
node "$DRIVER" release-provider-inflight "$PROVIDER_URL" "$NATIVE_INFLIGHT_MARKER"
node "$DRIVER" verify-native-inflight "$API_URL" "$NATIVE_INFLIGHT_SESSION" \
  "$NATIVE_INFLIGHT_RECEIPT" "$NATIVE_INFLIGHT_MARKER"
NATIVE_INFLIGHT_RETAINED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -Atc \
  "SELECT count(*) FROM runtime_dispatch WHERE run_id='${NATIVE_INFLIGHT_RUN}'
    AND active_attempt_owner='${NATIVE_INFLIGHT_OWNER}' AND active_attempt_epoch=${NATIVE_INFLIGHT_EPOCH}")
[ "$NATIVE_INFLIGHT_RETAINED" = "1" ] || { err "opaque physical slot was falsely retired"; exit 1; }
NATIVE_INFLIGHT_JOURNAL=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -Atc \
  "SELECT count(*) FILTER (WHERE data->'action'->'Set'->>'phase'='Entered')::text || '|' ||
          count(*) FILTER (WHERE data->'action'->'Set'->>'phase' IN ('Returned','TimedOut'))::text
    FROM runtime_state_command WHERE data->>'run_id'='${NATIVE_INFLIGHT_RUN}'
      AND data->>'key'='runtime.model_operation.v1'")
[ "$NATIVE_INFLIGHT_JOURNAL" = "1|0" ] || { err "old Worker journal gained a fabricated model return: $NATIVE_INFLIGHT_JOURNAL"; exit 1; }
node "$DRIVER" observe-provider-completed "$PROVIDER_URL" "$NATIVE_INFLIGHT_MARKER"
[ "$(kubectl -n "$NS" get pod "$PROVIDER_ORIGINAL_POD" -o jsonpath='{.metadata.uid}')" = "$PROVIDER_ORIGINAL_UID" ] \
  || { err "Provider double process changed during Worker crash qualification"; exit 1; }
NATIVE_INFLIGHT_PROVIDER_REQUESTS=$(kubectl -n "$NS" logs -l app=provider -c provider --tail=-1 --since=10m \
  | grep -Fc "ADR71_PROVIDER_REQUEST \"${NATIVE_INFLIGHT_MARKER}\"" || true)
[ "$NATIVE_INFLIGHT_PROVIDER_REQUESTS" = "1" ] || { err "in-flight invocation was replayed across Provider replicas"; exit 1; }
ok "independent Provider double completed; original unknown journal/slot retained, no replay, independent successor work passed"

ok "K3D/K3S FULL E2E PASS: one public API covered configuration, resources, credentials, publication, deployment, real Worker execution and response; completed Pod-bound Native crash recovered and opaque in-flight crash retained safely without duplicate execution; concurrent load met the public latency/error gates."
