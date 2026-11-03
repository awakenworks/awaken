#!/usr/bin/env bash
# L1 one unique cluster owns all test disks/PVCs; L2 any failure => diagnostics
# and exact cleanup; L3 unrelated clusters/StorageClasses never enter this owner.
set -euo pipefail
[[ "$#" = 0 ]] || { echo 'usage: local_storage_e2e.sh (environment configuration only)' >&2; exit 2; }
LOCAL_STORAGE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$LOCAL_STORAGE_ROOT"
source e2e/k3d/harness.sh
k3d_require_tools
command -v helm >/dev/null
LOCAL_STORAGE_CLUSTER="awaken-pvc-$$"
LOCAL_STORAGE_CONTEXT="k3d-$LOCAL_STORAGE_CLUSTER"
LOCAL_STORAGE_OUTPUT="${AWAKEN_EVAL_ARTIFACT_DIR:-$LOCAL_STORAGE_ROOT/target/test-evidence/local-storage}"
LOCAL_STORAGE_CREATED=0
LOCAL_STORAGE_KUBECONFIG="$(mktemp)"
k3d_use_private_kubeconfig "$LOCAL_STORAGE_KUBECONFIG"
mkdir -p "$LOCAL_STORAGE_OUTPUT"
cleanup() {
  local status=0 step_status=0 node
  if [[ "$LOCAL_STORAGE_CREATED" = 1 ]]; then
    _k3d_check_creation || return "$?"
    node=$(_k3d_created_node_id "k3d-$LOCAL_STORAGE_CLUSTER-server-0") || return "$?"
    kubectl --context "$LOCAL_STORAGE_CONTEXT" get pv -o json > "$LOCAL_STORAGE_OUTPUT/final-pvs.json" 2>&1 || true
    kubectl --context "$LOCAL_STORAGE_CONTEXT" get events -A -o json > "$LOCAL_STORAGE_OUTPUT/final-events.json" 2>&1 || true
    kubectl --context "$LOCAL_STORAGE_CONTEXT" -n awaken-storage logs daemonset/awaken-local-storage --tail=300 > "$LOCAL_STORAGE_OUTPUT/controller.log" 2>&1 || true
    kubectl --context "$LOCAL_STORAGE_CONTEXT" delete namespace pvc-a pvc-b --ignore-not-found --wait=true --timeout=90s >/dev/null 2>&1 || status=$?
    kubectl --context "$LOCAL_STORAGE_CONTEXT" -n awaken-storage delete daemonset awaken-local-storage --ignore-not-found --wait=true --timeout=90s >/dev/null 2>&1 || step_status=$?
    [[ "$status" != 0 ]] || status="$step_status"
    step_status=0
    docker exec "$node" sh -ec 'for path in /mnt/awaken-local/volume-*; do test -d "$path" || continue; umount "$path"; done' >/dev/null 2>&1 || step_status=$?
    [[ "$status" != 0 ]] || status="$step_status"
  fi
  step_status=0
  k3d_delete_cluster_and_remove "$LOCAL_STORAGE_CLUSTER" "$LOCAL_STORAGE_KUBECONFIG" || step_status=$?
  [[ "$status" != 0 ]] || status="$step_status"
  return "$status"
}
k3d_install_exit_cleanup cleanup
# The shared creator alone refuses collisions and unknown observations.
k3d_create_cluster "$LOCAL_STORAGE_CLUSTER" 0 128Mi '' "$(k3d_local_storage_image)"
LOCAL_STORAGE_CREATED=1
k3d_bind_private_kubeconfig "$LOCAL_STORAGE_CLUSTER"

k3d_import_storage_images "$LOCAL_STORAGE_CLUSTER"
k3d_install_local_storage "$LOCAL_STORAGE_CLUSTER" "$LOCAL_STORAGE_OUTPUT/install" 4
AWAKEN_LOCAL_STORAGE_CONTEXT="$LOCAL_STORAGE_CONTEXT" AWAKEN_EVAL_ARTIFACT_DIR="$LOCAL_STORAGE_OUTPUT" \
  AWAKEN_LOCAL_STORAGE_COMMIT="$(git rev-parse HEAD)" \
  node e2e/k3d/local_storage_driver.mjs
