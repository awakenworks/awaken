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
# Awaken Design backend; C5 model/provider secret names or values appear in the
# manifest. Effects: E1 one all-in-one backend can start from the canonical image;
# E2 Awaken/Console and management APIs have no terminal-user ingress; E3 config
# plus Awaken-owned provider configuration, Skills, Agents, and Sessions survive
# Pod replacement; E4 only the product backend may reach port 8080; E5 reject
# the deployment contract before a
# cluster mutation. Rules: K1 C1+C2+C3+C4+!C5=>E1+E2+E3+E4;
# K2 !C1|!C2|!C3|!C4|C5=>E5.
kubectl kustomize "$OVERLAY" >"$RENDERED"

[[ "$(grep -c '^kind: Deployment$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: Service$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: PersistentVolumeClaim$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: NetworkPolicy$' "$RENDERED")" -eq 1 ]]
grep -q 'type: ClusterIP' "$RENDERED"
! grep -Eq '^kind: (Ingress|Gateway)$|type: (NodePort|LoadBalancer)|hostPort:' "$RENDERED"
grep -q 'role = "all-in-one"' "$RENDERED"
grep -q 'no_browser = true' "$RENDERED"
grep -q 'identity_mode = "self-managed"' "$RENDERED"
grep -q 'persistentVolumeClaim:' "$RENDERED"
grep -q 'path: /readyz' "$RENDERED"
grep -q 'awaken.design/backend-access: "true"' "$RENDERED"
! grep -Eqi 'deepseek_api_key|provider-secret|api[_-]?key:|secret:' "$RENDERED"

echo "OK - internal Awaken product topology is private, durable, and secret-free."
