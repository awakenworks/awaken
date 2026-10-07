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
CONTROL_RENDERED="$(mktemp)"
trap 'rm -f "$RENDERED" "$CONTROL_RENDERED"' EXIT

# Internal-product topology cause/effect decision table.
# Causes: C1 the product overlay renders; C2 the Awaken Service is ClusterIP and
# no external Kubernetes route exists; C3 the restartable AllInOne owns no
# execution identity while one non-restarting Worker Job carries the Pod-bound
# identity/token projection, exact TokenReview authority, retained stop-evidence
# finalizer, TLS transport, and bounded probes; C4 ingress is
# restricted to namespaces explicitly labelled as an
# Awaken Design backend; C5 K8s sandbox uses the imported non-`latest` base and
# the canonical retained continuation PVC and separate Create-only executor
# and read/UID-fenced disposal Roles, while BuildKit Jobs, the derived-image
# Registry, and namespace-scoped RBAC including
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
# C8 the Create ledger has a distinct namespace and only the product
# ServiceAccount can create/get/update its ConfigMaps; the Worker has no
# binding there and neither principal may delete the retained ledger.
# K3 C8=>the topology permits an independently retained ledger; the live
# Worker transport test owns UID reobservation after Pod loss.
# K4 !C8=>reject the topology before calling it brokered recovery.
kubectl kustomize "$OVERLAY/create-control" > "$CONTROL_RENDERED"
grep -q 'name: awaken-create-control' "$CONTROL_RENDERED"
[[ "$(grep -c 'namespace: awaken-create-control' "$CONTROL_RENDERED")" -eq 2 ]]
grep -q 'name: awaken-product' "$CONTROL_RENDERED"
grep -q 'namespace: awaken-system' "$CONTROL_RENDERED"
! grep -q 'name: awaken-worker' "$CONTROL_RENDERED"
! grep -q '  - delete' "$CONTROL_RENDERED"
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
[[ "$(grep -c '^kind: Role$' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c '^kind: RoleBinding$' "$RENDERED")" -eq 3 ]]
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
grep -q 'worker_kubernetes = { namespace = "awaken-system", service_account = "awaken-worker", audience = "awaken-worker", provider_id = "k3d-awaken-product", create_control_namespace = "awaken-create-control" }' "$RENDERED"
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
# Credential projection cause/effect table: C1 exact Secret + C2 pod fsGroup +
# C3 read-only group mode + C4 no init participant => both processes can read
# one private key source and the Worker remains bindable. D1 C1..C4 => two
# direct 0440 projections and zero init/source-copy paths. D2 !C3 or !C4 => the
# static contract fails before deployment, preventing startup failure or an
# execution domain that PodProcessObserver must reject.
[[ "$(grep -c 'defaultMode: 288' "$RENDERED")" -eq 2 ]]
[[ "$(grep -c 'name: credential-envelope$' "$RENDERED")" -eq 4 ]]
! grep -q 'credential-envelope-source' "$RENDERED"
! grep -q 'name: install-credential-envelope' "$RENDERED"
grep -q -- '- jobs' "$RENDERED"
grep -q -- '- pods/exec' "$RENDERED"
grep -q -- '- pods/portforward' "$RENDERED"
# RBAC cause/effect table: B1 Worker receives only the shared read/dispose
# Role -> zero direct collection Create across Pod/PVC/CM/Secret/Job/DaemonSet;
# B2 product receives that same Role plus the exact Create-only Role -> broker
# can forward all six kinds. B3 the Worker retains exec/port-forward subresource
# Create for its existing bounded channel; this must not grant collection
# Create. Check rendered documents rather than trusting YAML source ordering.
python3 - "$RENDERED" <<'PY'
from pathlib import Path
import sys

documents = Path(sys.argv[1]).read_text().split('\n---\n')
def document(kind, name):
    matches = [part for part in documents if f'\nkind: {kind}\n' in '\n' + part and f'\nmetadata:\n  name: {name}\n' in '\n' + part]
    assert len(matches) == 1, (kind, name, len(matches))
    return matches[0]
def rules(body):
    found = []
    section = None
    for line in body.splitlines():
        if line == '- apiGroups:':
            found.append({'resources': set(), 'verbs': set()})
            section = None
        elif line == '  resources:':
            section = 'resources'
        elif line == '  verbs:':
            section = 'verbs'
        elif line.startswith('  - ') and section and found:
            found[-1][section].add(line[4:].strip('"'))
    return found

direct = {'pods', 'persistentvolumeclaims', 'configmaps', 'secrets', 'jobs', 'daemonsets'}
base = rules(document('Role', 'awaken-runtime-read-dispose'))
assert base and all('create' not in rule['verbs'] for rule in base if rule['resources'] & direct), 'B1'
assert any({'pods/exec', 'pods/portforward'} <= rule['resources'] and 'create' in rule['verbs'] for rule in base), 'B3'
creation = rules(document('Role', 'awaken-create-executor'))
assert creation and all(rule['verbs'] == {'create'} for rule in creation), 'B2 only Create'
assert set().union(*(rule['resources'] for rule in creation)) == direct, 'B2 six exact kinds'
worker = document('RoleBinding', 'awaken-worker-runtime')
assert 'name: awaken-runtime-read-dispose' in worker and 'name: awaken-worker' in worker and 'name: awaken-product' not in worker, 'B1 binding'
product = document('RoleBinding', 'awaken-runtime-read-dispose')
assert 'name: awaken-product' in product, 'B2 shared base'
product_create = document('RoleBinding', 'awaken-product-create')
assert 'name: awaken-create-executor' in product_create and 'name: awaken-product' in product_create and 'name: awaken-worker' not in product_create, 'B2 Create owner'
PY
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

# Live recovery rule PR1: the canonical Worker Job suffers three nonzero exits
# with its test failure budget reduced to zero. Effect: native Kubernetes must
# create three different Pod UIDs, each with zero container restarts, without
# reaching Failed/FailureTarget. PR2 absent/Count retry policy => the first exit
# exhausts the Job. This observes the actual controller, not a copied retry loop.
# The fixture runs only `exit 75`: it never registers, claims, or executes model
# work. Its projection omits execution credentials and the production evidence
# finalizer; the static contract above and existing PodProcessObserver tests own
# that separate authority boundary. Required live mode never silently skips.
if [[ "${AWAKEN_K8S_E2E:-0}" == 1 ]]; then
  python3 - "$RENDERED" "${AWAKEN_K8S_E2E_CONTEXT:?explicit live context required}" \
    "${AWAKEN_K8S_NAMESPACE:?explicit test namespace required}" \
    "${AWAKEN_K8S_FIXTURE_IMAGE:?exact fixture image required}" <<'PY'
import json, os, subprocess, sys, time

rendered, context, namespace, image = sys.argv[1:]
if not context.startswith("k3d-"):
    raise SystemExit("product recovery witness requires an explicit local k3d context")
if "@sha256:" not in image:
    raise SystemExit("product recovery witness requires a digest-pinned image")
base = ["kubectl", "--context", context, "--namespace", namespace, "--request-timeout=10s"]

def call(*args, input=None):
    return subprocess.run(base + list(args), input=input, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          check=True, timeout=30).stdout

stream = call("create", "--dry-run=client", "--validate=false", "-f", rendered, "-o", "json").strip()
objects = []
decoder = json.JSONDecoder()
while stream:
    item, end = decoder.raw_decode(stream)
    objects.extend(item["items"] if item.get("kind") == "List" else [item])
    stream = stream[end:].lstrip()
job = next(item for item in objects
           if item["kind"] == "Job" and item["metadata"]["name"] == "awaken-worker")
name = f"worker-recovery-witness-{os.getpid()}"
job["metadata"] = {"name": name, "namespace": namespace}
job["spec"]["backoffLimit"] = 0
template = job["spec"]["template"]
template["metadata"] = {"labels": {"awaken.dev/recovery-witness": name}}
pod = template["spec"]
pod.pop("serviceAccountName", None)
pod.pop("volumes", None)
pod["automountServiceAccountToken"] = False
container = pod["containers"][0]
container["image"] = image
container["imagePullPolicy"] = "IfNotPresent"
container["command"] = ["/bin/sh", "-c", "exit 75"]
for key in ["args", "volumeMounts", "ports", "readinessProbe", "lifecycle"]:
    container.pop(key, None)
created = False
try:
    call("create", "-f", "-", input=json.dumps(job))
    created = True
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        observed = json.loads(call("get", "job", name, "-o", "json"))
        status = observed.get("status", {})
        assert not any(c["type"] in ["Failed", "FailureTarget", "Complete"]
                       and c.get("status") == "True" for c in status.get("conditions", [])), \
            f"PR2 Worker recovery exhausted its Job budget: {status}"
        pods = json.loads(call("get", "pods", "-l", f"job-name={name}", "-o", "json"))["items"]
        exited = [p for p in pods if p.get("status", {}).get("phase") == "Failed"]
        if len(exited) >= 3:
            uids = {p["metadata"]["uid"] for p in exited}
            assert len(uids) == len(exited), "PR1 every retry needs a fresh Pod UID"
            for p in exited:
                containers = p["status"]["containerStatuses"]
                assert all(c["restartCount"] == 0 for c in containers), "PR1 no same-Pod process restart"
                assert containers[0]["state"]["terminated"]["exitCode"] == 75, "PR1 injected failure observed"
            assert status.get("failed", 0) == 0, "PR1 exits must not spend failure budget"
            print(json.dumps({"rule": "PR1", "context": context, "job": name,
                              "observedFailedPods": len(exited), "uniquePodUids": sorted(uids),
                              "countedFailures": status.get("failed", 0)}))
            break
        time.sleep(0.5)
    else:
        raise AssertionError("PR1 native Job controller did not replace three failed Worker incarnations")
finally:
    if created:
        call("delete", "job", name, "--wait=true", "--timeout=20s")
PY
fi
