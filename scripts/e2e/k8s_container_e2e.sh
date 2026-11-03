#!/usr/bin/env bash
# Real Kubernetes (k3d) end-to-end for the container ACP tier: stand up a cluster,
# load the busybox fixture image into its node, and run the gated k8s e2e test
# (crates/worker/awaken-sandbox-kubernetes/tests/k8s_e2e.rs) against it.
#
# Verified path: K8sRuntime::connect → create_container (real Session Pod via the kube
# API) → Pod Running → exec subresource stdio → newline ACP wire exchange.
#
# Prereqs: docker (daemon up), k3d, kubectl, a rust toolchain. Idempotent; cleans up
# the cluster on exit unless AWAKEN_K8S_KEEP=1.
set -euo pipefail

cd "$(dirname "$0")/../.."

# This runner takes no arguments; it used to accept and discard them, so a
# caller's flag disappeared into a full cluster build. Refuse before any tool
# admission or cluster creation.
if [ "$#" -ne 0 ]; then
  echo "usage: scripts/e2e/k8s_container_e2e.sh" >&2
  exit 2
fi
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"
source scripts/ci/_rust_test_execution.sh
. e2e/k3d/harness.sh

CLUSTER="${AWAKEN_K8S_CLUSTER:-awaken-k8s-e2e}"
export AWAKEN_K3D_CLUSTER="$CLUSTER"
FIXTURE_IMAGE="${AWAKEN_K8S_FIXTURE_IMAGE:-awaken-bb:1}"
export AWAKEN_K8S_FIXTURE_IMAGE="$FIXTURE_IMAGE"
SESSION_IMAGE="${AWAKEN_K8S_SESSION_IMAGE:-awaken-sandbox:local}"
export AWAKEN_K8S_SESSION_IMAGE="$SESSION_IMAGE"
CONTROL_IMAGE="${AWAKEN_K8S_CONTROL_IMAGE:-}"
if [[ -n "$CONTROL_IMAGE" ]] && ! k3d_validate_image_digest "$CONTROL_IMAGE"; then
  echo "AWAKEN_K8S_CONTROL_IMAGE must name the digest-pinned operator image used by the control E2E" >&2
  exit 2
fi
export AWAKEN_K8S_CONTROL_IMAGE="$CONTROL_IMAGE"
KUBECONFIG_FILE="$(mktemp)"
k3d_use_private_kubeconfig "$KUBECONFIG_FILE"
FIXTURE_CONTAINER="awaken-bb-tmp-$$"
FIXTURE_CONTAINER_ID=""

log() { printf '\n=== %s ===\n' "$*"; }

cleanup() {
  local fixture_status=0 cluster_status=0
  [[ -z "$FIXTURE_CONTAINER_ID" ]] || docker rm -f -v "$FIXTURE_CONTAINER_ID" >/dev/null 2>&1 || fixture_status=$?
  if [ "${AWAKEN_K8S_KEEP:-0}" = "1" ]; then
    log "retaining cluster ${CLUSTER} and kubeconfig ${KUBECONFIG_FILE}"
  else
    log "deleting cluster ${CLUSTER}"
    k3d_delete_cluster_and_remove "$CLUSTER" "$KUBECONFIG_FILE" || cluster_status=$?
  fi
  [[ "$fixture_status" = 0 ]] || return "$fixture_status"
  return "$cluster_status"
}

k3d_install_exit_cleanup cleanup

k3d_admit_or_exit "Kubernetes container E2E" 1

# Prepare the exact offline fixtures before starting the disposable cluster. The
# shared harness remains the sole owner of cluster policy and image import.
log "building offline fixture images"
# Reuse the exact local fixture when present. Requiring a registry HEAD even
# after the bytes are cached makes the otherwise-offline K8s proof depend on
# Docker Hub availability.
if ! docker image inspect busybox:1.36 >/dev/null 2>&1; then
  docker pull --platform "$K3D_PLATFORM" busybox:1.36 >/dev/null
fi
FIXTURE_CONTAINER_ID=$(docker create --name "$FIXTURE_CONTAINER" --platform "$K3D_PLATFORM" busybox:1.36 true)
[[ "$FIXTURE_CONTAINER_ID" =~ ^[0-9a-f]{64}$ ]] || exit 2
docker start -a "$FIXTURE_CONTAINER_ID" >/dev/null
docker commit "$FIXTURE_CONTAINER_ID" "$FIXTURE_IMAGE" >/dev/null
docker rm -f -v "$FIXTURE_CONTAINER_ID" >/dev/null
FIXTURE_CONTAINER_ID=""
# KQ0 no operator override => use the existing image owner's current-input
# admission, resolve its real manifest digest, then run the mandatory control test.
# KQ1 an explicit digest remains supported; malformed input fails above.
# Cause/effect: exact accepted tag + matching source fingerprint reuses the
# same bytes; a stale/missing tag rebuilds and revalidates. A hand-only check
# cannot qualify a changed control binary. The build owner remains singular.
"$(dirname "$0")/../../deploy/images/sandbox/build.sh" --ensure-current "$SESSION_IMAGE" ""
if [[ -z "$CONTROL_IMAGE" ]]; then
  CONTROL_IMAGE=$(k3d_image_digest "$SESSION_IMAGE")
fi
export AWAKEN_K8S_CONTROL_IMAGE="$CONTROL_IMAGE"

log "creating k3d cluster ${CLUSTER}"
# The shared harness owns nested-node reserve and actual capacity admission.
k3d_create_cluster "$CLUSTER" 0
k3d_bind_private_kubeconfig "$CLUSTER"
if [ "$(uname -s)" = "Darwin" ]; then
  # k3d may emit host.docker.internal on Docker Desktop. That address is for
  # containers reaching the host and can time out when kubectl itself runs on
  # the host. Use the load balancer's published loopback port instead.
  api_endpoint="$(docker port "k3d-${CLUSTER}-serverlb" 6443/tcp | head -n 1)"
  api_port="${api_endpoint##*:}"
  kubectl config set-cluster "k3d-${CLUSTER}" \
    --server="https://127.0.0.1:${api_port}" >/dev/null
fi
# The Docker container name is stable, but the Kubernetes Node object registers
# asynchronously and its name is an implementation detail. Wait until at least one
# Node exists, then wait for the actual object rather than assuming both names match.
node_registered=0
for _ in $(seq 1 60); do
  if kubectl --request-timeout=5s get nodes -o name 2>/dev/null | grep -q '^node/'; then
    node_registered=1
    break
  fi
  sleep 2
done
[ "$node_registered" = 1 ] || { echo "k3d node never registered" >&2; exit 1; }
kubectl --request-timeout=10s wait --for=condition=Ready nodes --all --timeout=120s

# ADR-0078 same-name isolation, observed against the real API server: an
# admitted Create paused in admission returns 409 while an inert object of the
# same Kind holds the namespace/name (reservation-held), publishes a new UID once
# that object is released (reservation-released), and a Pod Create still lands
# after its Namespace finalized (namespace-deletion, the counterexample). The
# control serves its pausing webhook on the host; the cluster reaches the host at
# the gateway of its Docker network.
log "qualifying same-name late-Create isolation for every brokered Kind"
webhook_address="$(docker network inspect "k3d-${CLUSTER}" -f '{{(index .IPAM.Config 0).Gateway}}')"
late_create() {
  python3 scripts/ci/k8s_namespace_late_create_negative_control.py \
    --kubeconfig "$KUBECONFIG_FILE" --advertise-address "$webhook_address" "$@"
}
late_create --resource pods --scenario namespace-deletion
for resource in pods configmaps secrets persistentvolumeclaims jobs daemonsets; do
  late_create --resource "$resource" --scenario reservation-held
  late_create --resource "$resource" --scenario reservation-released
done

# Install the exact graph production imports so the live suite exercises the
# same source attested by Worker readiness and restricted Pod admission.
log "installing canonical Sandbox NetworkPolicy graph"
kubectl apply -k deploy/k3d/bases/sandbox-network-policy

# The Hand-expiry rule needs the production binary; the canonical importer also
# loads the cluster's exact Pause/CoreDNS prerequisites into every node. The
# retained-filesystem rules additionally need the exact provisioner image:
# importing it from the host keeps this proof independent of in-node registry
# access, which is intentionally unavailable in offline CI topologies.
log "loading images through the shared k3d harness"
k3d_import_storage_images "$CLUSTER" "$FIXTURE_IMAGE" "$SESSION_IMAGE" "$CONTROL_IMAGE"

# Live cross-namespace allowlist cause/effect decision table:
# C1 the client is selected by the canonical default-deny and exact allowlist
# policy; C2 the destination namespace is the fixed `awaken-cloud` authority;
# C3 the destination Pod has the egress-gateway role; C4 the TCP port is 8081.
# E1 C1+C2+C3+C4 reaches the gateway; E2 changing only C3 is denied; E3
# changing only C2 is denied. Rules KNP1 exact tuple=>E1, KNP2 exact namespace
# plus decoy role=>E2, KNP3 foreign namespace plus copied role=>E3. Waiting for
# KNP1 before the negative probes fences policy-controller propagation, so a
# timeout cannot be misreported as successful isolation.
log "proving live cross-namespace egress allowlist boundaries"
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: awaken-cloud
---
apiVersion: v1
kind: Namespace
metadata:
  name: attacker-controlled
---
apiVersion: v1
kind: Pod
metadata:
  name: egress-gateway
  namespace: awaken-cloud
  labels:
    app.kubernetes.io/component: egress-gateway
spec:
  restartPolicy: Never
  containers:
    - name: http
      image: ${FIXTURE_IMAGE}
      imagePullPolicy: Never
      command: ["sh", "-c"]
      args: ["mkdir -p /www && echo gateway-ok >/www/index.html && exec httpd -f -p 8081 -h /www"]
---
apiVersion: v1
kind: Pod
metadata:
  name: role-decoy
  namespace: awaken-cloud
  labels:
    app.kubernetes.io/component: role-decoy
spec:
  restartPolicy: Never
  containers:
    - name: http
      image: ${FIXTURE_IMAGE}
      imagePullPolicy: Never
      command: ["sh", "-c"]
      args: ["mkdir -p /www && echo decoy >/www/index.html && exec httpd -f -p 8081 -h /www"]
---
apiVersion: v1
kind: Pod
metadata:
  name: copied-gateway-role
  namespace: attacker-controlled
  labels:
    app.kubernetes.io/component: egress-gateway
spec:
  restartPolicy: Never
  containers:
    - name: http
      image: ${FIXTURE_IMAGE}
      imagePullPolicy: Never
      command: ["sh", "-c"]
      args: ["mkdir -p /www && echo attacker >/www/index.html && exec httpd -f -p 8081 -h /www"]
---
apiVersion: v1
kind: Pod
metadata:
  name: allowlist-client
  labels:
    app: awaken-sandbox
    awaken-egress: allowlist
spec:
  restartPolicy: Never
  containers:
    - name: client
      image: ${FIXTURE_IMAGE}
      imagePullPolicy: Never
      command: ["sh", "-c", "sleep 600"]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: awaken-sandbox-allowlist-egress
spec:
  podSelector:
    matchLabels:
      app: awaken-sandbox
      awaken-egress: allowlist
  policyTypes: [Egress]
  egress:
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: awaken-cloud
          podSelector:
            matchLabels:
              app.kubernetes.io/component: egress-gateway
      ports:
        - protocol: TCP
          port: 8081
EOF
kubectl -n awaken-cloud wait --for=condition=Ready pod/egress-gateway pod/role-decoy --timeout=120s
kubectl -n attacker-controlled wait --for=condition=Ready pod/copied-gateway-role --timeout=120s
kubectl wait --for=condition=Ready pod/allowlist-client --timeout=120s
gateway_ip="$(kubectl -n awaken-cloud get pod egress-gateway -o jsonpath='{.status.podIP}')"
decoy_ip="$(kubectl -n awaken-cloud get pod role-decoy -o jsonpath='{.status.podIP}')"
attacker_ip="$(kubectl -n attacker-controlled get pod copied-gateway-role -o jsonpath='{.status.podIP}')"
gateway_response=""
for _ in $(seq 1 30); do
  gateway_response="$(kubectl exec allowlist-client -- wget -q -T 2 -O - "http://${gateway_ip}:8081" 2>/dev/null || true)"
  [ "$gateway_response" = "gateway-ok" ] && break
  sleep 1
done
[ "$gateway_response" = "gateway-ok" ] || {
  echo "KNP1 exact cross-namespace gateway tuple was not reachable" >&2
  exit 1
}
if kubectl exec allowlist-client -- wget -q -T 3 -O - "http://${decoy_ip}:8081" >/dev/null 2>&1; then
  echo "KNP2 same-namespace decoy role bypassed the egress allowlist" >&2
  exit 1
fi
if kubectl exec allowlist-client -- wget -q -T 3 -O - "http://${attacker_ip}:8081" >/dev/null 2>&1; then
  echo "KNP3 foreign namespace with copied gateway role bypassed the egress allowlist" >&2
  exit 1
fi

log "running the k8s e2e test"
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-kubernetes --test k8s_it -- --nocapture
# These scenarios deliberately share one small, offline cluster and exercise
# retained PVC deletion. Running all of them concurrently turns local-path's
# single-node cleanup queue into the subject under test and can leave Pods in
# Terminating long enough to mask the Sandbox lifecycle contract. Keep the
# product assertions strict, but serialize this substrate fixture.
# Live-suite qualification decision table: C1 the cluster gate is enabled; C2
# the control gate is enabled; C3 the exact digest-pinned control image was
# imported above. KQ1 C1+C2+C3 executes the marker/helper roundtrip; KQ2 any
# absent cause fails before Cargo rather than returning a successful skipped
# test. The Rust test remains the sole owner of the control-path assertions.
AWAKEN_K8S_E2E=1 AWAKEN_K8S_CONTROL_E2E=1 AWAKEN_K8S_ISOLATION_E2E=1 \
  AWAKEN_K8S_CONTROL_IMAGE="$CONTROL_IMAGE" \
  cargo test -p awaken-sandbox-kubernetes \
  --test k8s_e2e -- --nocapture --test-threads=1
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-kubernetes \
  --test k8s_policy_drift -- --nocapture --test-threads=1
# ADR-0092: a namespace quota refusal and an unschedulable Pod are both
# CapacityUnavailable, leave no Pod behind, and succeed once there is room.
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-kubernetes \
  --test k8s_capacity_e2e -- --nocapture --test-threads=1
# KQ3 the PVC case belongs to realization after extraction. Use the existing
# required executor: the same exact case must be listed and then pass with the
# live dependency enabled. KQ4 a stale owner/filter selects zero; KQ5 a missing
# live switch skips; KQ6 an assertion/substrate failure returns nonzero. All
# three refuse qualification rather than reporting a successful empty run.
AWAKEN_K8S_E2E=1 run_required_rust_tests -p awaken-session-realization \
  --features k8s-tests --lib --locked \
  cache_volume::tests::k8s_live_pvc_initialization_is_readable_and_reused \
  --require-case=cache_volume::tests::k8s_live_pvc_initialization_is_readable_and_reused \
  -- --exact
# Pod-bound Worker identity and independent stop are qualified by the existing
# distributed-control k3d gate, which first verifies exact test selection.

log "k8s container e2e PASSED"
