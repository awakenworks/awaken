#!/usr/bin/env bash
# Real Kubernetes (k3d) end-to-end for the container ACP tier: stand up a cluster,
# load the busybox fixture image into its node, and run the gated k8s e2e test
# (crates/worker/awaken-sandbox-container/tests/k8s_e2e.rs) against it.
#
# Verified path: K8sRuntime::connect → create_container (real Session Pod via the kube
# API) → Pod Running → exec subresource stdio → newline ACP wire exchange.
#
# Prereqs: docker (daemon up), k3d, kubectl, a rust toolchain. Idempotent; cleans up
# the cluster on exit unless AWAKEN_K8S_KEEP=1.
set -euo pipefail

cd "$(dirname "$0")/../.."
. e2e/k3d/harness.sh

CLUSTER="${AWAKEN_K8S_CLUSTER:-awaken-k8s-e2e}"
FIXTURE_IMAGE="${AWAKEN_K8S_FIXTURE_IMAGE:-awaken-bb:1}"
export AWAKEN_K8S_FIXTURE_IMAGE="$FIXTURE_IMAGE"
SESSION_IMAGE="${AWAKEN_K8S_SESSION_IMAGE:-awaken-sandbox:local}"
export AWAKEN_K8S_SESSION_IMAGE="$SESSION_IMAGE"
KUBECONFIG_FILE="$(mktemp)"
FIXTURE_CONTAINER="awaken-bb-tmp-$$"

log() { printf '\n=== %s ===\n' "$*"; }

cleanup() {
  docker rm -f "$FIXTURE_CONTAINER" >/dev/null 2>&1 || true
  rm -f "$KUBECONFIG_FILE"
  if [ "${AWAKEN_K8S_KEEP:-0}" != "1" ]; then
    log "deleting cluster ${CLUSTER}"
    k3d_delete_cluster "$CLUSTER"
  fi
}
trap cleanup EXIT

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
docker run --name "$FIXTURE_CONTAINER" --platform "$K3D_PLATFORM" busybox:1.36 true >/dev/null
docker commit "$FIXTURE_CONTAINER" "$FIXTURE_IMAGE" >/dev/null
docker rm -f "$FIXTURE_CONTAINER" >/dev/null
"$(dirname "$0")/../../deploy/images/sandbox/build.sh" --ensure-hand "$SESSION_IMAGE" ""

log "creating k3d cluster ${CLUSTER}"
k3d_create_cluster "$CLUSTER" 0 2
k3d kubeconfig merge "${CLUSTER}" --output "$KUBECONFIG_FILE" --overwrite >/dev/null
export KUBECONFIG="$KUBECONFIG_FILE"
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

# The Hand-expiry rule needs the production binary; the canonical importer also
# loads the cluster's exact Pause/CoreDNS prerequisites into every node.
log "loading images through the shared k3d harness"
k3d_import_images "$CLUSTER" "$FIXTURE_IMAGE" "$SESSION_IMAGE"

log "running the k8s e2e test"
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-container --features k8s --test k8s_it -- --nocapture
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-container --features k8s --test k8s_e2e -- --nocapture
AWAKEN_K8S_E2E=1 cargo test -p awaken-runtime-host --features container-k8s --lib \
  k8s_live_pvc_initialization_is_readable_and_reused -- --nocapture

log "k8s container e2e PASSED"
