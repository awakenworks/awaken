#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# Q1 Cargo and the canonical sandbox builder see current sources on every run;
# Q2 capacity admission includes every application image before cluster create.
# A tag-existence bypass can pass against old binaries, so no diagnostic reuse
# switch may bypass either build. Incremental caching stays Cargo/Docker-owned.
! grep -q 'ADR71_REUSE_IMAGE\|--ensure-hand' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq "import('@anthropic-ai/sdk')" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'npm ci --prefix e2e' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -Fq 'COORDINATOR_BIN=$(resolve_cargo_executable awaken-cli awaken-coordinator --features container-k8s)' \
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
RENDERED="$(mktemp)"
trap 'rm -f "$RENDERED"' EXIT
kubectl kustomize "$REPO_ROOT/deploy/k3d/distributed-control" >"$RENDERED"

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
# edge and E2 cannot bypass it through a mounted plaintext route. K2 signed transport admission
# remains owned by WorkerUpstream; the manifest only supplies deployment facts.
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
# namespace Pod get/delete; C2 the test Pod is non-restarting, retained and
# projected from one ServiceAccount token/Downward API identity; C3 the TLS edge
# blocks only its predecessor finish ACK; C4 the live script hard-stops its CRI
# container and never force-deletes the evidence object. E1 C1+C2+C3+C4 drives
# the existing ThreadCommit -> stop receipt -> Dispatch finish chain without a
# second recovery controller or replaying the public Event.
# Each assertion owns one observable terminal effect; the manifest remains the
# sole deployment source of truth.
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
grep -q -- '--server https://coordinator:3443' "$RENDERED"
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
grep -Fq 'k3d_render_sandbox_image "$DEPLOY_DIR/distributed-control" "$SANDBOX_IMAGE"' \
  "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'serviceAccountName: worker-runtime' "$RENDERED"
grep -q 'name: worker-runtime' "$RENDERED"
grep -q 'serviceAccountName: coordinator-runtime' "$RENDERED"
grep -q 'name: adr71-coordinator-token-review' "$RENDERED"
COORDINATOR_PROCESS_ROLE="$(sed -n '/metadata: { name: coordinator-worker-process }/,/^---$/p' \
  "$REPO_ROOT/deploy/k3d/distributed-control/resources.yaml" | head -12)"
grep -q 'resources: \["pods"\]' <<<"$COORDINATOR_PROCESS_ROLE"
grep -q 'verbs: \["get", "delete"\]' <<<"$COORDINATOR_PROCESS_ROLE"
! grep -Eq '"(create|list|watch|update|patch)"' <<<"$COORDINATOR_PROCESS_ROLE"
# Continuation-RBAC table: a retained Session needs C1 PVC create, C2 exact UID
# observation, and C3 terminal delete. E1 all three render in one PVC-only rule;
# E2 update/list/watch remain absent from that rule. KRB1=C1+C2+C3=>E1+E2.
WORKER_PVC_RULE="$(sed -n '/^kind: Role$/,/^---$/p' "$RENDERED" \
  | grep -A5 -- '- persistentvolumeclaims')"
grep -q -- '- create' <<<"$WORKER_PVC_RULE"
grep -q -- '- get' <<<"$WORKER_PVC_RULE"
grep -q -- '- delete' <<<"$WORKER_PVC_RULE"
! grep -Eq -- '- (update|list|watch)' <<<"$WORKER_PVC_RULE"
grep -q 'path: /admin/drain' "$RENDERED"
grep -q 'terminationGracePeriodSeconds: 20' "$RENDERED"
# Cause/effect rule: adopted Pod owner transfer requires update in addition to
# create/delete. Select the named Worker Role and its first object rule; document
# order may change when another least-privilege Role is added.
WORKER_ROLE="$(awk 'BEGIN { RS="---" } /kind: Role/ && /metadata: \{ name: worker-runtime \}/ { print }' \
  "$REPO_ROOT/deploy/k3d/distributed-control/resources.yaml")"
WORKER_OBJECT_RULE="$(sed -n '/resources: \["pods", "configmaps"\]/,/^  - apiGroups:/p' \
  <<<"$WORKER_ROLE")"
grep -q '"create"' <<<"$WORKER_OBJECT_RULE"
grep -q '"update"' <<<"$WORKER_OBJECT_RULE"
grep -q '"delete"' <<<"$WORKER_OBJECT_RULE"
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
grep -q 'crictl stop --timeout 0 "$PRIMARY_CONTAINER"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "sync_state = 'sync'" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
# Crash-semantics ownership rule: the ordinary opaque in-flight request remains
# excluded. The new case starts only after one terminal model record and an
# intentionally rejected finish ACK prove the complete ThreadCommit boundary.
! grep -q 'ADR71-CHAOS-SLOW' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'NATIVE_CRASH_MARKER="ADR71-NATIVE-HARD-CRASH"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'worker_kubernetes_identity_directory' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'restartPolicy: Never' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'awaken.dev/worker-process-evidence' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'prepare-native-crash' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'verify-native-crash' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'NATIVE_PROVIDER_BEFORE_CRASH' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'coordinator_worker_log_count' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -q 'logs -l app=coordinator -c worker-tls' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'observe_expired_predecessor' "$REPO_ROOT/crates/server/awaken-dispatch-http/src/worker_dispatch/session_coordination.rs"
grep -q 'crictl stop --timeout 0 "$NATIVE_OLD_CONTAINER"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'reject_old_native_finish' "$RENDERED"
grep -q 'ADR71_PROVIDER_REQUEST' "$REPO_ROOT/crates/devtools/awaken-scenario-host/src/distributed_control.rs"
# Stage-boundary capacity table: C1 the fault batch owns temporary retained
# Session Pods/PVCs; C2 later node/database checks retain only the original
# Session; C3 public disposal is followed by exact backend deletion joins.
# E1 C1+C2+C3 removes the dead fixture load before Stage 8, while missing either
# terminal intent or one physical join rejects the static scenario contract.
# The live lane establishes that these commands actually converge.
STAGE7="$(sed -n '/log "7\/11/,/log "8\/11/p' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh")"
grep -q 'dispose-batch' <<<"$STAGE7"
grep -Fq 'wait --for=delete "${BATCH_AUXILIARY_POD_TARGETS[@]}"' <<<"$STAGE7"
grep -Fq 'wait --for=delete "${BATCH_AUXILIARY_CLAIM_TARGETS[@]}"' <<<"$STAGE7"
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
