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
COORDINATOR_RESTORE_UID=""
COORDINATOR_RESTORE_REPLICAS=""
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
NATIVE_INPLACE="native-inplace"
NATIVE_INPLACE_MARKER="ADR71-NATIVE-INPLACE-CRASH"
NATIVE_CHECKPOINT="native-checkpoint"
NATIVE_CHECKPOINT_MARKER="ADR71-NATIVE-CHECKPOINT-REPOSITORY"
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

# One observation owner for both original role-log callers. The Provider's
# lifetime totals freeze EVERY original participant, not the surviving replica.
# GET/log failures are not absence, and a rotated/truncated prefix is Unknown.
role_log_participants() {
  local role="$1" container="$2"
  run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
    kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" \
    get deployment,pod -l "app=$role" -o json | python3 -c '
import json, sys
container = sys.argv[1]
items = json.load(sys.stdin)["items"]
deployments = [item for item in items if item["kind"] == "Deployment"]
pods = [item for item in items if item["kind"] == "Pod"]
if len(deployments) != 1:
    raise SystemExit(2)
deployment = deployments[0]
metadata, status = deployment["metadata"], deployment.get("status", {})
replicas = deployment["spec"]["replicas"]
if (not isinstance(replicas, int) or replicas < 1 or metadata.get("deletionTimestamp")
        or status.get("observedGeneration", 0) < metadata["generation"]
        or any(status.get(key, 0) != replicas for key in
               ("replicas", "updatedReplicas", "readyReplicas", "availableReplicas"))
        or len(pods) != replicas):
    raise SystemExit(2)
rows = []
for pod in pods:
    meta = pod["metadata"]
    selected = [value for value in pod.get("status", {}).get("containerStatuses", [])
                if value["name"] == container]
    if meta.get("deletionTimestamp") or len(selected) != 1:
        raise SystemExit(2)
    value = selected[0]
    started = value.get("state", {}).get("running", {}).get("startedAt")
    if (not value.get("ready") or not value.get("containerID") or not started
            or not isinstance(value.get("restartCount"), int)):
        raise SystemExit(2)
    rows.append((meta["name"], meta["uid"], value["containerID"],
                 str(value["restartCount"]), started))
if len({row[0] for row in rows}) != replicas or len({row[1] for row in rows}) != replicas:
    raise SystemExit(2)
print("\t".join(("@deployment", metadata["uid"], str(metadata["generation"]), str(replicas))))
for row in sorted(rows):
    if any(not field or any(char in field for char in "\t\r\n") for field in row):
        raise SystemExit(2)
    print("\t".join(row))
' "$container"
}

role_logs() {
  (
    local role="$1" container="$2" evidence="${3:-}" phase="${4:-read}"
    local before after saved pod rest temporary existing frozen=0
    [[ "$phase" = freeze || "$phase" = read ]] || return 2
    # Provider history has exactly one explicit startup freeze. A missing marker
    # on later reads is lost evidence, never permission to sample a new cohort.
    [[ "$role" != provider || -n "$evidence" ]] || return 2
    if [[ -n "$evidence" ]]; then
      [[ -d "$evidence" ]] || return 2
      if [[ "$phase" = freeze ]]; then
        existing=$(ls -A -- "$evidence") || return "$?"
        [[ -z "$existing" ]] || return 2
      else
        [[ -f "$evidence/participants" ]] || return 2
        saved=$(cat "$evidence/participants") || return "$?"
        [[ -n "$saved" ]] || return 2
        frozen=1
      fi
    else
      [[ "$phase" = read ]] || return 2
    fi
    temporary=$(mktemp -d) || return "$?"
    trap 'rm -rf -- "$temporary"' EXIT
    before=$(role_log_participants "$role" "$container") || return "$?"
    if [[ "$frozen" = 1 ]]; then
      [[ "$saved" = "$before" ]] || return 2
    fi
    : >"$temporary/all"
    while IFS=$'\t' read -r pod rest; do
      [[ "$pod" != @deployment ]] || continue
      run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
        kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" \
        logs "$pod" -c "$container" --prefix --tail=-1 >"$temporary/$pod.log" || return "$?"
      if [[ "$frozen" = 1 ]]; then
        [[ -f "$evidence/$pod.log" ]] || return 2
        # Preserve exact previously observed bytes per original process. An API
        # success after log rotation cannot erase an earlier accepted invocation.
        python3 - "$evidence/$pod.log" "$temporary/$pod.log" <<'PYLOG'
from pathlib import Path
import sys
if not Path(sys.argv[2]).read_bytes().startswith(Path(sys.argv[1]).read_bytes()):
    raise SystemExit(2)
PYLOG
        [[ "$?" = 0 ]] || return 2
      fi
      cat "$temporary/$pod.log" >>"$temporary/all" || return "$?"
      printf '\n' >>"$temporary/all" || return "$?"
    done <<<"$before"
    after=$(role_log_participants "$role" "$container") || return "$?"
    [[ "$after" = "$before" ]] || return 2
    if [[ -n "$evidence" ]]; then
      # Gate-local log evidence only; no production Registry/metric/store. Keep
      # successful full reads so the next observation can detect lost history.
      cp "$temporary/"*.log "$evidence/" || return "$?"
      if [[ "$phase" = freeze ]]; then
        # This is the existing completion marker: publish it LAST, once all
        # original process prefixes have been obtained and saved successfully.
        printf '%s\n' "$before" >"$evidence/participants" || return "$?"
      fi
    fi
    cat "$temporary/all"
  )
}

role_log_matches() {
  local role="$1" container="$2" mode="$3" output="$4" pattern="$5" evidence="${6:-}"
  local logs result status=0 flags=("$mode")
  [[ "$mode" = -F || "$mode" = -E ]] || return 2
  [[ "$output" = lines || "$output" = count ]] || return 2
  [[ "$output" != count ]] || flags+=(-c)
  logs=$(role_logs "$role" "$container" "$evidence") || return "$?"
  result=$(grep "${flags[@]}" -- "$pattern" <<<"$logs") || status=$?
  # grep 1 means no matching line; no API/transport/parser failure reaches here
  # as an empty log. Preserve grep errors (including invalid expressions).
  [[ "$status" = 0 || "$status" = 1 ]] || return "$status"
  printf '%s\n' "$result"
}

# The same owned Deployment pause/restore serves the original outage and the
# later lost-history fault. It changes no Registry/Dispatch record or evidence.
# Save restoration coordinates BEFORE scale IO, including an ambiguous failure.
pause_coordinator_observation() {
  local facts uid replicas version
  [[ -z "$COORDINATOR_RESTORE_UID" ]] || return 2
  facts=$(run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
    kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" get deploy/coordinator \
    -o jsonpath='{.metadata.uid}|{.spec.replicas}|{.metadata.resourceVersion}') || return "$?"
  IFS='|' read -r uid replicas version <<<"$facts"
  [[ -n "$uid" && "$replicas" =~ ^[1-9][0-9]*$ && -n "$version" ]] || return 2
  COORDINATOR_RESTORE_UID="$uid" COORDINATOR_RESTORE_REPLICAS="$replicas"
  run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
    kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" scale deploy/coordinator \
    --current-replicas="$replicas" --resource-version="$version" --replicas=0 >/dev/null || return "$?"
  kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" wait --for=delete pod -l app=coordinator --timeout=120s || return "$?"
}

restore_coordinator_observation() {
  local facts uid replicas version
  [[ -n "$COORDINATOR_RESTORE_UID" ]] || return 0
  facts=$(run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
    kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" get deploy/coordinator \
    -o jsonpath='{.metadata.uid}|{.spec.replicas}|{.metadata.resourceVersion}') || return "$?"
  IFS='|' read -r uid replicas version <<<"$facts"
  [[ "$uid" = "$COORDINATOR_RESTORE_UID" && -n "$version" ]] || return 2
  if [[ "$replicas" != "$COORDINATOR_RESTORE_REPLICAS" ]]; then
    [[ "$replicas" = 0 ]] || return 2
    run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
      kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" scale deploy/coordinator \
      --current-replicas=0 --resource-version="$version" --replicas="$COORDINATOR_RESTORE_REPLICAS" >/dev/null || return "$?"
  fi
  kubectl -n "$NS" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" rollout status deploy/coordinator --timeout=240s || return "$?"
  COORDINATOR_RESTORE_UID="" COORDINATOR_RESTORE_REPLICAS=""
}

# One readback command checks the actual producer's checkout and exact bytes;
# both observations use it. This never enumerates .git with the safe file walker
# or substitutes a Provider reply for actual Repository content.
verify_checkpoint_repository() {
  local pod="$1" uid="$2" commit="$3" current
  [[ -n "$pod" && -n "$uid" && "$commit" =~ ^[0-9a-f]{40}$ ]] || return 2
  current=$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.metadata.uid}') || return "$?"
  [[ "$current" = "$uid" ]] || return 2
  kubectl -n "$NS" exec "$pod" -c agent -- sh -ec \
    'test "$(git -C /workspace/source rev-parse HEAD)" = "$1"; printf "Hello World!\n" | cmp -s - /workspace/source/README' \
    awaken-checkpoint-source "$commit"
}

cleanup() {
  if [[ -n "$CAPACITY_SAMPLER" ]]; then
    kill "$CAPACITY_SAMPLER" 2>/dev/null || true
    wait "$CAPACITY_SAMPLER" 2>/dev/null || true
    CAPACITY_SAMPLER=""
  fi
  local status="$1" coordinator_status=0
  restore_coordinator_observation || coordinator_status=$?
  [ -z "$WORKER_TLS_DIR" ] || rm -rf -- "$WORKER_TLS_DIR"
  [ -z "$ROLE_OVERLAY" ] || rm -rf -- "$ROLE_OVERLAY"
  if [ "$status" -ne 0 ] && [ "${ADR71_KEEP_FAILED_CLUSTER:-0}" = "1" ]; then
    err "retaining failed k3d cluster $CLUSTER and kubeconfig $KUBECONFIG_FILE for diagnostics"
    return "$coordinator_status"
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
  # Allocated by this invocation only; retained together with a failed cluster
  # above. k3d_delete_cluster already removed it with the node volumes it
  # captured, so a failed postcondition cannot orphan it; this covers the rest.
  [ -z "$CONTINUATION_VOLUME" ] || docker volume rm "$CONTINUATION_VOLUME" >/dev/null 2>&1 || true
  return "$coordinator_status"
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
  local pod_name="$1" restart_policy="${2:-Never}"
  # One canonical Pod template covers both original Never predecessors and the
  # explicit Always in-place rule. TokenReview authenticates the projected Pod;
  # the Coordinator retains evidence before registration. No template creates
  # that finalizer or promotes readiness/restart policy into an exit receipt.
  case "$restart_policy" in
    Never|Always) ;;
    *) err "unsupported owned Native fixture restart policy"; return 2 ;;
  esac
  kubectl -n "$NS" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: ${pod_name}
  labels: { app: worker, awaken.dev/native-crash-fixture: "true" }
spec:
  serviceAccountName: worker-runtime
  automountServiceAccountToken: true
  restartPolicy: ${restart_policy}
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
  local pod="$1" expected_uid="$2" mode="${3:-ended}" expected_container="${4:-}"
  local facts uid node container restarts phase observed_container observed_uid observed_exit observed_restarts
  # One API observation binds UID, node and CRI id before the destructive
  # injection. Completion joins the exact retained UID/container, never a bare
  # name/404. Every Worker fault phase reuses this one physical injection owner.
  # Mode `ended`: a Never-restart Pod ends Failed with that exact exit.
  # Mode `in-place`: an Always Pod keeps its UID, kubelet restarts the
  # container, and `lastState` names the killed container with exit 137.
  facts=$(kubectl -n "$NS" get pod "$pod" \
    -o jsonpath='{.metadata.uid}|{.spec.nodeName}|{.status.containerStatuses[?(@.name=="worker")].containerID}|{.status.containerStatuses[?(@.name=="worker")].restartCount}')
  IFS='|' read -r uid node container restarts <<<"$facts"
  container=${container#containerd://}
  expected_container=${expected_container#containerd://}
  [ -z "$expected_container" ] || [ "$container" = "$expected_container" ] \
    || { err "the frozen Native Worker container changed before its fault"; return 1; }
  [ "$uid" = "$expected_uid" ] && [[ "$node" == "k3d-${CLUSTER}-"* ]] \
    && [[ "$container" =~ ^[0-9a-f]{64}$ ]] && [[ "$restarts" =~ ^[0-9]+$ ]] \
    || { err "invalid exact Native Worker CRI crash target"; return 1; }
  k3d_hard_stop_cri_container "$CLUSTER" "$node" "$container" || return "$?"
  for _ in $(seq 1 90); do
    if [ "$mode" = in-place ]; then
      facts=$(kubectl -n "$NS" get pod "$pod" \
        -o jsonpath='{.metadata.uid}|{.status.containerStatuses[?(@.name=="worker")].restartCount}|{.status.containerStatuses[?(@.name=="worker")].lastState.terminated.containerID}|{.status.containerStatuses[?(@.name=="worker")].lastState.terminated.exitCode}')
      IFS='|' read -r observed_uid observed_restarts observed_container observed_exit <<<"$facts"
      [ "$observed_uid" = "$expected_uid" ] && [ "$observed_restarts" = "$((restarts + 1))" ] \
        && [ "$observed_container" = "containerd://${container}" ] && [ "$observed_exit" = 137 ] && return 0
    else
      facts=$(kubectl -n "$NS" get pod "$pod" \
        -o jsonpath='{.metadata.uid}|{.status.phase}|{.status.containerStatuses[?(@.name=="worker")].state.terminated.containerID}|{.status.containerStatuses[?(@.name=="worker")].state.terminated.exitCode}')
      IFS='|' read -r observed_uid phase observed_container observed_exit <<<"$facts"
      [ "$observed_uid" = "$expected_uid" ] && [ "$phase" = "Failed" ] \
        && [ "$observed_container" = "containerd://${container}" ] && [ "$observed_exit" = 137 ] && return 0
    fi
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
CONTROL_BIN=$(resolve_cargo_executable awaken-control-process awaken-control)
COORDINATOR_BIN=$(resolve_cargo_executable awaken-coordinator-process awaken-coordinator --features container-k8s)
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
e2e_docker_init distributed-control
e2e_docker_sweep
k3d_ensure_images "$IMAGE" "$SANDBOX_IMAGE" postgres:16 nginx:1.27-alpine
CONTINUATION_VOLUME=$(docker volume create "${E2E_LABELS[@]}" --label "awaken.e2e.cluster=$CLUSTER" "awaken-e2e-$CLUSTER-continuation")
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
# C3 the drained Worker exits; kubelet restarts its container in place under
# the same Pod UID, and that new container instance registers as a new
# incarnation of the same Pod-scoped Worker ID, then becomes Ready.
WORKER_CONTAINERS_BEFORE=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].containerID}{"\n"}{end}')
WORKER_0_UID_BEFORE=$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.metadata.uid}')
WORKER_1_UID_BEFORE=$(kubectl -n "$NS" get pod worker-1 -o jsonpath='{.metadata.uid}')
pause_coordinator_observation
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
restore_coordinator_observation
node "$DRIVER" retry-publication "$API_URL"
kubectl -n "$NS" rollout status statefulset/worker --timeout=240s
WORKER_CONTAINERS_AFTER=$(kubectl -n "$NS" get pod -l app=worker -o jsonpath='{range .items[*]}{.status.containerStatuses[0].containerID}{"\n"}{end}')
[ "$WORKER_CONTAINERS_BEFORE" != "$WORKER_CONTAINERS_AFTER" ] || { err "drained Worker containers were reused"; exit 1; }
# Worker incarnation decision table (ADR-0078 amendment 2026-10-31): W1
# authority lost + exact local drain + process exit => kubelet restarts the
# container in place; W2 the new container instance registers as a new
# incarnation of the same Pod-scoped Worker ID. E1 both ordinals keep their
# Pod UID, restart in place at least once and run new container IDs; E2 an
# unchanged container ID fails this gate. The Coordinator settles each old
# incarnation from its container's exact `lastState` exit.
[ "$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.metadata.uid}')" = "$WORKER_0_UID_BEFORE" ] \
  || { err "worker-0 was replaced instead of restarted in place"; exit 1; }
[ "$(kubectl -n "$NS" get pod worker-1 -o jsonpath='{.metadata.uid}')" = "$WORKER_1_UID_BEFORE" ] \
  || { err "worker-1 was replaced instead of restarted in place"; exit 1; }
for pod in worker-0 worker-1; do
  [ "$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.status.containerStatuses[0].restartCount}')" -ge 1 ] \
    || { err "$pod did not restart its container in place"; exit 1; }
done
ok "Coordinator outage failed closed; restarted Worker containers registered as new incarnations"

log "6b/11 hard-kill one Worker container in place; its incarnation settles from lastState"
# Cause/effect decision table (ADR-0078 amendment 2026-10-31: the Worker ID is
# the Pod, an incarnation is one container instance). C1 the CRI hard-kills
# the running container of registered, retained worker-0 without deleting the
# Pod; C2 the StatefulSet Pod is restartPolicy Always. Effects: E1 the same
# Pod UID restarts the container in place (restartCount +1) and `lastState`
# names the killed container with exit 137; E2 the new container ID registers
# as a new incarnation of the same Worker ID; E3 the old incarnation records
# its exact exit 137 from `lastState` before the new one is admitted; E4 public
# work continues. Rules K1 C1+C2 => E1+E2; K2 E1 => E3; K3 E2 => E4. If the
# container restarted twice before observation, E3 is Unknown instead.
CRASHED_WORKER_UID=$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.metadata.uid}')
CRASHED_CONTAINER=$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.status.containerStatuses[0].containerID}')
CRASHED_INCARNATION=${CRASHED_CONTAINER#containerd://}
CRASHED_WORKER_ID="k8s:${NS}:worker-0:${CRASHED_WORKER_UID}"
CRASHED_REGISTERED=$(kubectl -n "$NS" exec postgres-primary-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT count(*) FROM worker_registry_worker
     WHERE worker_id = '${CRASHED_WORKER_ID}' AND incarnation_id = '${CRASHED_INCARNATION}'")
[ "$CRASHED_REGISTERED" = "1" ] || { err "worker-0 is not registered as its container instance"; exit 1; }
hard_crash_native_worker worker-0 "$CRASHED_WORKER_UID" in-place
kubectl -n "$NS" rollout status statefulset/worker --timeout=240s
[ "$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.metadata.uid}')" = "$CRASHED_WORKER_UID" ] \
  || { err "worker-0 was replaced instead of restarted in place"; exit 1; }
RESTARTED_CONTAINER=$(kubectl -n "$NS" get pod worker-0 -o jsonpath='{.status.containerStatuses[0].containerID}')
RESTARTED_INCARNATION=${RESTARTED_CONTAINER#containerd://}
[ -n "$RESTARTED_INCARNATION" ] && [ "$RESTARTED_INCARNATION" != "$CRASHED_INCARNATION" ] \
  || { err "worker-0 did not start a new container instance"; exit 1; }
for _ in $(seq 1 120); do
  RESTARTED_LEASES=$(kubectl -n "$NS" exec postgres-primary-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
    "SELECT count(*) FROM worker_registry_worker
       WHERE worker_id = '${CRASHED_WORKER_ID}' AND incarnation_id = '${RESTARTED_INCARNATION}'
       AND expires_at_ms > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)")
  [ "$RESTARTED_LEASES" = "1" ] && break
  sleep 1
done
[ "$RESTARTED_LEASES" = "1" ] || { err "restarted container never registered as a new incarnation"; exit 1; }
CRASHED_EXIT=$(kubectl -n "$NS" exec postgres-primary-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT count(*) FROM worker_registry_worker
     WHERE worker_id = '${CRASHED_WORKER_ID}' AND incarnation_id = '${CRASHED_INCARNATION}'
     AND process_exit_json LIKE '%k8s-container-exit%' AND process_exit_json LIKE '%exit_code%137%'")
[ "$CRASHED_EXIT" = "1" ] || { err "killed incarnation has no exact lastState exit 137: $CRASHED_EXIT"; exit 1; }
node "$DRIVER" verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-WORKER-CRASH
ok "killed Worker container restarted in place, its old incarnation settled exit 137, and work continued"

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
# The existing target/required-execution owners retain the caller's private
# target and bind one independently listed, executed and non-skipped case.
source "$REPO_ROOT/scripts/ci/_cargo_target.sh"
awaken_configure_cargo_target "$REPO_ROOT"
source "$REPO_ROOT/scripts/ci/_rust_test_execution.sh"
TRANSPORT_TEST=worker_transport::kubernetes::tests::live::real_pod_identity_survives_reopen_and_complete_evidence_retires_exact_attempt
# T1 current package/feature/lib target + full case + live switches must all
# remain. The canonical required executor lists and runs this same argv, rejects
# zero selection/skip/failure and retains its original result/receipt. A second
# TRANSPORT_TESTS/grep selector would duplicate that observation authority.
AWAKEN_K8S_E2E=1 AWAKEN_K3D_CLUSTER="$CLUSTER" AWAKEN_K8S_FIXTURE_IMAGE="$IMAGE" \
  AWAKEN_K8S_CONTROL_IMAGE="$SANDBOX_IMAGE" \
  run_required_rust_tests -p awaken-process-startup --features container-k8s --lib --locked \
  "$TRANSPORT_TEST" --require-case="$TRANSPORT_TEST" -- --exact
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
# L1-L8 below observe the same fresh-cluster Provider cohort from BEFORE the
# first unique Native marker through the final Q4 count. Keep full log evidence
# beside this run's existing local logs; no rolling time window can hide replay.
PROVIDER_LOG_EVIDENCE=$(mktemp -d "${TMPDIR:-/tmp}/${CLUSTER}_provider_logs.XXXXXX")
role_logs provider provider "$PROVIDER_LOG_EVIDENCE" freeze >/dev/null
printf 'PROVIDER_LOG_EVIDENCE_PATH=%s\n' "$PROVIDER_LOG_EVIDENCE"
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
  FINISH_REJECTED=$(role_log_matches coordinator worker-tls -E count \
    'POST /v1/worker/dispatch/attempt/finish.* 503 ')
  [ "$FINISH_REJECTED" -gt 0 ] && break
  sleep 1
done
[ "$FINISH_REJECTED" -gt 0 ] \
  || { err "old Native Worker never reached the held finish ACK boundary"; exit 1; }
NATIVE_PROVIDER_BEFORE_CRASH=$(role_log_matches provider provider -F count "ADR71_PROVIDER_REQUEST \"${NATIVE_CRASH_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
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
NATIVE_PROVIDER_REQUESTS=$(role_log_matches provider provider -F count "ADR71_PROVIDER_REQUEST \"${NATIVE_CRASH_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
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
  PROVIDER_ARRIVAL=$(role_log_matches provider provider -F lines "ADR71_PROVIDER_REQUEST \"${NATIVE_INFLIGHT_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
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
NATIVE_INFLIGHT_PROVIDER_REQUESTS=$(role_log_matches provider provider -F count "ADR71_PROVIDER_REQUEST \"${NATIVE_INFLIGHT_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
[ "$NATIVE_INFLIGHT_PROVIDER_REQUESTS" = "1" ] || { err "in-flight invocation was replayed across Provider replicas"; exit 1; }
ok "independent Provider double completed; original unknown journal/slot retained, no replay, independent successor work passed"

log "11c/11 recover the original terminal Run through one same-UID container restart"
# In-place decision table: IP0 one original SDK Event, one Provider response,
# durable NaturalEnd commit and generation1 finish503 freeze the occupied Run,
# receipt, Thread, owner/epoch, Pod UID and container A before the fault. IP1 the
# canonical CRI owner kills only A -> same UID/new B/lastState A137 and a greater
# registered generation. IP2 existing recovery settles that same Run/receipt:
# no new Event, no second Provider call, no Cancelled or replacement terminal
# commit, and no Pod deletion. Missing/multiple/changed coordinates refuse before
# injection or fail the observation; Ready/404 never substitutes for evidence.
# This is terminal-commit/ACK recovery, not a Working/Awaiting resume or a claim
# that an opaque remote effect completed (11b remains Unknown unchanged).
NATIVE_IDLE_UID=$(kubectl -n "$NS" get pod "$NATIVE_INFLIGHT_NEXT" -o jsonpath='{.metadata.uid}')
hard_crash_native_worker "$NATIVE_INFLIGHT_NEXT" "$NATIVE_IDLE_UID"
create_native_worker_pod "$NATIVE_INPLACE" Always
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_INPLACE" --timeout=240s
NATIVE_INPLACE_FACTS=$(kubectl -n "$NS" get pod "$NATIVE_INPLACE" \
  -o jsonpath='{.metadata.uid}|{.status.containerStatuses[?(@.name=="worker")].containerID}')
IFS='|' read -r NATIVE_INPLACE_UID NATIVE_INPLACE_CONTAINER <<<"$NATIVE_INPLACE_FACTS"
NATIVE_INPLACE_INCARNATION=${NATIVE_INPLACE_CONTAINER#containerd://}
[[ "$NATIVE_INPLACE_UID" =~ ^[0-9a-f-]+$ ]] && [[ "$NATIVE_INPLACE_INCARNATION" =~ ^[0-9a-f]{64}$ ]] \
  || { err "IP0 has no exact original Pod/container"; exit 1; }
NATIVE_INPLACE_WORKER="k8s:${NS}:${NATIVE_INPLACE}:${NATIVE_INPLACE_UID}"
for _ in $(seq 1 120); do
  NATIVE_INPLACE_GENERATION=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
    "SELECT generation FROM worker_registry_worker
       WHERE worker_id='${NATIVE_INPLACE_WORKER}' AND incarnation_id='${NATIVE_INPLACE_INCARNATION}'
         AND expires_at_ms > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)")
  [ "$NATIVE_INPLACE_GENERATION" = "1" ] && break
  sleep 1
done
[ "$NATIVE_INPLACE_GENERATION" = "1" ] || { err "IP0 original generation1 never registered"; exit 1; }
NATIVE_INPLACE_OWNER="${NATIVE_INPLACE_WORKER}:1:${NATIVE_INPLACE_INCARNATION}"
NATIVE_PREPARE=$(node "$DRIVER" prepare-native-crash \
  "$API_URL" "$ENVIRONMENT_ID" "$NATIVE_INPLACE_MARKER" | tail -1)
read -r NATIVE_PREPARED NATIVE_INPLACE_SESSION NATIVE_INPLACE_RECEIPT <<<"$NATIVE_PREPARE"
[ "$NATIVE_PREPARED" = "OK" ] && [ -n "$NATIVE_INPLACE_SESSION" ] && [ -n "$NATIVE_INPLACE_RECEIPT" ] \
  || { err "IP0 original Event identities are incomplete"; exit 1; }
for _ in $(seq 1 60); do
  NATIVE_INPLACE_FINISH=$(role_log_matches coordinator worker-tls -E count \
    "POST /v1/worker/dispatch/attempt/finish.* 503 .*${NATIVE_INPLACE_WORKER}.*${NATIVE_INPLACE_INCARNATION}")
  [ "$NATIVE_INPLACE_FINISH" -gt 0 ] && break
  sleep 1
done
[ "$NATIVE_INPLACE_FINISH" -gt 0 ] || { err "IP0 exact incarnation never reached held finish503"; exit 1; }
NATIVE_INPLACE_PROVIDER=$(role_log_matches provider provider -F count "ADR71_PROVIDER_REQUEST \"${NATIVE_INPLACE_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
[ "$NATIVE_INPLACE_PROVIDER" = "1" ] || { err "IP0 did not execute exactly one Provider request"; exit 1; }
NATIVE_INPLACE_DISPATCH=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT run_id || '|' || active_attempt_epoch::text FROM runtime_dispatch
     WHERE request::text LIKE '%${NATIVE_INPLACE_SESSION}%' AND active_attempt_owner='${NATIVE_INPLACE_OWNER}'")
[ "$(grep -c . <<<"$NATIVE_INPLACE_DISPATCH")" = "1" ] || { err "IP0 requires one exact occupied Run"; exit 1; }
IFS='|' read -r NATIVE_INPLACE_RUN NATIVE_INPLACE_EPOCH <<<"$NATIVE_INPLACE_DISPATCH"
[ -n "$NATIVE_INPLACE_RUN" ] && [[ "$NATIVE_INPLACE_EPOCH" =~ ^[1-9][0-9]*$ ]] \
  || { err "IP0 original Run/epoch malformed"; exit 1; }
NATIVE_INPLACE_ADMITTED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT count(*) FROM runtime_dispatch_operation WHERE run_id='${NATIVE_INPLACE_RUN}'
     AND operation->>'type'='attempt_admitted'
     AND operation->'attempt'->'claim'->>'owner'='${NATIVE_INPLACE_OWNER}'
     AND operation->'attempt'->'claim'->>'epoch'='${NATIVE_INPLACE_EPOCH}'")
[ "$NATIVE_INPLACE_ADMITTED" = "1" ] || { err "IP0 lacks the original exact physical admission"; exit 1; }
NATIVE_INPLACE_COMMIT=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT sequence::text || '|' || thread_id FROM runtime_commit
     WHERE run_id='${NATIVE_INPLACE_RUN}' AND phase::jsonb=jsonb_build_object('Ended','NaturalEnd')")
[ "$(grep -c . <<<"$NATIVE_INPLACE_COMMIT")" = "1" ] || { err "IP0 lacks the one original NaturalEnd commit"; exit 1; }
IFS='|' read -r NATIVE_INPLACE_SEQUENCE NATIVE_INPLACE_THREAD <<<"$NATIVE_INPLACE_COMMIT"
[[ "$NATIVE_INPLACE_SEQUENCE" =~ ^[0-9]+$ ]] && [ -n "$NATIVE_INPLACE_THREAD" ] \
  || { err "IP0 original commit coordinates malformed"; exit 1; }
hard_crash_native_worker "$NATIVE_INPLACE" "$NATIVE_INPLACE_UID" in-place "$NATIVE_INPLACE_CONTAINER"
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_INPLACE" --timeout=240s
NATIVE_RESTART_FACTS=$(kubectl -n "$NS" get pod "$NATIVE_INPLACE" \
  -o jsonpath='{.metadata.uid}|{.status.containerStatuses[?(@.name=="worker")].containerID}|{.metadata.deletionTimestamp}')
IFS='|' read -r NATIVE_RESTART_UID NATIVE_RESTART_CONTAINER NATIVE_RESTART_DELETION <<<"$NATIVE_RESTART_FACTS"
NATIVE_RESTART_INCARNATION=${NATIVE_RESTART_CONTAINER#containerd://}
[ "$NATIVE_RESTART_UID" = "$NATIVE_INPLACE_UID" ] && [ -z "$NATIVE_RESTART_DELETION" ] \
  && [[ "$NATIVE_RESTART_INCARNATION" =~ ^[0-9a-f]{64}$ ]] \
  && [ "$NATIVE_RESTART_INCARNATION" != "$NATIVE_INPLACE_INCARNATION" ] \
  || { err "IP1 replaced/deleted the Pod or reused container A"; exit 1; }
for _ in $(seq 1 120); do
  NATIVE_RESTART_REGISTERED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
    "SELECT count(*) FROM worker_registry_worker
       WHERE worker_id='${NATIVE_INPLACE_WORKER}' AND incarnation_id='${NATIVE_RESTART_INCARNATION}'
         AND generation > ${NATIVE_INPLACE_GENERATION}
         AND expires_at_ms > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)")
  [ "$NATIVE_RESTART_REGISTERED" = "1" ] && break
  sleep 1
done
[ "$NATIVE_RESTART_REGISTERED" = "1" ] || { err "IP1 new container never registered a greater generation"; exit 1; }
node "$DRIVER" verify-native-crash \
  "$API_URL" "$NATIVE_INPLACE_SESSION" "$NATIVE_INPLACE_RECEIPT" "$NATIVE_INPLACE_MARKER"
NATIVE_INPLACE_FINAL=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT
    (SELECT count(*) FROM runtime_dispatch WHERE run_id='${NATIVE_INPLACE_RUN}')::text || '|' ||
    (SELECT count(*) FROM runtime_dispatch_operation WHERE run_id='${NATIVE_INPLACE_RUN}'
       AND operation->>'type'='settled' AND operation->>'outcome'='Done')::text || '|' ||
    (SELECT count(*) FROM runtime_commit WHERE run_id='${NATIVE_INPLACE_RUN}'
       AND thread_id='${NATIVE_INPLACE_THREAD}' AND sequence=${NATIVE_INPLACE_SEQUENCE}
       AND phase::jsonb=jsonb_build_object('Ended','NaturalEnd'))::text || '|' ||
    (SELECT count(*) FROM runtime_commit WHERE run_id='${NATIVE_INPLACE_RUN}' AND phase::jsonb ? 'Ended')::text || '|' ||
    (SELECT count(*) FROM worker_registry_worker WHERE worker_id='${NATIVE_INPLACE_WORKER}'
       AND incarnation_id='${NATIVE_INPLACE_INCARNATION}' AND generation=${NATIVE_INPLACE_GENERATION}
       AND process_exit_json LIKE '%k8s-container-exit%'
       AND process_exit_json LIKE '%${NATIVE_INPLACE_INCARNATION}%' AND process_exit_json LIKE '%exit_code%137%')::text")
[ "$NATIVE_INPLACE_FINAL" = "0|1|1|1|1" ] \
  || { err "IP2 original Run/terminal/physical evidence changed: $NATIVE_INPLACE_FINAL"; exit 1; }
NATIVE_INPLACE_PROVIDER=$(role_log_matches provider provider -F count "ADR71_PROVIDER_REQUEST \"${NATIVE_INPLACE_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
[ "$NATIVE_INPLACE_PROVIDER" = "1" ] || { err "IP2 original Run reissued its Provider request"; exit 1; }
NATIVE_RESTART_FACTS=$(kubectl -n "$NS" get pod "$NATIVE_INPLACE" \
  -o jsonpath='{.metadata.uid}|{.metadata.deletionTimestamp}')
[ "$NATIVE_RESTART_FACTS" = "${NATIVE_INPLACE_UID}|" ] \
  || { err "IP2 original Pod was removed/replaced while retiring only A"; exit 1; }
ok "same Pod UID/new registered incarnation recovered the original terminal Run and receipt without Cancelled/model replay"

log "11d/11 keep the original occupied Awaiting Run through Superseded and source Rebuild"
# Matrix causes/effects: Q0 one real client-tool request has Returned and the
# original Runtime Awaiting commit/ticket is durable, while A's finish ACK is
# held. Q1 all owned Coordinator observers are absent; exact A->B->C CRI faults
# overwrite lastState with B under the SAME Pod UID. B is running, not Registry
# Ready. Q2 restore only the original Coordinator UID/replicas -> A Superseded,
# C is the next registered generation, original Run/ticket still Awaiting with
# no Cancelled or duplicate model step. Q3 source is the real pinned HTTPS Repo;
# close only its original Pod with UID+RV, then exact C->D while producers are
# absent. Q4 restored D consumes the SAME qualified client result -> original
# Run NaturalEnd, Root Rebuild, new exact environment binding and real README.
# Every failure restores the original cohort before owned cleanup. No Store
# row, Source boolean, finalizer or PVC is rewritten/deleted to obtain evidence.
# The prior Always fixture has completed its required Run; remove only that
# exact auxiliary Pod AFTER its no-deletion/recovery assertions above. This is
# fixture retirement, never an exit/closure/recovery oracle for the new case.
CHECKPOINT_IDLE_META=$(kubectl -n "$NS" get pod "$NATIVE_INPLACE" \
  -o jsonpath='{.metadata.uid}|{.metadata.resourceVersion}')
IFS='|' read -r CHECKPOINT_IDLE_UID CHECKPOINT_IDLE_RV <<<"$CHECKPOINT_IDLE_META"
[ "$CHECKPOINT_IDLE_UID" = "$NATIVE_INPLACE_UID" ] && [ -n "$CHECKPOINT_IDLE_RV" ] \
  || { err "Q0 auxiliary fixture identity changed"; exit 1; }
k3d_delete_exact_pod "$CLUSTER" "$NS" "$NATIVE_INPLACE" "$CHECKPOINT_IDLE_UID" "$CHECKPOINT_IDLE_RV"
kubectl -n "$NS" wait --for=delete "pod/$NATIVE_INPLACE" --timeout=120s
create_native_worker_pod "$NATIVE_CHECKPOINT" Always
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_CHECKPOINT" --timeout=240s
CHECKPOINT_A_META=$(kubectl -n "$NS" get pod "$NATIVE_CHECKPOINT" \
  -o jsonpath='{.metadata.uid}|{.status.containerStatuses[?(@.name=="worker")].containerID}|{.status.containerStatuses[?(@.name=="worker")].state.running.startedAt}|{.status.containerStatuses[?(@.name=="worker")].restartCount}')
IFS='|' read -r CHECKPOINT_UID CHECKPOINT_A_CONTAINER CHECKPOINT_A_STARTED CHECKPOINT_A_RESTARTS <<<"$CHECKPOINT_A_META"
CHECKPOINT_A_INCARNATION=${CHECKPOINT_A_CONTAINER#containerd://}
[[ "$CHECKPOINT_UID" =~ ^[0-9a-f-]+$ ]] && [[ "$CHECKPOINT_A_INCARNATION" =~ ^[0-9a-f]{64}$ ]] \
  && [ -n "$CHECKPOINT_A_STARTED" ] && [[ "$CHECKPOINT_A_RESTARTS" =~ ^[0-9]+$ ]] \
  || { err "Q0 has no exact running A"; exit 1; }
CHECKPOINT_WORKER="k8s:${NS}:${NATIVE_CHECKPOINT}:${CHECKPOINT_UID}"
for _ in $(seq 1 120); do
  CHECKPOINT_A_GENERATION=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
    "SELECT generation FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}'
       AND incarnation_id='${CHECKPOINT_A_INCARNATION}'
       AND expires_at_ms > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000)")
  [ "$CHECKPOINT_A_GENERATION" = 1 ] && break
  sleep 1
done
[ "$CHECKPOINT_A_GENERATION" = 1 ] || { err "Q0 requires actual registered generation1"; exit 1; }
CHECKPOINT_A_OWNER="${CHECKPOINT_WORKER}:1:${CHECKPOINT_A_INCARNATION}"
CHECKPOINT_PREPARE=$(node "$DRIVER" prepare-native-checkpoint \
  "$API_URL" "$ENVIRONMENT_ID" "$NATIVE_CHECKPOINT_MARKER" | tail -1)
read -r CHECKPOINT_PREPARED CHECKPOINT_SESSION CHECKPOINT_RECEIPT CHECKPOINT_SOURCE_COMMIT <<<"$CHECKPOINT_PREPARE"
[ "$CHECKPOINT_PREPARED" = OK ] && [ -n "$CHECKPOINT_SESSION" ] && [ -n "$CHECKPOINT_RECEIPT" ] \
  && [[ "$CHECKPOINT_SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
  || { err "Q0 original checkpoint Event identities missing"; exit 1; }
for _ in $(seq 1 120); do
  CHECKPOINT_HELD=$(role_log_matches coordinator worker-tls -E count \
    "POST /v1/worker/dispatch/attempt/finish.* 503 .*${CHECKPOINT_WORKER}.*${CHECKPOINT_A_INCARNATION}")
  [ "$CHECKPOINT_HELD" -gt 0 ] && break
  sleep 1
done
[ "$CHECKPOINT_HELD" -gt 0 ] || { err "Q0 original A never reached held finish"; exit 1; }
CHECKPOINT_CLAIM=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT run_id || '|' || active_attempt_epoch::text FROM runtime_dispatch
     WHERE request::text LIKE '%${CHECKPOINT_SESSION}%' AND active_attempt_owner='${CHECKPOINT_A_OWNER}'")
[ "$(grep -c . <<<"$CHECKPOINT_CLAIM")" = 1 ] || { err "Q0 did not retain one original physical admission"; exit 1; }
IFS='|' read -r CHECKPOINT_RUN CHECKPOINT_EPOCH <<<"$CHECKPOINT_CLAIM"
[ -n "$CHECKPOINT_RUN" ] && [[ "$CHECKPOINT_EPOCH" =~ ^[1-9][0-9]*$ ]] \
  || { err "Q0 original Run/epoch malformed"; exit 1; }
CHECKPOINT_ADMITTED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT count(*) FROM runtime_dispatch_operation WHERE run_id='${CHECKPOINT_RUN}'
     AND operation->>'type'='attempt_admitted'
     AND operation->'attempt'->'claim'->>'owner'='${CHECKPOINT_A_OWNER}'
     AND operation->'attempt'->'claim'->>'epoch'='${CHECKPOINT_EPOCH}'")
[ "$CHECKPOINT_ADMITTED" = 1 ] || { err "Q0 lacks the original exact physical admission record"; exit 1; }
CHECKPOINT_AWAITING_COMMIT=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT sequence::text || '|' || thread_id FROM runtime_commit
     WHERE run_id='${CHECKPOINT_RUN}' AND phase::jsonb=to_jsonb('Awaiting'::text)")
[ "$(grep -c . <<<"$CHECKPOINT_AWAITING_COMMIT")" = 1 ] || { err "Q0 lacks exactly one safe Awaiting commit"; exit 1; }
IFS='|' read -r CHECKPOINT_SEQUENCE CHECKPOINT_THREAD <<<"$CHECKPOINT_AWAITING_COMMIT"
[[ "$CHECKPOINT_SEQUENCE" =~ ^[1-9][0-9]*$ ]] && [ -n "$CHECKPOINT_THREAD" ] \
  || { err "Q0 original Awaiting commit coordinates malformed"; exit 1; }
CHECKPOINT_TICKET=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT ticket::text FROM runtime_waiting WHERE run_id='${CHECKPOINT_RUN}'")
[ "$(grep -c . <<<"$CHECKPOINT_TICKET")" = 1 ] || { err "Q0 has no exact committed resume ticket"; exit 1; }
CHECKPOINT_SOURCE_POD=$(k3d_sandbox_pod_for_session "$NS" "$CHECKPOINT_SESSION")
CHECKPOINT_SOURCE_UID=$(kubectl -n "$NS" get pod "$CHECKPOINT_SOURCE_POD" -o jsonpath='{.metadata.uid}')
# This fixed-Retained topology has a real claim. Its presence alone is not a
# source-access fact; record identity without deleting it to manufacture M5.
CHECKPOINT_SOURCE_CLAIM=$(kubectl -n "$NS" get pod "$CHECKPOINT_SOURCE_POD" -o jsonpath='{.spec.volumes[*].persistentVolumeClaim.claimName}')
[[ -n "$CHECKPOINT_SOURCE_CLAIM" && "$CHECKPOINT_SOURCE_CLAIM" != *' '* ]] \
  || { err "Q0 retained topology did not expose its one actual claim"; exit 1; }
CHECKPOINT_SOURCE_CLAIM_UID=$(kubectl -n "$NS" get pvc "$CHECKPOINT_SOURCE_CLAIM" -o jsonpath='{.metadata.uid}')
[ -n "$CHECKPOINT_SOURCE_CLAIM_UID" ] || { err "Q0 retained source claim lacks identity"; exit 1; }
verify_checkpoint_repository "$CHECKPOINT_SOURCE_POD" "$CHECKPOINT_SOURCE_UID" "$CHECKPOINT_SOURCE_COMMIT"
CHECKPOINT_BINDING_BEFORE=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT aggregate_json::jsonb #>> '{aggregate,environment,binding}' FROM managed_session
     WHERE session_id='${CHECKPOINT_SESSION}' AND aggregate_json::jsonb->>'format'='awaken.session.v2'
       AND aggregate_json::jsonb #>> '{aggregate,environment,phase}'='resident'")
[ -n "$CHECKPOINT_SOURCE_UID" ] && [ -n "$CHECKPOINT_BINDING_BEFORE" ] \
  || { err "Q0 original source has no physical UID/Root-owned binding"; exit 1; }
pause_coordinator_observation
CHECKPOINT_COORDINATOR_UID="$COORDINATOR_RESTORE_UID"
CHECKPOINT_COORDINATOR_REPLICAS="$COORDINATOR_RESTORE_REPLICAS"
CHECKPOINT_UNRECORDED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT count(*) FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}'
     AND incarnation_id='${CHECKPOINT_A_INCARNATION}' AND process_exit_json IS NULL")
[ "$CHECKPOINT_UNRECORDED" = 1 ] || { err "Q1 A's history was already recorded before withholding"; exit 1; }
hard_crash_native_worker "$NATIVE_CHECKPOINT" "$CHECKPOINT_UID" in-place "$CHECKPOINT_A_CONTAINER"
kubectl -n "$NS" wait '--for=jsonpath={.status.containerStatuses[?(@.name=="worker")].state.running.startedAt}' \
  "pod/$NATIVE_CHECKPOINT" --timeout=90s
CHECKPOINT_B_META=$(kubectl -n "$NS" get pod "$NATIVE_CHECKPOINT" \
  -o jsonpath='{.metadata.uid}|{.status.containerStatuses[?(@.name=="worker")].containerID}|{.status.containerStatuses[?(@.name=="worker")].state.running.startedAt}')
IFS='|' read -r CHECKPOINT_B_UID CHECKPOINT_B_CONTAINER CHECKPOINT_B_STARTED <<<"$CHECKPOINT_B_META"
[ "$CHECKPOINT_B_UID" = "$CHECKPOINT_UID" ] && [ "$CHECKPOINT_B_CONTAINER" != "$CHECKPOINT_A_CONTAINER" ] \
  && [ -n "$CHECKPOINT_B_STARTED" ] || { err "Q1 B is not a real same-UID running instance"; exit 1; }
CHECKPOINT_B_INCARNATION=${CHECKPOINT_B_CONTAINER#containerd://}
CHECKPOINT_B_REGISTERED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT count(*) FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}' AND incarnation_id='${CHECKPOINT_B_INCARNATION}'")
[ "$CHECKPOINT_B_REGISTERED" = 0 ] || { err "Q1 withheld B unexpectedly registered"; exit 1; }
hard_crash_native_worker "$NATIVE_CHECKPOINT" "$CHECKPOINT_UID" in-place "$CHECKPOINT_B_CONTAINER"
kubectl -n "$NS" wait '--for=jsonpath={.status.containerStatuses[?(@.name=="worker")].state.running.startedAt}' \
  "pod/$NATIVE_CHECKPOINT" --timeout=90s
CHECKPOINT_C_META=$(kubectl -n "$NS" get pod "$NATIVE_CHECKPOINT" \
  -o jsonpath='{.metadata.uid}|{.status.containerStatuses[?(@.name=="worker")].containerID}|{.status.containerStatuses[?(@.name=="worker")].state.running.startedAt}|{.status.containerStatuses[?(@.name=="worker")].restartCount}|{.status.containerStatuses[?(@.name=="worker")].lastState.terminated.containerID}|{.status.containerStatuses[?(@.name=="worker")].lastState.terminated.exitCode}')
python3 - "$CHECKPOINT_UID" "$CHECKPOINT_A_CONTAINER" "$CHECKPOINT_A_STARTED" "$CHECKPOINT_A_RESTARTS" "$CHECKPOINT_B_CONTAINER" "$CHECKPOINT_C_META" <<'PYCHECK'
import datetime,sys
uid,a,started,count,b,facts=sys.argv[1:]
actual,c,c_started,restarts,last,code=facts.split('|')
assert actual==uid and c not in (a,b) and last==b and code=='137'
assert int(restarts)==int(count)+2
assert datetime.datetime.fromisoformat(c_started.replace('Z','+00:00')) > datetime.datetime.fromisoformat(started.replace('Z','+00:00'))
PYCHECK
IFS='|' read -r _ CHECKPOINT_C_CONTAINER _ _ _ _ <<<"$CHECKPOINT_C_META"
CHECKPOINT_C_INCARNATION=${CHECKPOINT_C_CONTAINER#containerd://}
CHECKPOINT_UNRECORDED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT count(*) FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}'
     AND incarnation_id='${CHECKPOINT_A_INCARNATION}' AND process_exit_json IS NULL")
[ "$CHECKPOINT_UNRECORDED" = 1 ] || { err "Q1 A gained an observation while all producers were absent"; exit 1; }
restore_coordinator_observation
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_CHECKPOINT" --timeout=240s
for _ in $(seq 1 120); do
  CHECKPOINT_RECOVERED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
    "SELECT
      (SELECT count(*) FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}'
        AND incarnation_id='${CHECKPOINT_C_INCARNATION}' AND generation=2
        AND expires_at_ms > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000))::text || '|' ||
      (SELECT count(*) FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}'
        AND incarnation_id='${CHECKPOINT_A_INCARNATION}' AND generation=1
        AND process_exit_json::jsonb->>'end'='superseded'
        AND process_exit_json LIKE '%k8s-container-superseded%' AND process_exit_json LIKE '%${CHECKPOINT_A_INCARNATION}%')::text || '|' ||
      (SELECT count(*) FROM runtime_dispatch WHERE run_id='${CHECKPOINT_RUN}' AND status='awaiting' AND active_attempt_owner IS NULL)::text")
  [ "$CHECKPOINT_RECOVERED" = '1|1|1' ] && break
  sleep 1
done
[ "$CHECKPOINT_RECOVERED" = '1|1|1' ] || { err "Q2 old exact supersession/original Awaiting did not recover: $CHECKPOINT_RECOVERED"; exit 1; }
CHECKPOINT_CURRENT_TICKET=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT ticket::text FROM runtime_waiting WHERE run_id='${CHECKPOINT_RUN}'")
[ "$CHECKPOINT_CURRENT_TICKET" = "$CHECKPOINT_TICKET" ] || { err "Q2 original resume ticket changed"; exit 1; }
CHECKPOINT_PUBLIC=$(node "$DRIVER" observe-native-checkpoint "$API_URL" "$CHECKPOINT_SESSION" "$CHECKPOINT_RECEIPT" | tail -1)
read -r CHECKPOINT_OBSERVED CHECKPOINT_OBSERVED_SESSION CHECKPOINT_OBSERVED_RECEIPT CHECKPOINT_TOOL_USE <<<"$CHECKPOINT_PUBLIC"
[ "$CHECKPOINT_OBSERVED" = OK ] && [ "$CHECKPOINT_OBSERVED_SESSION" = "$CHECKPOINT_SESSION" ] \
  && [ "$CHECKPOINT_OBSERVED_RECEIPT" = "$CHECKPOINT_RECEIPT" ] && [ -n "$CHECKPOINT_TOOL_USE" ] \
  || { err "Q2 original public checkpoint identity missing"; exit 1; }
CHECKPOINT_PROVIDER_CALLS=$(role_log_matches provider provider -F count "ADR71_PROVIDER_REQUEST \"${NATIVE_CHECKPOINT_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
[ "$CHECKPOINT_PROVIDER_CALLS" = 1 ] || { err "Q2 safe checkpoint model step was reissued"; exit 1; }
# A second owned producer pause prevents D from eagerly adopting the still-live
# source before its fault. Retained PVCs are NEVER deleted to manufacture M5.
pause_coordinator_observation
[ "$COORDINATOR_RESTORE_UID" = "$CHECKPOINT_COORDINATOR_UID" ] \
  && [ "$COORDINATOR_RESTORE_REPLICAS" = "$CHECKPOINT_COORDINATOR_REPLICAS" ] \
  || { err "Q3 original Coordinator cohort changed between faults"; exit 1; }
CHECKPOINT_SOURCE_META=$(kubectl -n "$NS" get pod "$CHECKPOINT_SOURCE_POD" \
  -o jsonpath='{.metadata.uid}|{.metadata.resourceVersion}')
IFS='|' read -r CHECKPOINT_SOURCE_CURRENT_UID CHECKPOINT_SOURCE_RV <<<"$CHECKPOINT_SOURCE_META"
[ "$CHECKPOINT_SOURCE_CURRENT_UID" = "$CHECKPOINT_SOURCE_UID" ] && [ -n "$CHECKPOINT_SOURCE_RV" ] \
  || { err "Q3 source identity changed before its one fault"; exit 1; }
k3d_delete_exact_pod "$CLUSTER" "$NS" "$CHECKPOINT_SOURCE_POD" "$CHECKPOINT_SOURCE_UID" "$CHECKPOINT_SOURCE_RV"
hard_crash_native_worker "$NATIVE_CHECKPOINT" "$CHECKPOINT_UID" in-place "$CHECKPOINT_C_CONTAINER"
kubectl -n "$NS" wait '--for=jsonpath={.status.containerStatuses[?(@.name=="worker")].state.running.startedAt}' \
  "pod/$NATIVE_CHECKPOINT" --timeout=90s
CHECKPOINT_D_META=$(kubectl -n "$NS" get pod "$NATIVE_CHECKPOINT" \
  -o jsonpath='{.metadata.uid}|{.status.containerStatuses[?(@.name=="worker")].containerID}|{.status.containerStatuses[?(@.name=="worker")].state.running.startedAt}|{.status.containerStatuses[?(@.name=="worker")].restartCount}|{.status.containerStatuses[?(@.name=="worker")].lastState.terminated.containerID}|{.status.containerStatuses[?(@.name=="worker")].lastState.terminated.exitCode}')
python3 - "$CHECKPOINT_UID" "$CHECKPOINT_C_CONTAINER" "$CHECKPOINT_C_META" "$CHECKPOINT_D_META" <<'PYCHECK'
import datetime,sys
uid,c,old,facts=sys.argv[1:]
_,_,started,count,_,_=old.split('|')
actual,d,d_started,restarts,last,code=facts.split('|')
assert actual==uid and d!=c and last==c and code=='137'
assert int(restarts)==int(count)+1
assert datetime.datetime.fromisoformat(d_started.replace('Z','+00:00')) > datetime.datetime.fromisoformat(started.replace('Z','+00:00'))
PYCHECK
IFS='|' read -r _ CHECKPOINT_D_CONTAINER _ _ _ _ <<<"$CHECKPOINT_D_META"
CHECKPOINT_D_INCARNATION=${CHECKPOINT_D_CONTAINER#containerd://}
restore_coordinator_observation
kubectl -n "$NS" wait --for=condition=Ready "pod/$NATIVE_CHECKPOINT" --timeout=240s
for _ in $(seq 1 120); do
  CHECKPOINT_D_REGISTERED=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
    "SELECT
      (SELECT count(*) FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}'
        AND incarnation_id='${CHECKPOINT_D_INCARNATION}' AND generation=3
        AND expires_at_ms > FLOOR(EXTRACT(EPOCH FROM clock_timestamp())*1000))::text || '|' ||
      (SELECT count(*) FROM worker_registry_worker WHERE worker_id='${CHECKPOINT_WORKER}'
        AND incarnation_id='${CHECKPOINT_C_INCARNATION}' AND generation=2
        AND process_exit_json LIKE '%k8s-container-exit%' AND process_exit_json LIKE '%${CHECKPOINT_C_INCARNATION}%'
        AND process_exit_json LIKE '%exit_code%137%')::text")
  [ "$CHECKPOINT_D_REGISTERED" = '1|1' ] && break
  sleep 1
done
[ "$CHECKPOINT_D_REGISTERED" = '1|1' ] || { err "Q3 D registration/exact C exit did not join: $CHECKPOINT_D_REGISTERED"; exit 1; }
node "$DRIVER" resume-native-checkpoint "$API_URL" "$CHECKPOINT_SESSION" "$CHECKPOINT_RECEIPT" \
  "$CHECKPOINT_TOOL_USE" "$NATIVE_CHECKPOINT_MARKER"
CHECKPOINT_SOURCE_REPLACEMENT=$(k3d_sandbox_pod_for_session "$NS" "$CHECKPOINT_SESSION")
CHECKPOINT_SOURCE_NEW_UID=$(kubectl -n "$NS" get pod "$CHECKPOINT_SOURCE_REPLACEMENT" -o jsonpath='{.metadata.uid}')
[ -n "$CHECKPOINT_SOURCE_NEW_UID" ] && [ "$CHECKPOINT_SOURCE_NEW_UID" != "$CHECKPOINT_SOURCE_UID" ] \
  || { err "Q4 source was not actually replaced"; exit 1; }
verify_checkpoint_repository "$CHECKPOINT_SOURCE_REPLACEMENT" "$CHECKPOINT_SOURCE_NEW_UID" "$CHECKPOINT_SOURCE_COMMIT"
CHECKPOINT_BINDING_AFTER=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT aggregate_json::jsonb #>> '{aggregate,environment,binding}' FROM managed_session
     WHERE session_id='${CHECKPOINT_SESSION}' AND aggregate_json::jsonb->>'format'='awaken.session.v2'
       AND aggregate_json::jsonb #>> '{aggregate,environment,phase}'='resident'
       AND (aggregate_json::jsonb #> '{aggregate,environment_materialization,origin,kind}') ? 'rebuild'")
[ -n "$CHECKPOINT_BINDING_AFTER" ] && [ "$CHECKPOINT_BINDING_AFTER" != "$CHECKPOINT_BINDING_BEFORE" ] \
  || { err "Q4 original Root has no new authorized Rebuild binding"; exit 1; }
CHECKPOINT_FINAL=$(kubectl -n "$NS" exec postgres-standby-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 -c \
  "SELECT
    (SELECT count(*) FROM runtime_dispatch WHERE run_id='${CHECKPOINT_RUN}')::text || '|' ||
    (SELECT count(*) FROM runtime_waiting WHERE run_id='${CHECKPOINT_RUN}')::text || '|' ||
    (SELECT count(*) FROM runtime_commit WHERE run_id='${CHECKPOINT_RUN}' AND thread_id='${CHECKPOINT_THREAD}'
      AND phase::jsonb=jsonb_build_object('Ended','NaturalEnd'))::text || '|' ||
    (SELECT count(*) FROM runtime_commit WHERE run_id='${CHECKPOINT_RUN}' AND phase::jsonb ? 'Ended')::text || '|' ||
    (SELECT count(DISTINCT run_id) FROM runtime_commit WHERE thread_id='${CHECKPOINT_THREAD}')::text")
[ "$CHECKPOINT_FINAL" = '0|0|1|1|1' ] || { err "Q4 original Run ended differently or remained pending: $CHECKPOINT_FINAL"; exit 1; }
CHECKPOINT_PROVIDER_CALLS=$(role_log_matches provider provider -F count "ADR71_PROVIDER_REQUEST \"${NATIVE_CHECKPOINT_MARKER}\"" "$PROVIDER_LOG_EVIDENCE")
[ "$CHECKPOINT_PROVIDER_CALLS" = 2 ] || { err "Q4 original two model steps were replayed/missing"; exit 1; }
[ "$(kubectl -n "$NS" get pod "$NATIVE_CHECKPOINT" -o jsonpath='{.metadata.uid}')" = "$CHECKPOINT_UID" ] \
  || { err "Q4 logical Worker Pod was replaced"; exit 1; }
python3 - "$CHECKPOINT_SESSION" "$CHECKPOINT_RECEIPT" "$CHECKPOINT_TOOL_USE" "$CHECKPOINT_RUN" "$CHECKPOINT_EPOCH" \
  "$CHECKPOINT_UID" "$CHECKPOINT_A_INCARNATION" "$CHECKPOINT_B_INCARNATION" "$CHECKPOINT_C_INCARNATION" "$CHECKPOINT_D_INCARNATION" \
  "$CHECKPOINT_SOURCE_UID" "$CHECKPOINT_SOURCE_NEW_UID" "$CHECKPOINT_SOURCE_CLAIM" "$CHECKPOINT_SOURCE_CLAIM_UID" "$CHECKPOINT_SOURCE_COMMIT" <<'PYAUDIT'
import json,sys
keys=('session','original_receipt','qualified_tool','run','original_epoch','worker_pod_uid',
      'A_incarnation','B_unregistered_incarnation','C_generation2_incarnation','D_generation3_incarnation',
      'original_source_pod_uid','new_source_pod_uid','original_retained_claim','original_retained_claim_uid','repository_commit')
print('CHECKPOINT_AUDIT '+json.dumps(dict(zip(keys,sys.argv[1:])),sort_keys=True))
PYAUDIT
ok "original occupied Awaiting Run survived same-UID Superseded/restart and completed a source-bearing Retained Rebuild"
log "NoSource409 is not manufactured in this fixed-Retained Container topology; its Host decision table is a separate required proof"

ok "K3D/K3S FULL E2E PASS: one public API covered configuration, resources, credentials, publication, deployment, real Worker execution and response; completed Pod-bound Native crash recovered and opaque in-flight crash retained safely without duplicate execution; concurrent load met the public latency/error gates."
