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
LOCAL_STORAGE_PRIOR_CONTEXT="$(kubectl config current-context 2>/dev/null || true)"
mkdir -p "$LOCAL_STORAGE_OUTPUT"
cleanup() {
  local status=$?
  if [[ "$LOCAL_STORAGE_CREATED" = 1 ]]; then
    kubectl --context "$LOCAL_STORAGE_CONTEXT" get pv -o json > "$LOCAL_STORAGE_OUTPUT/final-pvs.json" 2>&1 || true
    kubectl --context "$LOCAL_STORAGE_CONTEXT" get events -A -o json > "$LOCAL_STORAGE_OUTPUT/final-events.json" 2>&1 || true
    kubectl --context "$LOCAL_STORAGE_CONTEXT" -n awaken-storage logs daemonset/awaken-local-storage --tail=300 > "$LOCAL_STORAGE_OUTPUT/controller.log" 2>&1 || true
    kubectl --context "$LOCAL_STORAGE_CONTEXT" delete namespace pvc-a pvc-b --ignore-not-found --wait=true --timeout=90s >/dev/null 2>&1 || true
    kubectl --context "$LOCAL_STORAGE_CONTEXT" -n awaken-storage delete daemonset awaken-local-storage --ignore-not-found --wait=true --timeout=90s >/dev/null 2>&1 || true
    docker exec "k3d-$LOCAL_STORAGE_CLUSTER-server-0" sh -ec 'for path in /mnt/awaken-local/volume-*; do test -d "$path" || continue; umount "$path"; done' >/dev/null 2>&1 || true
    k3d_delete_cluster "$LOCAL_STORAGE_CLUSTER" || status=1
  fi
  if [[ -n "$LOCAL_STORAGE_PRIOR_CONTEXT" && "$(kubectl config current-context 2>/dev/null || true)" = "$LOCAL_STORAGE_CONTEXT" ]]; then
    kubectl config use-context "$LOCAL_STORAGE_PRIOR_CONTEXT" >/dev/null || status=1
  fi
  exit "$status"
}
trap cleanup EXIT
if k3d cluster list -o json | python3 -c 'import json,sys;sys.exit(0 if any(x["name"]==sys.argv[1] for x in json.load(sys.stdin)) else 1)' "$LOCAL_STORAGE_CLUSTER"; then
  echo 'unique fixture already exists; refusing replacement' >&2; exit 2
fi
LOCAL_STORAGE_CREATED=1
k3d_create_cluster "$LOCAL_STORAGE_CLUSTER" 0 128Mi '' "$(k3d_local_storage_image)"
k3d_import_storage_images "$LOCAL_STORAGE_CLUSTER"
k3d_install_local_storage "$LOCAL_STORAGE_CLUSTER" "$LOCAL_STORAGE_OUTPUT/install" 4
AWAKEN_LOCAL_STORAGE_CONTEXT="$LOCAL_STORAGE_CONTEXT" AWAKEN_EVAL_ARTIFACT_DIR="$LOCAL_STORAGE_OUTPUT" \
  AWAKEN_LOCAL_STORAGE_COMMIT="$(git rev-parse HEAD)" \
  node e2e/k3d/local_storage_driver.mjs
