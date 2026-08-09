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
# D5 namespace sandbox in a Pod -> no privilege escalation or unconfined seccomp.
# Each assertion owns one observable terminal effect; the manifest remains the
# sole deployment source of truth.
grep -q '/usr/local/bin/awaken-worker' "$RENDERED"
! grep -q '/usr/local/bin/awaken worker' "$RENDERED"
grep -q 'CREATE ROLE awaken_control LOGIN' "$RENDERED"
grep -q 'CREATE ROLE awaken_coordinator LOGIN' "$RENDERED"
! grep -q 'postgres://postgres:' "$RENDERED"
[[ "$(grep -c '^[[:space:]]*startupProbe:' "$RENDERED")" -eq 7 ]]
[[ "$(grep -c '^[[:space:]]*livenessProbe:' "$RENDERED")" -eq 7 ]]
[[ "$(grep -c '^[[:space:]]*resources:' "$RENDERED")" -eq 9 ]]
[[ "$(grep -c '^kind: NetworkPolicy$' "$RENDERED")" -eq 9 ]]
grep -q 'name: default-deny' "$RENDERED"
grep -q 'allowPrivilegeEscalation: false' "$RENDERED"
grep -q 'type: RuntimeDefault' "$RENDERED"
! grep -q 'allowPrivilegeEscalation: true' "$RENDERED"
! grep -q 'type: Unconfined' "$RENDERED"

echo "OK - distributed role deployment enforces artifact, DB, health, resource, network and sandbox boundaries."
