#!/usr/bin/env bash
set -euo pipefail


# This runner takes no arguments. Accepting one silently is how a caller's
# intent disappears into a run that answers a different question, and here it
# is minutes of real cluster or server work; refuse before any of it.
if [ "$#" -ne 0 ]; then
  echo "usage: e2e/k3d/product_backend_contract_test.sh" >&2
  exit 2
fi
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OVERLAY="$REPO_ROOT/deploy/k3d/product-backend"
source "$REPO_ROOT/e2e/k3d/harness.sh"
RENDERED="$(mktemp)"
trap 'rm -f "$RENDERED"' EXIT

# Internal-product topology cause/effect decision table.
# Causes: C1 the product overlay renders; C2 the Awaken Service is ClusterIP and
# no external Kubernetes route exists; C3 the restartable AllInOne owns no
# execution identity while one non-restarting Worker Job carries the Pod-bound
# identity/token projection, exact TokenReview authority, retained stop-evidence
# finalizer, TLS transport, and bounded probes; C4 ingress is
# restricted to namespaces explicitly labelled as an
# Awaken Design backend; C5 K8s sandbox uses the imported non-`latest` base and
# the canonical retained continuation PVC, with exact create/get/delete RBAC,
# while
# BuildKit Jobs, the derived-image Registry, and namespace-scoped RBAC including
# Pod metadata update for resourceVersion-fenced reaper-lease transfer are
# coherent through the Registry container's internal :5000 endpoint (the host
# publishing port is not reachable inside the cluster), while the Sandbox base
# is bound once to its validated OCI digest before apply; C6 the Kubernetes
# backend selects the canonical resident Pod channel and grants its ServiceAccount
# the namespace-scoped Pod port-forward subresource used by that channel, while
# retaining attached exec only for bounded file operations; C7 model/provider
# secret names or values appear in the manifest. Effects: E1 one
# all-in-one backend can start from the canonical image;
# E2 Awaken/Console and management APIs have no terminal-user ingress; E3 config,
# Awaken-owned provider configuration, Skills, Agents, Sessions, and each active
# Session workspace survive their owning Pod replacement; E4 only the product
# backend may reach port 8080; E5 K8s ACP
# startup uses the already-owned resident channel rather than depending on an
# apiserver exec-stream protocol, and may open that channel without broadening
# authority beyond the Session namespace; E6 reject the deployment contract before a
# cluster mutation. Rules: K1 C1+C2+C3+C4+C5+C6+!C7=>E1+E2+E3+E4+E5;
# K2 !C1|!C2|!C3|!C4|!C5|!C6|C7=>E6.
SANDBOX_IMAGE="registry.test/awaken-sandbox@sha256:$(printf 'c%.0s' {1..64})"
[[ "$(grep -o '__AWAKEN_SANDBOX_IMAGE__' "$OVERLAY/resources.yaml" | wc -l)" -eq 2 ]]
! grep -q 'container_image = "awaken-sandbox:local"' "$OVERLAY/resources.yaml"
k3d_render_sandbox_image "$OVERLAY" "$SANDBOX_IMAGE" 2 >"$RENDERED"

[[ "$(grep -c '^kind: Deployment$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: Job$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: Service$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: PersistentVolumeClaim$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: NetworkPolicy$' "$RENDERED")" -eq 3 ]]
grep -q 'name: awaken-product-ingress' "$RENDERED"
grep -q 'name: awaken-sandbox-default-deny' "$RENDERED"
grep -q 'name: awaken-sandbox-open-egress' "$RENDERED"
[[ "$(grep -c '^kind: ServiceAccount$' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c '^kind: Role$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: RoleBinding$' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c '^kind: ClusterRole$' "$RENDERED")" -eq 1 ]]
[[ "$(grep -c '^kind: ClusterRoleBinding$' "$RENDERED")" -eq 1 ]]
grep -q 'type: ClusterIP' "$RENDERED"
! grep -Eq '^kind: (Ingress|Gateway)$|type: (NodePort|LoadBalancer)|hostPort:' "$RENDERED"
grep -q 'role = "all-in-one"' "$RENDERED"
grep -q 'internal_bind = "0.0.0.0:8081"' "$RENDERED"
grep -q 'run_local_pool = false' "$RENDERED"
# Capacity rule: the Classroom flagship campaign deliberately enters ten
# independent Sessions. The local product Worker must admit that exact bound;
# autoscaling and per-Sandbox resource limits remain separate authorities.
grep -q 'worker_max_concurrent = 10' "$RENDERED"
grep -q 'no_browser = true' "$RENDERED"
grep -q 'identity_mode = "self-managed"' "$RENDERED"
grep -q 'worker_kubernetes_identity_directory = "/var/run/awaken-worker-identity"' "$RENDERED"
grep -q 'worker_kubernetes = { namespace = "awaken-system", service_account = "awaken-worker", audience = "awaken-worker", provider_id = "k3d-awaken-product" }' "$RENDERED"
! grep -q 'worker_trust_credentials_file' "$RENDERED"
! grep -q 'worker_request_credential_file' "$RENDERED"
grep -q 'sandbox_tier = "k8s"' "$RENDERED"
grep -q 'k8s_continuation_volume = { storage_class_name = "local-path", size = "1Gi" }' "$RENDERED"
grep -q "container_image = \"$SANDBOX_IMAGE\"" "$RENDERED"
! grep -q '__AWAKEN_SANDBOX_IMAGE__' "$RENDERED"
grep -q 'container_hand_residency = "resident"' "$RENDERED"
grep -q 'package_image_builder = "k8s"' "$RENDERED"
grep -q 'package_image_registry = "k3d-awaken-registry.localhost:5000/environments"' "$RENDERED"
grep -q 'k8s_buildkit_image = "k3d-awaken-registry.localhost:5000/system/buildkit:v0.30.0-rootless"' "$RENDERED"
! grep -q 'k3d-awaken-registry.localhost:5111' "$RENDERED"
grep -q 'package_registry_insecure = true' "$RENDERED"
grep -q 'serviceAccountName: awaken-product' "$RENDERED"
grep -q 'serviceAccountName: awaken-worker' "$RENDERED"
grep -q 'restartPolicy: Never' "$RENDERED"
grep -q 'awaken.dev/worker-process-evidence' "$RENDERED"
grep -q 'https://awaken-product:8443' "$RENDERED"
grep -q 'image: nginx:1.27-alpine' "$RENDERED"
grep -q 'secretName: awaken-worker-upstream-tls' "$RENDERED"
grep -q 'name: awaken-worker-upstream-ca' "$RENDERED"
[[ "$(grep -c 'credential_envelope_keys_file = "/etc/awaken-credential-envelope/keys.json"' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c 'secretName: awaken-worker-credential-envelope' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c 'defaultMode: 256' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c 'name: install-credential-envelope' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c 'chmod 0600 /target/keys.json' "$RENDERED")" -eq 2 ]]
grep -q -- '- jobs' "$RENDERED"
grep -q -- '- pods/exec' "$RENDERED"
grep -q -- '- pods/portforward' "$RENDERED"
PRODUCT_PVC_RULE="$(grep -A4 -- '- persistentvolumeclaims' "$RENDERED")"
grep -q -- '- create' <<<"$PRODUCT_PVC_RULE"
grep -q -- '- get' <<<"$PRODUCT_PVC_RULE"
grep -q -- '- delete' <<<"$PRODUCT_PVC_RULE"
! grep -q -- '- list' <<<"$PRODUCT_PVC_RULE"
! grep -q -- '- watch' <<<"$PRODUCT_PVC_RULE"
! grep -q -- '- update' <<<"$PRODUCT_PVC_RULE"
PRODUCT_OBJECT_RULE="$(grep -A12 -- '- pods' "$RENDERED" | head -13)"
grep -q -- '- create' <<<"$PRODUCT_OBJECT_RULE"
grep -q -- '- update' <<<"$PRODUCT_OBJECT_RULE"
grep -q -- '- delete' <<<"$PRODUCT_OBJECT_RULE"
grep -q 'persistentVolumeClaim:' "$RENDERED"
grep -q 'mountPath: /var/run/awaken-worker-identity' "$RENDERED"
grep -q 'audience: awaken-worker' "$RENDERED"
grep -q 'fieldPath: metadata.uid' "$RENDERED"
TOKEN_REVIEW_RULE="$(grep -A4 -- '- authentication.k8s.io' "$RENDERED")"
grep -q -- '- tokenreviews' <<<"$TOKEN_REVIEW_RULE"
grep -q -- '- create' <<<"$(grep -A3 -- '- tokenreviews' "$RENDERED")"
[[ "$(grep -c '^kind: Secret$' "$RENDERED")" -eq 0 ]]
grep -q 'startupProbe:' "$RENDERED"
grep -q 'failureThreshold: 180' "$RENDERED"
grep -q 'path: /readyz' "$RENDERED"
grep -q 'awaken.design/backend-access: "true"' "$RENDERED"
! grep -Eqi 'deepseek_api_key|provider-secret|api[_-]?key:|secret:' "$RENDERED"

echo "OK - internal Awaken product topology is private, durable, and secret-free."
