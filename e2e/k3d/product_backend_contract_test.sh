#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OVERLAY="$REPO_ROOT/deploy/k3d/product-backend"
RENDERED="$(mktemp)"
trap 'rm -f "$RENDERED"' EXIT

# Internal-product topology cause/effect decision table.
# Causes: C1 the product overlay renders; C2 the Awaken Service is ClusterIP and
# no external Kubernetes route exists; C3 state/config/tmp mounts and probes are
# declared; C4 ingress is restricted to namespaces explicitly labelled as an
# Awaken Design backend; C5 K8s sandbox, BuildKit Job, shared Registry, and
# namespace-scoped RBAC are coherent; C6 model/provider secret names or values
# appear in the manifest. Effects: E1 one all-in-one backend can start from the canonical image;
# E2 Awaken/Console and management APIs have no terminal-user ingress; E3 config
# plus Awaken-owned provider configuration, Skills, Agents, and Sessions survive
# Pod replacement; E4 only the product backend may reach port 8080; E5 reject
# the deployment contract before a
# cluster mutation. Rules: K1 C1+C2+C3+C4+C5+!C6=>E1+E2+E3+E4;
# K2 !C1|!C2|!C3|!C4|!C5|C6=>E5.
kubectl kustomize "$OVERLAY" >"$RENDERED"

[[ "$(grep -c '^kind: Deployment$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: Service$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: PersistentVolumeClaim$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: NetworkPolicy$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: ServiceAccount$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: Role$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: RoleBinding$' "$RENDERED")" -eq 1 ]]
grep -q 'type: ClusterIP' "$RENDERED"
! grep -Eq '^kind: (Ingress|Gateway)$|type: (NodePort|LoadBalancer)|hostPort:' "$RENDERED"
grep -q 'role = "all-in-one"' "$RENDERED"
grep -q 'no_browser = true' "$RENDERED"
grep -q 'identity_mode = "self-managed"' "$RENDERED"
grep -q 'sandbox_tier = "k8s"' "$RENDERED"
grep -q 'package_image_builder = "k8s"' "$RENDERED"
grep -q 'package_image_registry = "k3d-awaken-registry.localhost:5111/environments"' "$RENDERED"
grep -q 'package_registry_insecure = true' "$RENDERED"
grep -q 'serviceAccountName: awaken-product' "$RENDERED"
grep -q -- '- jobs' "$RENDERED"
grep -q -- '- pods/exec' "$RENDERED"
grep -q 'persistentVolumeClaim:' "$RENDERED"
grep -q 'path: /readyz' "$RENDERED"
grep -q 'awaken.design/backend-access: "true"' "$RENDERED"
! grep -Eqi 'deepseek_api_key|provider-secret|api[_-]?key:|secret:' "$RENDERED"

echo "OK - internal Awaken product topology is private, durable, and secret-free."
