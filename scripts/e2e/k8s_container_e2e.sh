#!/usr/bin/env bash
# Real Kubernetes (k3d) end-to-end for the container ACP tier: stand up a cluster,
# load the busybox `nc` fixture image into its node, and run the gated k8s e2e test
# (crates/worker/awaken-sandbox-container/tests/k8s_e2e.rs) against it.
#
# Verified path: K8sRuntime::connect → create_container (real Pod via the kube API) →
# Pod Running → kubectl port-forward → open_channel dial → newline ACP wire exchange.
#
# Prereqs: docker (daemon up), k3d, kubectl, a rust toolchain. Idempotent; cleans up
# the cluster on exit unless AWAKEN_K8S_KEEP=1.
set -euo pipefail

CLUSTER="${AWAKEN_K8S_CLUSTER:-awaken-k8s-e2e}"
FIXTURE_IMAGE="awaken-bb:1"
NODE="k3d-${CLUSTER}-server-0"

log() { printf '\n=== %s ===\n' "$*"; }

cleanup() {
  if [ "${AWAKEN_K8S_KEEP:-0}" != "1" ]; then
    log "deleting cluster ${CLUSTER}"
    k3d cluster delete "${CLUSTER}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

log "creating k3d cluster ${CLUSTER}"
if ! k3d cluster list 2>/dev/null | grep -q "^${CLUSTER}\b"; then
  k3d cluster create "${CLUSTER}" --wait --timeout 150s
fi
export KUBECONFIG="$(k3d kubeconfig write "${CLUSTER}")"
kubectl wait --for=condition=Ready "node/${NODE}" --timeout=120s

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
docker exec "${NODE}" crictl images 2>/dev/null | grep -E "pause|awaken-bb" || true

log "running the k8s e2e test"
AWAKEN_K8S_E2E=1 cargo test -p awaken-sandbox-container --features k8s --test k8s_e2e -- --nocapture

log "k8s container e2e PASSED"
