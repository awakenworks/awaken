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
# Q3 the Control executable comes from its Worker-free process package;
# resolving the old CLI target would either fail or reopen the broad closure.
grep -Fq 'CONTROL_BIN=$(resolve_cargo_executable awaken-control-process awaken-control)' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
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
grep -Fq '"$KUBECONFIG_FILE" || cleanup_status=$?' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
# Cleanup composition C/E: T1 early build/creation refusal + no receipt =>
# original cleanup removes only caller-owned local artifacts, never a named
# foreign cluster; T2 restore failure precedes a later teardown failure;
# T3 successful restore + teardown failure => that raw failure. The actual
# function is evaluated below; shared ownership/IO rules run in harness selftest.
(
  source "$REPO_ROOT/e2e/k3d/harness.sh"
  eval "$(sed -n '/^cleanup() {/,/^}/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  owned_local=$(mktemp -d)
  trap 'rm -rf -- "$owned_local"' EXIT
  DEPLOY_DIR="$owned_local"; KUBECONFIG_FILE="$owned_local/private-config"
  touch "$owned_local/awaken-control" "$owned_local/awaken-coordinator" \
    "$owned_local/awaken-server" "$owned_local/awaken-worker" "$KUBECONFIG_FILE"
  CLUSTER=awaken-foreign; CAPACITY_SAMPLER=""; WORKER_TLS_DIR=""; ROLE_OVERLAY=""
  PARTITIONED_NODE=""; STOPPED_CONTAINER=""; API_PF=""; PROVIDER_PF=""; CONTINUATION_VOLUME=""
  K3D_CREATED_CLUSTER=""; mutations=""; restore_status=0
  restore_coordinator_observation() { return "$restore_status"; }
  log() { :; }
  docker() { mutations+=" docker"; return 99; }
  k3d() { mutations+=" k3d"; return 99; }
  cleanup 0
  [[ -z "$mutations" && ! -e "$KUBECONFIG_FILE" ]]
  teardown_status=27
  k3d_delete_cluster_and_remove() { return "$teardown_status"; }
  restore_status=43
  status=0; cleanup 0 || status=$?
  [[ "$status" = 43 ]]
  restore_status=0
  status=0; cleanup 0 || status=$?
  [[ "$status" = 27 ]]
)
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
# T1 preserves the exact current transport target and each live cause; T2
# removing its canonical source/floor/feature or restoring a second list/grep
# path cannot qualify it. Execution/skip/refusal semantics remain owned by the
# existing required executor's tests, not by another parser in this contract.
TRANSPORT_GATE=$(sed -n '/^TRANSPORT_TEST=/,/^kubectl -n "\$NS" scale statefulset\/worker/p' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")
grep -Fq 'source "$REPO_ROOT/scripts/ci/_rust_test_execution.sh"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'AWAKEN_K8S_E2E=1 AWAKEN_K3D_CLUSTER="$CLUSTER" AWAKEN_K8S_FIXTURE_IMAGE="$IMAGE"' <<<"$TRANSPORT_GATE"
grep -Fq 'AWAKEN_K8S_CONTROL_IMAGE="$SANDBOX_IMAGE"' <<<"$TRANSPORT_GATE"
grep -Fq 'run_required_rust_tests -p awaken-process-startup --features container-k8s --lib --locked' <<<"$TRANSPORT_GATE"
grep -Fq '"$TRANSPORT_TEST" --require-case="$TRANSPORT_TEST" -- --exact' <<<"$TRANSPORT_GATE"
! grep -Eq '^TRANSPORT_TESTS=' <<<"$TRANSPORT_GATE"
! grep -Fq 'grep -Fqx "$TRANSPORT_TEST: test"' <<<"$TRANSPORT_GATE"
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
# IP-template table: the actual Pod producer keeps Never by default, explicitly
# renders Always for in-place restart, and rejects other values before kubectl.
# Stub only IO; do not duplicate the template or its policy selection.
render_native_worker() (
  NS=awaken-adr71 IMAGE=contract-image
  kubectl() { cat >&3; }
  err() { echo "$*" >&2; }
  eval "$(sed -n '/^create_native_worker_pod() {/,/^}/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  create_native_worker_pod contract-worker "$@" 3>&1
)
grep -q '^  restartPolicy: Never$' <<<"$(render_native_worker)"
grep -q '^  restartPolicy: Always$' <<<"$(render_native_worker Always)"
if render_native_worker invalid >/dev/null 2>&1; then
  echo "invalid owned Native restart policy was admitted" >&2
  exit 1
else
  [[ "$?" = 2 ]]
fi
# IP3 a stale captured container is refused before the one CRI owner is called;
# no retry may silently resample the replacement. Reuse the actual helper body.
(
  CLUSTER=contract NS=awaken-adr71 calls=0
  err() { echo "$*" >&2; }
  kubectl() { printf 'uid|k3d-contract-agent-0|containerd://%064d|0' 0; }
  k3d_hard_stop_cri_container() { calls=$((calls + 1)); return 99; }
  eval "$(sed -n '/^hard_crash_native_worker() {/,/^}/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  result=0
  hard_crash_native_worker contract-worker uid in-place stale-container >/dev/null 2>&1 || result=$?
  [[ "$result" = 1 && "$calls" = 0 ]]
)
# Log-observation decision table, using the ACTUAL runner bodies and IO only:
# L1 two complete original processes + unchanged full prefixes -> exact total;
# L2 partial successful text followed by log/API37 is failure, not count1;
# L3 missing/replaced/restarted participant or changing Deployment -> Unknown;
# L4 replacement DURING the read is likewise refused before emitting success;
# L5 same-process log truncation/rotation cannot erase an observed first call;
# L6 all history survives a long fault window (no --since), so replay totals2;
# L7 only grep no-match is0; invalid expression is still an error;
# L8 Coordinator reads use this same full acquisition, but a fresh observation
# cohort per call because this test intentionally scales those observers later.
# These stubs prove evidence refusal/counting, never real Kubernetes recovery.
(
  fixture=$(mktemp -d)
  trap 'rm -rf -- "$fixture"' EXIT
  mkdir "$fixture/evidence"
  NS=exact K3D_DIAGNOSTIC_TIMEOUT_SECONDS=20
  log_status=0 get_status=0 changed_during_read=0
  run_with_deadline() { shift; "$@"; }
  python3 - "$fixture" <<'PYLOGFIXTURE'
from pathlib import Path
import json, sys
root = Path(sys.argv[1])
definition = {"kind":"Deployment", "metadata":{"uid":"deployment-uid", "generation":1},
              "spec":{"replicas":2}, "status":{"observedGeneration":1,"replicas":2,
              "updatedReplicas":2,"readyReplicas":2,"availableReplicas":2}}
pods = [{"kind":"Pod", "metadata":{"name":name,"uid":name+"-uid"}, "status":{
    "containerStatuses":[{"name":"provider","ready":True,"containerID":"containerd://"+name,
    "restartCount":0,"state":{"running":{"startedAt":"2000-01-01T00:00:00Z"}}}]}}
    for name in ("provider-a","provider-b")]
(root/"participants.json").write_text(json.dumps({"items":[definition,*pods]}))
(root/"original.json").write_bytes((root/"participants.json").read_bytes())
for name in ("provider-a","provider-b"):
    (root/(name+".log")).write_bytes(("[pod/"+name+"/provider] old startup\n").encode())
PYLOGFIXTURE
  kubectl() {
    printf '%s\n' "$*" >>"$fixture/calls"
    [[ "$*" != *--since* ]] || return 91
    if [[ " $* " = *' get deployment,pod '* ]]; then
      cat "$fixture/participants.json"
      return "$get_status"
    fi
    if [[ " $* " = *' logs '* ]]; then
      local previous="" pod="" argument
      for argument in "$@"; do
        [[ "$previous" != logs ]] || pod="$argument"
        previous="$argument"
      done
      cat "$fixture/$pod.log"
      if [[ "$changed_during_read" = 1 ]]; then
        python3 - "$fixture/participants.json" <<'PYLOGCHANGE'
import json, sys
from pathlib import Path
p=Path(sys.argv[1]); value=json.loads(p.read_text())
value["items"][1]["status"]["containerStatuses"][0]["containerID"]="foreign"
p.write_text(json.dumps(value))
PYLOGCHANGE
      fi
      return "$log_status"
    fi
    return 92
  }
  for owner in role_log_participants role_logs role_log_matches; do
    eval "$(sed -n "/^$owner() {/,/^}/p" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  done
  role_logs provider provider "$fixture/evidence" freeze >/dev/null
  printf '[pod/provider-a/provider] ADR71_PROVIDER_REQUEST "unique"\n' >>"$fixture/provider-a.log"
  [[ "$(role_log_matches provider provider -F count 'ADR71_PROVIDER_REQUEST "unique"' "$fixture/evidence")" = 1 ]]
  [[ "$(role_log_matches provider provider -F lines 'ADR71_PROVIDER_REQUEST "unique"' "$fixture/evidence")" = '[pod/provider-a/provider] ADR71_PROVIDER_REQUEST "unique"' ]]
  [[ "$(role_log_matches provider provider -F count missing "$fixture/evidence")" = 0 ]]
  result=0 log_status=37 output=""
  output=$(role_log_matches provider provider -F count 'ADR71_PROVIDER_REQUEST "unique"' "$fixture/evidence") || result=$?
  [[ "$result" = 37 && -z "$output" ]]
  result=0 log_status=0 get_status=31 output=""
  output=$(role_log_matches provider provider -F count unique "$fixture/evidence") || result=$?
  [[ "$result" = 31 && -z "$output" ]]
  get_status=0
  for cause in missing uid container restart started deployment replicas; do
    python3 - "$fixture" "$cause" <<'PYLOGCAUSE'
import json, sys
from pathlib import Path
root=Path(sys.argv[1]); value=json.loads((root/"original.json").read_text()); cause=sys.argv[2]
selected=value["items"][1]["status"]["containerStatuses"][0]
if cause=="missing": value["items"].pop()
if cause=="uid": value["items"][1]["metadata"]["uid"]="foreign"
if cause=="container": selected["containerID"]="foreign"
if cause=="restart": selected["restartCount"]=1
if cause=="started": selected["state"]["running"]["startedAt"]="2000-01-02T00:00:00Z"
if cause=="deployment": value["items"][0]["metadata"]["uid"]="foreign"
if cause=="replicas": value["items"][0]["spec"]["replicas"]=1
(root/"participants.json").write_text(json.dumps(value))
PYLOGCAUSE
    result=0 output=""
    output=$(role_log_matches provider provider -F count unique "$fixture/evidence") || result=$?
    [[ "$result" = 2 && -z "$output" ]]
  done
  cp "$fixture/original.json" "$fixture/participants.json"
  mv "$fixture/evidence/provider-a.log" "$fixture/retained.log"
  result=0 output=""
  output=$(role_log_matches provider provider -F count unique "$fixture/evidence") || result=$?
  [[ "$result" = 2 && -z "$output" ]]
  mv "$fixture/retained.log" "$fixture/evidence/provider-a.log"
  result=0 changed_during_read=1 output=""
  output=$(role_log_matches provider provider -F count unique "$fixture/evidence") || result=$?
  [[ "$result" = 2 && -z "$output" ]]
  changed_during_read=0
  cp "$fixture/original.json" "$fixture/participants.json"
  cp "$fixture/provider-a.log" "$fixture/saved.log"
  printf '[pod/provider-a/provider] ADR71_PROVIDER_REQUEST "unique"\n' >"$fixture/provider-a.log"
  result=0 output=""
  output=$(role_log_matches provider provider -F count unique "$fixture/evidence") || result=$?
  [[ "$result" = 2 && -z "$output" ]]
  cp "$fixture/saved.log" "$fixture/provider-a.log"
  printf '[pod/provider-b/provider] ADR71_PROVIDER_REQUEST "unique"\n' >>"$fixture/provider-b.log"
  [[ "$(role_log_matches provider provider -F count 'ADR71_PROVIDER_REQUEST "unique"' "$fixture/evidence")" = 2 ]]
  result=0 output=""
  output=$(role_log_matches provider provider -E count '[' "$fixture/evidence" 2>/dev/null) || result=$?
  [[ "$result" = 2 && -z "$output" ]]
  # L9 a completed count2 does NOT authorize a fresh baseline if its freeze
  # completion marker is lost. The same fixture keeps both counts but replaces
  # one participant UID/CID: read must remain Unknown, never bind that new set.
  cp "$fixture/evidence/participants" "$fixture/participants.saved"
  rm "$fixture/evidence/participants"
  python3 - "$fixture/participants.json" <<'PYLOGLOST'
import json, sys
from pathlib import Path
p=Path(sys.argv[1]); value=json.loads(p.read_text())
value["items"][1]["metadata"]["uid"]="replacement-uid"
value["items"][1]["status"]["containerStatuses"][0]["containerID"]="replacement-container"
p.write_text(json.dumps(value))
PYLOGLOST
  result=0 output=""
  output=$(role_log_matches provider provider -F count 'ADR71_PROVIDER_REQUEST "unique"' "$fixture/evidence") || result=$?
  printf 'lost-freeze-and-replacement actual_status=%s actual_count=%s expected_status=2\n' "$result" "$output"
  [[ "$result" = 2 && -z "$output" ]]
  cp "$fixture/participants.saved" "$fixture/evidence/participants"
  cp "$fixture/original.json" "$fixture/participants.json"
  # L10 read is never an initializer: empty/half-written/missing directories and
  # omitted Provider baseline refuse; an explicit second freeze cannot rewrite
  # a completed cohort. Only the original startup call may seal an empty path.
  mkdir "$fixture/uninitialized" "$fixture/partial"
  cp "$fixture/evidence/provider-a.log" "$fixture/partial/provider-a.log"
  for evidence in "$fixture/uninitialized" "$fixture/partial" "$fixture/missing" ""; do
    result=0 output=""
    output=$(role_log_matches provider provider -F count unique "$evidence") || result=$?
    [[ "$result" = 2 && -z "$output" ]]
  done
  result=0 output=""
  output=$(role_logs provider provider "$fixture/evidence" freeze) || result=$?
  [[ "$result" = 2 && -z "$output" ]]
  # The original sealed cohort remains usable after these refusals.
  [[ "$(role_log_matches provider provider -F count 'ADR71_PROVIDER_REQUEST "unique"' "$fixture/evidence")" = 2 ]]
  # Same acquisition for the other role; no Provider lifetime baseline is reused
  # after the deliberate Coordinator pause/restore changes its own participant set.
  python3 - "$fixture/participants.json" <<'PYLOGROLE'
import json, sys
from pathlib import Path
p=Path(sys.argv[1]); value=json.loads(p.read_text())
for pod in value["items"][1:]:
    pod["status"]["containerStatuses"][0]["name"]="worker-tls"
p.write_text(json.dumps(value))
PYLOGROLE
  [[ "$(role_log_matches coordinator worker-tls -E count 'ADR71_PROVIDER_REQUEST .*unique')" = 2 ]]
  ! grep -q -- --since "$fixture/calls"
)
# Q-read table: exact original UID+fixed commit -> one actual read command;
# foreign/missing UID -> zero exec; get failure -> raw status. This observes
# forwarding/refusal only, never stands in for real cluster clone/readback.
(
  fixture=$(mktemp -d)
  trap 'rm -f -- "$fixture/exec"; rmdir -- "$fixture"' EXIT
  NS=exact current_uid=owned get_status=0
  kubectl() {
    if [[ " $* " = *' exec '* ]]; then
      printf '%s\0' "$@" >"$fixture/exec"
    else
      (( get_status == 0 )) || return "$get_status"
      printf '%s' "$current_uid"
    fi
  }
  eval "$(sed -n '/^verify_checkpoint_repository() {/,/^}/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  printf -v expected_commit '%040d' 0
  verify_checkpoint_repository source-pod owned "$expected_commit"
  python3 - "$fixture/exec" "$expected_commit" <<'PYREAD'
import pathlib,sys
args=pathlib.Path(sys.argv[1]).read_bytes().split(b'\0')[:-1]
assert args[:8] == [b'-n',b'exact',b'exec',b'source-pod',b'-c',b'agent',b'--',b'sh']
assert args[-2:] == [b'awaken-checkpoint-source',sys.argv[2].encode()]
assert b'git -C /workspace/source rev-parse HEAD' in args[-3]
assert b'cmp -s - /workspace/source/README' in args[-3]
PYREAD
  rm -f -- "$fixture/exec"
  current_uid=foreign
  ! verify_checkpoint_repository source-pod owned "$expected_commit"
  [[ ! -e "$fixture/exec" ]]
  ! verify_checkpoint_repository source-pod '' "$expected_commit"
  [[ ! -e "$fixture/exec" ]]
  current_uid=owned get_status=31 result=0
  verify_checkpoint_repository source-pod owned "$expected_commit" || result=$?
  [[ "$result" = 31 && ! -e "$fixture/exec" ]]
)
# CP-restore decision table: P1 owned UID/replicas -> suspend every observer,
# then restore that original count. P2 scale failure saves restoration BEFORE
# IO; cleanup can observe the original count and finish idempotently. P3 foreign
# UID refuses before mutation. P4 rollout failure preserves the restoration
# receipt for the existing cleanup owner. These are actual helper+IO-stub tests,
# not Registry/Session or successful live recovery claims.
(
  fixture=$(mktemp -d)
  trap 'rm -f -- "$fixture/uid" "$fixture/replicas" "$fixture/calls"; rmdir -- "$fixture"' EXIT
  printf original-uid >"$fixture/uid"
  printf 2 >"$fixture/replicas"
  NS=exact K3D_DIAGNOSTIC_TIMEOUT_SECONDS=20
  COORDINATOR_RESTORE_UID="" COORDINATOR_RESTORE_REPLICAS=""
  scale_status=0 rollout_status=0
  run_with_deadline() { shift; "$@"; }
  kubectl() {
    printf '%s\n' "$*" >>"$fixture/calls"
    if [[ " $* " = *' get deploy/coordinator '* ]]; then
      printf '%s|%s|original-rv' "$(cat "$fixture/uid")" "$(cat "$fixture/replicas")"
    elif [[ " $* " = *' scale deploy/coordinator '* ]]; then
      [[ "$*" = *'--resource-version=original-rv'* ]] || return 31
      (( scale_status == 0 )) || return "$scale_status"
      for argument in "$@"; do
        [[ "$argument" != --replicas=* ]] || printf '%s' "${argument#--replicas=}" >"$fixture/replicas"
      done
    elif [[ " $* " = *' rollout status '* ]]; then
      return "$rollout_status"
    elif [[ " $* " = *' wait --for=delete '* ]]; then
      [[ "$(cat "$fixture/replicas")" = 0 ]]
    else return 32
    fi
  }
  eval "$(sed -n '/^pause_coordinator_observation() {/,/^}/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  eval "$(sed -n '/^restore_coordinator_observation() {/,/^}/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
  pause_coordinator_observation
  [[ "$COORDINATOR_RESTORE_UID/$COORDINATOR_RESTORE_REPLICAS" = original-uid/2 ]]
  [[ "$(cat "$fixture/replicas")" = 0 ]]
  restore_coordinator_observation
  [[ "$(cat "$fixture/replicas")" = 2 && -z "$COORDINATOR_RESTORE_UID" ]]
  scale_status=43 result=0
  pause_coordinator_observation || result=$?
  [[ "$result" = 43 && "$COORDINATOR_RESTORE_UID" = original-uid ]]
  [[ "$(cat "$fixture/replicas")" = 2 ]]
  scale_status=0
  restore_coordinator_observation
  pause_coordinator_observation
  printf foreign-uid >"$fixture/uid"
  prior=$(wc -l <"$fixture/calls")
  result=0
  restore_coordinator_observation || result=$?
  [[ "$result" = 2 && "$(wc -l <"$fixture/calls")" = "$((prior + 1))" ]]
  [[ "$(cat "$fixture/replicas")" = 0 && -n "$COORDINATOR_RESTORE_UID" ]]
  printf original-uid >"$fixture/uid"
  rollout_status=44 result=0
  restore_coordinator_observation || result=$?
  [[ "$result" = 44 && -n "$COORDINATOR_RESTORE_UID" ]]
  [[ "$(cat "$fixture/replicas")" = 2 ]]
  rollout_status=0
  restore_coordinator_observation
  [[ -z "$COORDINATOR_RESTORE_UID" && -z "$COORDINATOR_RESTORE_REPLICAS" ]]
)
# The Coordinator adds the Worker process-evidence finalizer at registration;
# neither a fixture Pod nor the fleet template may carry it from creation.
! grep -q 'finalizers: \[awaken.dev/worker-process-evidence\]' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -q 'awaken.dev/worker-process-evidence' "$RENDERED"
grep -Fq 'wait --for=delete "pod/$NATIVE_CRASH_OLD"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'prepare-native-crash' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'verify-native-crash' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'NATIVE_PROVIDER_BEFORE_CRASH' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'role_log_matches coordinator worker-tls -E count' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -q 'logs -l app=coordinator -c worker-tls' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'observe_expired_predecessor' "$REPO_ROOT/crates/platform/dispatch/awaken-dispatch-http/src/worker_dispatch/session_coordination.rs"
grep -Fq 'hard_crash_native_worker "$NATIVE_CRASH_OLD" "$NATIVE_OLD_UID"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'hard_crash_native_worker "$NATIVE_CRASH_NEXT" "$NATIVE_INFLIGHT_UID"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
[[ "$(grep -Fc 'k3d_hard_stop_cri_container "$CLUSTER" "$node" "$container" || return "$?"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")" -eq 1 ]]
! grep -Fq 'crictl stop --timeout' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'NATIVE_INFLIGHT_RETAINED' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq '[ "$NATIVE_INFLIGHT_JOURNAL" = "1|0" ]' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'observe-provider-completed' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'verify-native-inflight' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'reject_old_native_finish' "$RENDERED"
# IP0-IP2 freeze the original command/commit and fault exactly that container.
# These composition obligations do not claim live PASS: the original distributed
# gate must execute the attached physical table and every existing negative.
STAGE11C="$(sed -n '/log "11c\/11/,/ok "K3D\/K3S FULL/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
grep -Fq 'create_native_worker_pod "$NATIVE_INPLACE" Always' <<<"$STAGE11C"
grep -Fq 'hard_crash_native_worker "$NATIVE_INPLACE" "$NATIVE_INPLACE_UID" in-place "$NATIVE_INPLACE_CONTAINER"' <<<"$STAGE11C"
grep -Fq '"$API_URL" "$NATIVE_INPLACE_SESSION" "$NATIVE_INPLACE_RECEIPT" "$NATIVE_INPLACE_MARKER"' <<<"$STAGE11C"
grep -Fq '[ "$NATIVE_INPLACE_FINAL" = "0|1|1|1|1" ]' <<<"$STAGE11C"
grep -Fq 'FROM runtime_commit' <<<"$STAGE11C"
grep -Fq 'FROM runtime_dispatch_operation' <<<"$STAGE11C"
grep -Fq 'generation > ${NATIVE_INPLACE_GENERATION}' <<<"$STAGE11C"
grep -Fq 'native-inplace:[^|]+' "$RENDERED"
grep -Fq '$http_x_awaken_worker_incarnation' "$RENDERED"
! grep -Fq 'events.send' <<<"$STAGE11C"
! grep -Fq 'verify-durable' <<<"$STAGE11C"
! grep -Fq 'delete pod' <<<"$STAGE11C"
grep -q 'ADR71_PROVIDER_REQUEST' "$REPO_ROOT/crates/devtools/awaken-scenario-host/src/distributed_control.rs"
# Q0-Q4 composition: the live case must freeze old committed custody and join
# real original Run/UID/ticket/source effects. These checks guard wiring only,
# never convert a static match into a physical or current-source PASS.
STAGE11D="$(sed -n '/log "11d\/11/,/ok "K3D\/K3S FULL/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
grep -Fq 'prepare-native-checkpoint' <<<"$STAGE11D"
grep -Fq 'observe-native-checkpoint' <<<"$STAGE11D"
grep -Fq 'resume-native-checkpoint' <<<"$STAGE11D"
grep -Fq 'FROM runtime_waiting' <<<"$STAGE11D"
grep -Fq '[ "$CHECKPOINT_CURRENT_TICKET" = "$CHECKPOINT_TICKET" ]' <<<"$STAGE11D"
grep -Fq 'process_exit_json::jsonb->>' <<<"$STAGE11D"
grep -Fq "='superseded'" <<<"$STAGE11D"
grep -Fq 'AND generation=2' <<<"$STAGE11D"
grep -Fq '[ "$CHECKPOINT_B_REGISTERED" = 0 ]' <<<"$STAGE11D"
grep -Fq 'int(restarts)==int(count)+2' <<<"$STAGE11D"
grep -Fq 'k3d_delete_exact_pod "$CLUSTER" "$NS" "$CHECKPOINT_SOURCE_POD" "$CHECKPOINT_SOURCE_UID" "$CHECKPOINT_SOURCE_RV"' <<<"$STAGE11D"
grep -Fq 'verify_checkpoint_repository "$CHECKPOINT_SOURCE_REPLACEMENT" "$CHECKPOINT_SOURCE_NEW_UID" "$CHECKPOINT_SOURCE_COMMIT"' <<<"$STAGE11D"
grep -Fq "'{aggregate,environment_materialization,origin,kind}'" <<<"$STAGE11D"
! grep -Eq '\b(INSERT|UPDATE|DELETE FROM)\b|--force|delete pvc|has_source[[:space:]]*=' <<<"$STAGE11D"
grep -Fq 'restore_coordinator_observation || coordinator_status=$?' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'k3d_delete_exact_pod_selftest' "$REPO_ROOT/e2e/k3d/harness.sh"
grep -Fq 'native-checkpoint:[^|]+' "$RENDERED"
grep -Fq 'waitForSessionCustomToolBoundary' "$REPO_ROOT/e2e/k3d/distributed_control_driver.ts"
grep -Eq "^const CHECKPOINT_COMMIT = '[0-9a-f]{40}';$" "$REPO_ROOT/e2e/k3d/distributed_control_driver.ts"

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
grep -Fq 'worker-0 was replaced instead of restarted in place' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'worker-1 was replaced instead of restarted in place' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
# The Worker ID is the Pod and an incarnation is one container instance
# (ADR-0078 amendment 2026-10-31): stage 6b hard-kills worker-0's container in
# place through the canonical CRI helper, then joins the same Pod UID, the new
# container's registration, the old incarnation's exact lastState exit and
# continued public work. The Worker fleet stays a restartable StatefulSet.
STAGE6B="$(sed -n '/log "6b\/11/,/log "7\/11/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
grep -Fq 'hard_crash_native_worker worker-0 "$CRASHED_WORKER_UID" in-place' <<<"$STAGE6B"
grep -Fq "incarnation_id = '\${RESTARTED_INCARNATION}'" <<<"$STAGE6B"
grep -Fq "process_exit_json LIKE '%exit_code%137%'" <<<"$STAGE6B"
grep -Fq 'verify-durable "$API_URL" "$DEPLOYMENT_ID" "$SESSION_ID" ADR71-AFTER-WORKER-CRASH' <<<"$STAGE6B"
! grep -Fq 'delete pod' <<<"$STAGE6B"
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
