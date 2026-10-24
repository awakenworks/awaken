#!/usr/bin/env bash
set -euo pipefail


# This runner takes no arguments. Accepting one silently is how a caller's
# intent disappears into a run that answers a different question, and here it
# is minutes of real cluster or server work; refuse before any of it.
if [ "$#" -ne 0 ]; then
  echo "usage: e2e/k3d/distributed_control_contract_test.sh" >&2
  exit 2
fi
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# Cluster selection cause/effect table: C1 absent/empty override => original
# identity; C2 valid explicit identity => isolated cluster and forwarder log;
# C3 malformed identity => exit 2 before any infrastructure or cleanup call.
# Evaluate the real initialization prefix, not a second selector implementation.
selected_cluster() (
  export AWAKEN_K3D_CLUSTER="$1"
  docker() { return 99; }
  k3d() { return 99; }
  kubectl() { return 99; }
  eval "$(sed '/^log() {/,$d' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  printf '%s|%s\n' "$CLUSTER" "$API_PF_LOG"
)
[[ "$(selected_cluster "")" = "awaken-adr71|${TMPDIR:-/tmp}/awaken-adr71_api_pf.log" ]]
[[ "$(selected_cluster awaken-adr71-retry)" = "awaken-adr71-retry|${TMPDIR:-/tmp}/awaken-adr71-retry_api_pf.log" ]]
for invalid in --all '../other' 'awaken bad'; do
  if selected_cluster "$invalid" >/dev/null 2>&1; then
    echo "invalid cluster was admitted: $invalid" >&2
    exit 1
  else
    [[ "$?" = 2 ]]
  fi
done
# Q1 Cargo and the canonical sandbox builder see current sources on every run;
# Q2 capacity admission includes every application image before cluster create.
# A tag-existence bypass can pass against old binaries, so no diagnostic reuse
# switch may bypass either build. Incremental caching stays Cargo/Docker-owned.
! grep -q 'ADR71_REUSE_IMAGE\|--ensure-hand' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq "import('@anthropic-ai/sdk')" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'npm ci --prefix e2e' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'COORDINATOR_BIN=$(resolve_cargo_executable awaken-coordinator-process awaken-coordinator --features container-k8s)' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'SANDBOX_BUILD_IMAGE="${ADR71_SANDBOX_BUILD_IMAGE:-awaken-sandbox:local}"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq '"$REPO_ROOT/deploy/images/sandbox/build.sh" --ensure-current "$SANDBOX_BUILD_IMAGE" ""' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'SANDBOX_IMAGE=$(k3d_image_digest "$SANDBOX_BUILD_IMAGE")' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'k3d_create_cluster "$CLUSTER" 3 "" "" --shared-volume "$CONTINUATION_VOLUME" "$IMAGE" "$SANDBOX_IMAGE" postgres:16 nginx:1.27-alpine' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'k3d_import_storage_images "$CLUSTER" "$IMAGE" "$SANDBOX_IMAGE" postgres:16 nginx:1.27-alpine' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
# D12 host build failure occurs before cluster creation and must not query an
# ambient kubectl context; after creation every diagnostic uses the harness's
# exact-context, bounded observer. The harness self-test owns deadline behavior.
grep -Fq 'CLUSTER_CREATED=0' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'CLUSTER_CREATED=1' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! sed -n '/^diagnostics() {/,/^}/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh" \
  | grep -Eq '^[[:space:]]*kubectl '
grep -Fq 'k3d_diagnostic_kubectl "$CLUSTER"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
# Context-isolation decision table: C1 another worktree changes kubectl's
# default context during this run; C2 the exact cluster has its own kubeconfig;
# C3 normal cleanup or explicit failed-cluster retention. R1=C1+C2 keeps every
# unqualified fault-injection command on this cluster; R2=C2+C3 removes only
# this run's config after cluster deletion; R3=C2+retention keeps both for
# diagnosis. The live eleven-stage run proves the command behavior; this
# contract rejects a return to shared-context orchestration before that run.
grep -Fq 'KUBECONFIG_FILE=$(mktemp)' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'k3d_use_private_kubeconfig "$KUBECONFIG_FILE"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'k3d_bind_private_kubeconfig "$CLUSTER"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq '"$KUBECONFIG_FILE" || return "$?"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
RENDERED="$(mktemp)"
CONTROL_RENDERED="$(mktemp)"
trap 'rm -f "$RENDERED" "$CONTROL_RENDERED"' EXIT
kubectl kustomize "$REPO_ROOT/deploy/k3d/distributed-control" >"$RENDERED"
kubectl kustomize "$REPO_ROOT/deploy/k3d/distributed-control/create-control" >"$CONTROL_RENDERED"

# Cause/effect decision table:
# D1 standalone Worker artifact -> no management-CLI Worker launch remains.
# D2 two bounded DB owners -> app URLs never use the bootstrap superuser.
# D3 every long-running workload -> startup, readiness, liveness and resources.
# D4 NetworkPolicy composition: C1 the overlay owns nine namespace/role policies;
# C2 it imports the two-policy canonical Sandbox base. E1 the rendered graph has
# exactly eleven policies; E2 each canonical Sandbox policy name occurs once.
# K1 selector/rule shape stays owned by test_image_contract.py and the Runtime
# live attestor; this test does not copy that authority. D4-D1 C1+C2 => E1+E2;
# a missing, extra, or duplicate rendered policy fails the contract.
# D5 K8s sandbox + the one built image's exact OCI digest + namespace-scoped
# ServiceAccount with Pod metadata update -> Worker creates isolated Session
# Pods and their trusted control forwarder from the same immutable artifact,
# and can CAS-transfer their reaper lease after restart without privilege
# escalation or local fallback. C1 the source contains one placeholder; C2 the
# live harness resolves one validated digest; C3 capacity, import and rendered
# Worker config consume that same value. E1 no mutable tag can pass this static
# contract; E2 a missing or duplicate placeholder fails the live deployment
# before any workload is applied.
# D6 the existing distributed-Control composition adapter + explicit config path
# + the same Secret mounted there -> production Control assembly receives the
# deployment-owned model resolver without a parallel catalog or HOME config path.
# D7 distinct public/internal binds + Service ports + NetworkPolicy edges -> the
# gateway can reach only public 3000, Control reaches Coordinator on private
# 3001, and remote Workers reach only the private-CA TLS sidecar on 3443. C1 an
# explicit DNS SAN/CA projection + C2 one TLS sidecar per Coordinator Pod whose
# leaf/key volume is not mounted into the Worker container + C3 Worker egress
# excludes direct 3001 -> E1 production Worker transport admits the exact HTTPS
# edge and E2 cannot bypass it through a mounted plaintext route. Pod token and
# UID admission remain owned by the existing Kubernetes Worker transport.
# D7-D1=C1+C2+C3=>E1+E2; missing TLS inputs, health/resources, or the exact edge
# fails this contract before the live distributed rule runs.
# D8 acknowledged-write RPO=0 requires one named remote_apply standby and the
# live scenario must stop the primary CRI container before promotion. D9 the
# settings must be persisted by the init script rather than passed to the
# temporary bootstrap server: otherwise POSTGRES_DB creation waits for a
# standby which cannot connect until bootstrap finishes. Rules D8=true proves
# the recovery point; D9=true proves bootstrap remains reachable.
# D10 Server roles must configure every role-owned store as PostgreSQL. If one
# is omitted, config resolution silently selects SQLite and creates a second
# local installation authority that an ephemeral migration Job cannot retain.
# C1 all six Control stores and both Coordinator stores have explicit URLs;
# C2 local platform-workspace-id projection is absent. D10+C1+C2 => migration,
# replicas, and recovery share only the PostgreSQL installation binding.
# D11 replicated Worker identity: C1 each Worker owns a distinct disposable
# emptyDir; C2 every Worker is configured for the same Kubernetes namespace;
# C3 the production selector treats that namespace, not local storage, as the
# stable realization scope. E1 every replica derives the same
# ContainerRealizationNamespace; E2 no unproduced platform-workspace-id can gate
# startup. D11-D1=C1+C2+C3=>E1+E2. The Kani harness owns C3 for all SandboxTier
# variants; this rendered contract owns absence of the redundant deployment path.
# D12 Pod-bound hard-crash recovery: C1 Coordinator owns TokenReview plus exact
# namespace Pod get/update/delete; C2 the admitted crash fixture is
# non-restarting and retained before admission;
# projected from one ServiceAccount token/Downward API identity; C3 the TLS edge
# blocks only its predecessor finish ACK; C4 the live script hard-stops its CRI
# container and never force-deletes the evidence object. E1 C1+C2+C3+C4 drives
# the existing ThreadCommit -> stop receipt -> Dispatch finish chain without a
# second recovery controller or replaying the public Event.
# D13 normal Worker retirement: C1 preStop closes claim admission before
# SIGTERM; C2 the configured drain is 15s; C3 the exact cancellation/return
# observer owns a further bounded 20s; C4 Kubernetes hard-kills at Pod grace.
# E1 the rendered 60s grace contains C2+C3 plus lifecycle publication margin;
# E2 shortening it to the former 20s value fails this deployment contract.
# Each assertion owns one observable terminal effect; the manifest remains the
# sole deployment source of truth.
# D14 brokered Create authority: C1 Worker receives one Pod-bound token/UID,
# never a signer; C2 Coordinator selects Kubernetes transport and its separate
# ConfigMap ledger Namespace before Ready; C3 both deployments import the one
# runtime read/dispose and six-Kind Create Role source; C4 only Coordinator is
# bound to Create while Worker keeps exact read/dispose; C5 the shared VAP read
# binding covers both runtime SAs. Effects: E1 first Sandbox POST has its
# broker route; E2 direct Worker Create/deletecollection stays denied; E3 no
# stage-late transport switch can create a second authority. Rules B1 all
# causes=>E1+E2+E3; B2 any missing=>reject before the live rollout.
grep -q '/usr/local/bin/awaken-worker' "$RENDERED"
! grep -q '/usr/local/bin/awaken worker' "$RENDERED"
for role in control coordinator; do
  role_commands="$(grep -A4 "/usr/local/bin/awaken-${role}$" "$RENDERED")"
  grep -q -- '- database' <<<"$role_commands"
  grep -q -- '- migrate' <<<"$role_commands"
  grep -q -- '- --config' <<<"$role_commands"
done
grep -q '/usr/local/bin/awaken-server' "$RENDERED"
grep -q 'name: AWAKEN_MODEL_MODE' "$RENDERED"
grep -q 'value: distributed-control' "$RENDERED"
grep -q 'name: AWAKEN_SCENARIO_CONFIG' "$RENDERED"
grep -q 'value: /etc/awaken/control.toml' "$RENDERED"
[[ "$(grep -c 'internal_bind = "0.0.0.0:3001"' "$RENDERED")" -eq 2 ]]
grep -q 'coordinator_internal_url = "http://coordinator:3001"' "$RENDERED"
grep -q 'control_internal_url = "http://control:3001"' "$RENDERED"
grep -q 'worker_server_ca_certificate_file = "/etc/awaken-worker-tls/ca.crt"' "$RENDERED"
grep -q 'https://coordinator:3443' "$RENDERED"
grep -q 'worker_kubernetes_identity_directory = "/var/run/awaken/identity"' "$RENDERED"
grep -q '^    \[worker_kubernetes\]$' "$RENDERED"
grep -q 'create_control_namespace = "awaken-create-control"' "$RENDERED"
! grep -q 'worker_request_credential_file' "$RENDERED"
! grep -q 'worker_trust_credentials_file' "$RENDERED"
! grep -q 'adr71-worker-auth' "$RENDERED"
grep -q 'audience: awaken-worker' "$RENDERED"
grep -q 'fieldPath: metadata.uid' "$RENDERED"
! grep -q -- '--server http://coordinator:3001' "$RENDERED"
grep -q 'proxy_pass http://127.0.0.1:3001;' "$RENDERED"
[[ "$(grep -c 'pid /tmp/nginx.pid;' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '_temp_path /tmp/' "$RENDERED")" -eq 5 ]]
grep -q '/etc/nginx-worker/nginx.conf' "$RENDERED"
[[ "$(grep -c 'image: nginx:1.27-alpine' "$RENDERED")" -eq 2 ]]
grep -q 'name: worker-tls' "$RENDERED"
grep -q 'secretName: adr71-worker-upstream-tls' "$RENDERED"
grep -q 'name: adr71-worker-upstream-ca' "$RENDERED"
[[ "$(grep -c 'key: tls.key' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c 'key: ca.crt' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c 'name: internal' "$RENDERED")" -eq 4 ]]
[[ "$(grep -c 'port: 3001' "$RENDERED")" -eq 6 ]]
CONTROL_SERVICE="$(sed -n '/metadata: { name: control, labels: { app: control } }/,/^---$/p' "$REPO_ROOT/deploy/k3d/distributed-control/resources.yaml")"
COORDINATOR_SERVICE="$(sed -n '/metadata: { name: coordinator, labels: { app: coordinator } }/,/^---$/p' "$REPO_ROOT/deploy/k3d/distributed-control/resources.yaml")"
! grep -q 'worker-tls' <<<"$CONTROL_SERVICE"
grep -q 'name: worker-tls, port: 3443, targetPort: 3443' <<<"$COORDINATOR_SERVICE"
! grep -q '/usr/local/bin/awaken", "coordinator"' "$RENDERED"
! grep -q '/usr/local/bin/awaken", "database", "migrate"' "$RENDERED"
grep -q 'CREATE ROLE awaken_control LOGIN' "$RENDERED"
grep -q 'CREATE ROLE awaken_coordinator LOGIN' "$RENDERED"
! grep -q 'postgres://postgres:' "$RENDERED"
[[ "$(grep -c 'data_subject_db = "postgres://awaken_control:' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c 'captured_content_db = "postgres://awaken_coordinator:' "$RENDERED")" -eq 1 ]]
! grep -q 'platform-workspace-id' "$RENDERED"
! grep -q 'name: adr71-installation' "$RENDERED"
[[ "$(grep -c '^[[:space:]]*startupProbe:' "$RENDERED")" -eq 8 ]]
[[ "$(grep -c '^[[:space:]]*readinessProbe:' "$RENDERED")" -eq 8 ]]
[[ "$(grep -c '^[[:space:]]*livenessProbe:' "$RENDERED")" -eq 8 ]]
[[ "$(grep -c '^        resources:' "$RENDERED")" -eq 10 ]]
[[ "$(grep -c '^kind: NetworkPolicy$' "$RENDERED")" -eq 11 ]]
[[ "$(grep -c '^  name: awaken-sandbox-default-deny$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^  name: awaken-sandbox-open-egress$' "$RENDERED")" -eq 1 ]]
grep -q 'name: default-deny' "$RENDERED"
grep -q 'allowPrivilegeEscalation: false' "$RENDERED"
grep -q 'type: RuntimeDefault' "$RENDERED"
! grep -q 'allowPrivilegeEscalation: true' "$RENDERED"
! grep -q 'type: Unconfined' "$RENDERED"
grep -q 'sandbox_tier = "k8s"' "$RENDERED"
grep -q 'k8s_namespace = "awaken-adr71"' "$RENDERED"
! grep -q 'k8s_memoryd_image' "$RENDERED"
[[ "$(grep -o '__AWAKEN_SANDBOX_IMAGE__' "$RENDERED" | wc -l)" -eq 1 ]]
! grep -q 'container_image = "awaken-sandbox:local"' "$RENDERED"
# D5-R2 the runtime overlay imports the canonical deployment and substitutes
# only this cluster's role tag before the SAME digest renderer. A direct-base
# spelling check would reject valid isolated images and miss the new call path.
# The live lane verifies rendered bytes/UID and the harness owns digest rejection.
grep -Fq 'resources: [../distributed-control]' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq '    newTag: $CLUSTER' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'k3d_render_sandbox_image "$ROLE_OVERLAY" "$SANDBOX_IMAGE"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'serviceAccountName: worker-runtime' "$RENDERED"
grep -q 'name: worker-runtime' "$RENDERED"
grep -q 'serviceAccountName: coordinator-runtime' "$RENDERED"
grep -q 'name: adr71-coordinator-token-review' "$RENDERED"
grep -Fq -- '- ../bases/sandbox-runtime-rbac' \
  "$REPO_ROOT/deploy/k3d/distributed-control/kustomization.yaml"
grep -Fq -- '- ../bases/sandbox-runtime-rbac' \
  "$REPO_ROOT/deploy/k3d/product-backend/kustomization.yaml"
grep -Fq -- '- ../../product-backend/create-control' \
  "$REPO_ROOT/deploy/k3d/distributed-control/create-control/kustomization.yaml"
grep -Fq 'kubectl apply -k "$DEPLOY_DIR/distributed-control/create-control"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -Fq -- '- ../product-backend/create-control' \
  "$REPO_ROOT/deploy/k3d/distributed-control/kustomization.yaml"
# D14 namespace decision: C6 the control overlay has its own explicit
# Namespace and is applied without `-n`; C7 only after it succeeds is the
# runtime overlay applied with `-n awaken-adr71`. C6+C7 => broker ledger Role
# exists before the first WorkUnit; mixing both streams under one `-n` must
# fail this contract instead of relying on a ready but incomplete Coordinator.
grep -q '^  name: awaken-create-control$' "$CONTROL_RENDERED"
grep -q '^  namespace: awaken-create-control$' "$CONTROL_RENDERED"
! grep -q '^  namespace: awaken-create-control$' "$RENDERED"
CONTROL_APPLY_LINE=$(grep -nF 'kubectl apply -k "$DEPLOY_DIR/distributed-control/create-control"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh" | cut -d: -f1)
RUNTIME_APPLY_LINE=$(grep -nF 'k3d_render_sandbox_image "$ROLE_OVERLAY" "$SANDBOX_IMAGE"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh" | cut -d: -f1)
[[ "$CONTROL_APPLY_LINE" -lt "$RUNTIME_APPLY_LINE" ]]
! grep -q 'kind: Role$' "$REPO_ROOT/deploy/k3d/distributed-control/resources.yaml"
for binding in coordinator-runtime-read-dispose worker-runtime; do
  BINDING="$(awk -v expected="$binding" 'BEGIN { RS="---" }
    /kind: RoleBinding/ && $0 ~ "name: " expected { print }' "$RENDERED")"
  grep -q 'name: awaken-runtime-read-dispose' <<<"$BINDING"
done
CREATE_BINDING="$(awk 'BEGIN { RS="---" }
  /kind: RoleBinding/ && /name: coordinator-create-executor/ { print }' "$RENDERED")"
grep -q 'name: awaken-create-executor' <<<"$CREATE_BINDING"
grep -q 'name: coordinator-runtime' <<<"$CREATE_BINDING"
! grep -q 'name: worker-runtime' <<<"$CREATE_BINDING"
LEDGER_BINDING="$(awk 'BEGIN { RS="---" }
  /kind: RoleBinding/ && /name: awaken-create-ledger/ { print }' "$CONTROL_RENDERED")"
grep -q 'namespace: awaken-create-control' <<<"$LEDGER_BINDING"
grep -q 'name: coordinator-runtime' <<<"$LEDGER_BINDING"
! grep -q 'name: worker-runtime' <<<"$LEDGER_BINDING"
POLICY_BINDING="$(awk 'BEGIN { RS="---" }
  /kind: ClusterRoleBinding/ && /name: awaken-create-isolation-read/ { print }' "$CONTROL_RENDERED")"
grep -q 'name: coordinator-runtime' <<<"$POLICY_BINDING"
grep -q 'name: worker-runtime' <<<"$POLICY_BINDING"
grep -q 'namespace: awaken-adr71' <<<"$POLICY_BINDING"
grep -q 'http://127.0.0.1:9090/admin/drain' "$RENDERED"
grep -q 'method="POST"' "$RENDERED"
grep -q 'terminationGracePeriodSeconds: 60' "$RENDERED"
! grep -q 'terminationGracePeriodSeconds: 20' "$RENDERED"
# The product contract owns the exact shared Role verbs. The live distributed
# gate checks six denied Worker Creates and six allowed Coordinator Creates.
! grep -q 'sandbox_allow_local_fallback = true' "$RENDERED"
! grep -q 'cidr: 0.0.0.0/0' "$RENDERED"
COORDINATOR_NETWORK_POLICY="$(sed -n '/metadata: { name: coordinator }/,/^---$/p' "$REPO_ROOT/deploy/k3d/distributed-control/network-policies.yaml")"
WORKER_NETWORK_POLICY="$(sed -n '/metadata: { name: worker }/,/^---$/p' "$REPO_ROOT/deploy/k3d/distributed-control/network-policies.yaml")"
grep -q 'port: 3001' <<<"$COORDINATOR_NETWORK_POLICY"
grep -q 'port: 3443' <<<"$COORDINATOR_NETWORK_POLICY"
grep -q 'port: 3443' <<<"$WORKER_NETWORK_POLICY"
! grep -q 'port: 3001' <<<"$WORKER_NETWORK_POLICY"
grep -q 'tls_identity_fixture.mjs' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'create configmap adr71-worker-upstream-ca' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'create secret tls adr71-worker-upstream-tls' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'name: runtime-kube-api' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'real_pod_identity_survives_reopen_and_complete_evidence_retires_exact_attempt' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "ALTER SYSTEM SET synchronous_commit = 'remote_apply';" "$RENDERED"
grep -q "ALTER SYSTEM SET synchronous_standby_names = 'FIRST 1 (awaken_standby)';" "$RENDERED"
! grep -q -- '- synchronous_commit=remote_apply' "$RENDERED"
! grep -q -- '- synchronous_standby_names=FIRST 1 (awaken_standby)' "$RENDERED"
grep -q 'application_name=awaken_standby' "$RENDERED"
grep -q 'connect_timeout=2' "$RENDERED"
# H1 primary-DB crash and H2 native-Worker crash both call the canonical
# UID/CRI-fenced stop helper; H3 an inline stop would reintroduce a parallel
# physical-control implementation. The harness self-test owns its effect table.
grep -Fq 'k3d_hard_stop_cri_container "$CLUSTER" "$PRIMARY_NODE" "$PRIMARY_CONTAINER"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "sync_state = 'sync'" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
# Positive recovery starts only after one terminal model record and a rejected
# finish ACK. The negative in-flight case keeps the original journal/slot even
# after its independent mock Provider completes, and uses no fabricated receipt.
! grep -q 'ADR71-CHAOS-SLOW' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'NATIVE_CRASH_MARKER="ADR71-NATIVE-HARD-CRASH"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'worker_kubernetes_identity_directory' "$RENDERED"
grep -q 'restartPolicy: Never' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'awaken.dev/worker-process-evidence' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'wait --for=delete "pod/$NATIVE_CRASH_OLD"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'prepare-native-crash' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'verify-native-crash' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'NATIVE_PROVIDER_BEFORE_CRASH' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'coordinator_worker_log_count' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -q 'logs -l app=coordinator -c worker-tls' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'observe_expired_predecessor' "$REPO_ROOT/crates/server/awaken-dispatch-http/src/worker_dispatch/session_coordination.rs"
grep -Fq 'hard_crash_native_worker "$NATIVE_CRASH_OLD" "$NATIVE_OLD_UID"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'hard_crash_native_worker "$NATIVE_CRASH_NEXT" "$NATIVE_INFLIGHT_UID"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
[[ "$(grep -Fc 'k3d_hard_stop_cri_container "$CLUSTER" "$node" "$container" || return "$?"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")" -eq 1 ]]
! grep -Fq 'crictl stop --timeout' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'NATIVE_INFLIGHT_RETAINED' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq '[ "$NATIVE_INFLIGHT_JOURNAL" = "1|0" ]' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'observe-provider-completed' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'verify-native-inflight' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'reject_old_native_finish' "$RENDERED"
grep -q 'ADR71_PROVIDER_REQUEST' "$REPO_ROOT/crates/devtools/awaken-scenario-host/src/distributed_control.rs"
# Stage-boundary capacity table: C1 the fault batch owns temporary retained
# Session Pods/PVCs; C2 later node/database checks retain only the original
# Session; C3 public disposal is followed by exact backend deletion joins.
# E1 C1+C2+C3 removes the dead fixture load before Stage 8, while missing either
# terminal intent or one physical join rejects the static scenario contract.
# Both joins must consume one absolute 120-second budget, not sequential full
# budgets that could admit nearly 240 seconds; the runner self-test owns the
# deadline arithmetic and this contract owns the exact Stage 7 call sites.
# The live lane establishes that these commands actually converge.
STAGE7="$(sed -n '/log "7\/11/,/log "8\/11/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
grep -q 'dispose-batch' <<<"$STAGE7"
grep -Fq 'BATCH_CLEANUP_DEADLINE=$((SECONDS + BATCH_CLEANUP_TIMEOUT_SECONDS))' <<<"$STAGE7"
grep -Fq 'batch_cleanup_wait_until "$BATCH_CLEANUP_DEADLINE" "${BATCH_AUXILIARY_POD_TARGETS[@]}"' <<<"$STAGE7"
grep -Fq 'batch_cleanup_wait_until "$BATCH_CLEANUP_DEADLINE" "${BATCH_AUXILIARY_CLAIM_TARGETS[@]}"' <<<"$STAGE7"
# D15 cause/effect: a hard node stop gives Node Unknown and may give every
# original Sandbox container only synthetic Unknown/255; retaining the original
# UID/PVC/guard is required and force deletion cannot qualify an exit. E1 the
# stage reads old truth without a new Event, E2 it admits only a new independent
# Session for the later DB fault, E3 neither Worker nor Sandbox Pod is forced
# away from its evidence finalizer. Stage 6 separately requires both ordinals
# to change UID instead of silently restarting within one immutable ledger.
STAGE8="$(sed -n '/log "8\/11/,/log "9\/11/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
grep -Fq 'awaken.dev/create-result-guard' <<<"$STAGE8"
grep -Fq 'create-independent-session' <<<"$STAGE8"
grep -Fq 'read-durable' <<<"$STAGE8"
! grep -Fq -- '--force' <<<"$STAGE8"
grep -Fq 'worker-0 reused its immutable Pod UID' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'worker-1 reused its immutable Pod UID' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "get endpoints kubernetes" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
# Promotion decision table: P1 the old synchronous writer dies and its standby
# is promoted -> clear the inherited dead-peer requirement before publication;
# P2 the promoted Pod is read/write + has an empty synchronous peer set -> one
# stable Service endpoint may select it; P3 either predicate is false -> fail
# before any application request. This guards the F1/F3 rules attached to the
# executable failover scenario instead of introducing a second HA mechanism.
grep -q "ALTER SYSTEM SET synchronous_standby_names = '';" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "current_setting('synchronous_standby_names')" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'stable postgres Service did not select only the promoted writer' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'label pod postgres-standby-0 database-role=primary --overwrite' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -q 'patch service postgres' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
[[ "$(grep -c -- '- /etc/awaken/control.toml' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c 'mountPath: /etc/awaken$' "$RENDERED")" -ge 4 ]]
! grep -q '/etc/awaken-home' "$RENDERED"

echo "OK - distributed role deployment enforces artifact, DB, health, resource, network and sandbox boundaries."
