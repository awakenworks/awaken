#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OVERLAY="$REPO_ROOT/deploy/k3d/product-backend"
RENDERED="$(mktemp)"
trap 'rm -f "$RENDERED"' EXIT

# Internal-product topology cause/effect decision table.
# Causes: C1 the product overlay renders; C2 the Awaken Service is ClusterIP and
# no external Kubernetes route exists; C3 state/config/tmp mounts, signed Worker
# trust projected from an externally provisioned Secret, and a bounded startup
# grace plus readiness/liveness probes are declared; C4 ingress is
# restricted to namespaces explicitly labelled as an
# Awaken Design backend; C5 K8s sandbox uses the imported non-`latest` base while
# BuildKit Jobs, the derived-image Registry, and namespace-scoped RBAC including
# Pod metadata update for resourceVersion-fenced reaper-lease transfer are
# coherent through the Registry container's internal :5000 endpoint (the host
# publishing port is not reachable inside the cluster); C6 the Kubernetes
# backend selects the canonical resident Pod channel and grants its ServiceAccount
# the namespace-scoped Pod port-forward subresource used by that channel, while
# retaining attached exec only for bounded file operations; C7 model/provider
# secret names or values appear in the manifest. Effects: E1 one
# all-in-one backend can start from the canonical image;
# E2 Awaken/Console and management APIs have no terminal-user ingress; E3 config
# plus Awaken-owned provider configuration, Skills, Agents, and Sessions survive
# Pod replacement; E4 only the product backend may reach port 8080; E5 K8s ACP
# startup uses the already-owned resident channel rather than depending on an
# apiserver exec-stream protocol, and may open that channel without broadening
# authority beyond the Session namespace; E6 reject the deployment contract before a
# cluster mutation. Rules: K1 C1+C2+C3+C4+C5+C6+!C7=>E1+E2+E3+E4+E5;
# K2 !C1|!C2|!C3|!C4|!C5|!C6|C7=>E6.
kubectl kustomize "$OVERLAY" >"$RENDERED"

[[ "$(grep -c '^kind: Deployment$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: Service$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: PersistentVolumeClaim$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: NetworkPolicy$' "$RENDERED")" -eq 3 ]]
grep -q 'name: awaken-product-ingress' "$RENDERED"
grep -q 'name: awaken-sandbox-default-deny' "$RENDERED"
grep -q 'name: awaken-sandbox-open-egress' "$RENDERED"
[[ "$(grep -c '^kind: ServiceAccount$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: Role$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: RoleBinding$' "$RENDERED")" -eq 1 ]]
grep -q 'type: ClusterIP' "$RENDERED"
! grep -Eq '^kind: (Ingress|Gateway)$|type: (NodePort|LoadBalancer)|hostPort:' "$RENDERED"
grep -q 'role = "all-in-one"' "$RENDERED"
grep -q 'no_browser = true' "$RENDERED"
grep -q 'identity_mode = "self-managed"' "$RENDERED"
grep -q 'worker_trust_credentials_file = "/etc/awaken-auth/worker-trust.json"' "$RENDERED"
grep -q 'sandbox_tier = "k8s"' "$RENDERED"
grep -q 'container_image = "awaken-sandbox:local"' "$RENDERED"
grep -q 'container_hand_residency = "resident"' "$RENDERED"
grep -q 'package_image_builder = "k8s"' "$RENDERED"
grep -q 'package_image_registry = "k3d-awaken-registry.localhost:5000/environments"' "$RENDERED"
grep -q 'k8s_buildkit_image = "k3d-awaken-registry.localhost:5000/system/buildkit:v0.30.0-rootless"' "$RENDERED"
! grep -q 'k3d-awaken-registry.localhost:5111' "$RENDERED"
grep -q 'package_registry_insecure = true' "$RENDERED"
grep -q 'serviceAccountName: awaken-product' "$RENDERED"
grep -q -- '- jobs' "$RENDERED"
grep -q -- '- pods/exec' "$RENDERED"
grep -q -- '- pods/portforward' "$RENDERED"
PRODUCT_OBJECT_RULE="$(sed -n '14,34p' "$RENDERED")"
grep -q -- '- create' <<<"$PRODUCT_OBJECT_RULE"
grep -q -- '- update' <<<"$PRODUCT_OBJECT_RULE"
grep -q -- '- delete' <<<"$PRODUCT_OBJECT_RULE"
grep -q 'persistentVolumeClaim:' "$RENDERED"
grep -q 'secretName: awaken-product-worker-auth' "$RENDERED"
grep -q 'mountPath: /etc/awaken-auth' "$RENDERED"
[[ "$(grep -c '^kind: Secret$' "$RENDERED")" -eq 0 ]]
grep -q 'startupProbe:' "$RENDERED"
grep -q 'failureThreshold: 180' "$RENDERED"
grep -q 'path: /readyz' "$RENDERED"
grep -q 'awaken.design/backend-access: "true"' "$RENDERED"
! grep -Eqi 'deepseek_api_key|provider-secret|api[_-]?key:|secret:' "$RENDERED"

echo "OK - internal Awaken product topology is private, durable, and secret-free."
