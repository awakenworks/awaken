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
# D4 default deny + explicit role graph -> nine NetworkPolicies.
# D5 K8s sandbox + admitted image + namespace-scoped ServiceAccount with Pod
# metadata update -> Worker creates isolated Session Pods and can CAS-transfer
# their reaper lease after restart, without privilege escalation or local fallback.
# D6 the existing distributed-Control composition adapter + explicit config path
# + the same Secret mounted there -> production Control assembly receives the
# deployment-owned model resolver without a parallel catalog or HOME config path.
# D7 distinct public/internal binds + Service ports + NetworkPolicy edges -> the
# gateway can reach only public 3000 while Control, Coordinator, and Worker use
# only private 3001; neither role silently collapses both authorities again.
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
grep -q -- '--server http://coordinator:3001' "$RENDERED"
[[ "$(grep -c 'name: internal' "$RENDERED")" -eq 4 ]]
[[ "$(grep -c 'port: 3001' "$RENDERED")" -ge 7 ]]
! grep -q '/usr/local/bin/awaken", "coordinator"' "$RENDERED"
! grep -q '/usr/local/bin/awaken", "database", "migrate"' "$RENDERED"
grep -q 'CREATE ROLE awaken_control LOGIN' "$RENDERED"
grep -q 'CREATE ROLE awaken_coordinator LOGIN' "$RENDERED"
! grep -q 'postgres://postgres:' "$RENDERED"
[[ "$(grep -c '^[[:space:]]*startupProbe:' "$RENDERED")" -eq 7 ]]
[[ "$(grep -c '^[[:space:]]*livenessProbe:' "$RENDERED")" -eq 7 ]]
[[ "$(grep -c '^        resources:' "$RENDERED")" -eq 9 ]]
[[ "$(grep -c '^kind: NetworkPolicy$' "$RENDERED")" -eq 9 ]]
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
grep -q 'name: worker-kube-api' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'crictl stop --timeout 0 "$WORKER_ZERO_CONTAINER"' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q "get endpoints kubernetes" "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
grep -q 'label pod postgres-standby-0 database-role=primary --overwrite' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
! grep -q 'patch service postgres' "$REPO_ROOT/e2e/k3d/distributed_control_e2e.sh"
[[ "$(grep -c -- '- /etc/awaken/control.toml' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c 'mountPath: /etc/awaken$' "$RENDERED")" -ge 4 ]]
! grep -q '/etc/awaken-home' "$RENDERED"

echo "OK - distributed role deployment enforces artifact, DB, health, resource, network and sandbox boundaries."
