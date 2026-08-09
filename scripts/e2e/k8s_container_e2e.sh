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

CLUSTER="${AWAKEN_K8S_CLUSTER:-awaken-k8s-e2e}"
FIXTURE_IMAGE="${AWAKEN_K8S_FIXTURE_IMAGE:-awaken-bb:1}"
export AWAKEN_K8S_FIXTURE_IMAGE="$FIXTURE_IMAGE"
SESSION_IMAGE="${AWAKEN_K8S_SESSION_IMAGE:-awaken-sandbox:local}"
export AWAKEN_K8S_SESSION_IMAGE="$SESSION_IMAGE"
NODE="k3d-${CLUSTER}-server-0"
KUBECONFIG_FILE="$(mktemp)"

log() { printf '\n=== %s ===\n' "$*"; }

cleanup() {
  rm -f "$KUBECONFIG_FILE"
  if [ "${AWAKEN_K8S_KEEP:-0}" != "1" ]; then
    log "deleting cluster ${CLUSTER}"
    k3d cluster delete "${CLUSTER}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

log "creating k3d cluster ${CLUSTER}"
cluster_row="$(k3d cluster list --no-headers 2>/dev/null | awk -v name="$CLUSTER" '$1 == name { print; exit }')"
if [ -z "$cluster_row" ]; then
  # Keep the disposable test node usable on large developer disks: k3s defaults to
  # a percentage threshold that can taint the node while hundreds of GiB remain.
  # Other repository k3d gates use the same 2% floor.
  EVICT="eviction-hard=imagefs.available<2%,nodefs.available<2%"
  k3d cluster create "${CLUSTER}" --wait --timeout 150s \
    --runtime-ulimit "nofile=65536:65536" \
    --k3s-arg "--kubelet-arg=$EVICT@server:*"
else
  read -r _ servers _ load_balancer <<<"$cluster_row"
  ready_servers="${servers%/*}"
  desired_servers="${servers#*/}"
  # Reuse is safe only for a complete cluster. An interrupted earlier setup can
  # leave a row behind with no load balancer; waiting for Nodes in that topology
  # only converts the original failure into a two-minute timeout.
  if [ "$ready_servers" != "$desired_servers" ] \
    || [ "$desired_servers" = "0" ] \
    || [ "$load_balancer" != "true" ]; then
    echo "existing k3d cluster is incomplete: $cluster_row" >&2
    exit 1
  fi
fi
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

# Load the pause image + a flattened busybox as the fixture. The node has no registry
# egress here, and a multi-arch `docker save` can miss a blob for containerd — flatten
# via `docker commit` so the archive imports cleanly.
log "loading images into the node's containerd"
docker pull rancher/mirrored-pause:3.6 >/dev/null 2>&1 || true
docker save rancher/mirrored-pause:3.6 | docker exec -i "${NODE}" ctr -n k8s.io images import - >/dev/null

docker pull --platform linux/amd64 busybox:1.36 >/dev/null
docker rm -f awaken-bb-tmp >/dev/null 2>&1 || true
docker run --name awaken-bb-tmp --platform linux/amd64 busybox:1.36 true >/dev/null 2>&1 || true
docker commit awaken-bb-tmp "${FIXTURE_IMAGE}" >/dev/null
docker rm -f awaken-bb-tmp >/dev/null 2>&1 || true
docker save "${FIXTURE_IMAGE}" | docker exec -i "${NODE}" ctr -n k8s.io images import - >/dev/null

# The Hand-expiry rule executes the production binary inside the Session Pod;
# busybox cannot prove that path. Build the canonical sandbox image with an
# explicitly empty ACP package set (the test needs only `hand --stdio`) and load
# the exact selected tag into the same isolated node.
"$(dirname "$0")/../../deploy/images/sandbox/build.sh" --ensure-hand "$SESSION_IMAGE" ""
docker save "$SESSION_IMAGE" | docker exec -i "${NODE}" ctr -n k8s.io images import - >/dev/null
docker exec "${NODE}" crictl images 2>/dev/null | grep -E "pause|awaken-bb|awaken-sandbox" || true

log "running the k8s e2e test"
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-container --features k8s --test k8s_it -- --nocapture
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-container --features k8s --test k8s_e2e -- --nocapture

log "k8s container e2e PASSED"
