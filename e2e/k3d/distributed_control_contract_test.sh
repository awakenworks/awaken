#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
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
# D5 K8s sandbox + admitted image + namespace-scoped ServiceAccount with Pod
# metadata update -> Worker creates isolated Session Pods and can CAS-transfer
# their reaper lease after restart, without privilege escalation or local fallback.
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
# live scenario must stop the primary CRI container before promotion. Omitting
# either condition turns the test back into a planned asynchronous switchover
# which cannot establish the advertised recovery point.
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
grep -q 'container_image = "awaken-sandbox:local"' "$RENDERED"
grep -q 'serviceAccountName: worker-runtime' "$RENDERED"
grep -q 'name: worker-runtime' "$RENDERED"
grep -q 'path: /admin/drain' "$RENDERED"
grep -q 'terminationGracePeriodSeconds: 20' "$RENDERED"
# Cause/effect rule: adopted Pod owner transfer requires update in addition to
# create/delete. Kustomize renders flow-style source verbs as a block list, so
# assert each capability inside the first Worker Role rule instead of coupling
# the contract to YAML presentation.
WORKER_OBJECT_RULE="$(sed -n '8,25p' "$RENDERED")"
grep -q -- '- create' <<<"$WORKER_OBJECT_RULE"
grep -q -- '- update' <<<"$WORKER_OBJECT_RULE"
grep -q -- '- delete' <<<"$WORKER_OBJECT_RULE"
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
grep -q 'name: worker-kube-api' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'crictl stop --timeout 0 "$WORKER_ZERO_CONTAINER"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'synchronous_commit=remote_apply' "$RENDERED"
grep -q 'synchronous_standby_names=FIRST 1 (awaken_standby)' "$RENDERED"
grep -q 'application_name=awaken_standby' "$RENDERED"
grep -q 'connect_timeout=2' "$RENDERED"
grep -q 'crictl stop --timeout 0 "$PRIMARY_CONTAINER"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "sync_state = 'sync'" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "get endpoints kubernetes" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'label pod postgres-standby-0 database-role=primary --overwrite' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -q 'patch service postgres' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
[[ "$(grep -c -- '- /etc/awaken/control.toml' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c 'mountPath: /etc/awaken$' "$RENDERED")" -ge 4 ]]
! grep -q '/etc/awaken-home' "$RENDERED"

echo "OK - distributed role deployment enforces artifact, DB, health, resource, network and sandbox boundaries."
