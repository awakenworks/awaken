#!/usr/bin/env bash
# One pinned upstream chart is the manifest owner; this wrapper owns admission
# and rendering only. Node disks are prepared by infrastructure, never Runtime.
set -euo pipefail
[[ "$#" = 3 && "$1" = --context ]] || { echo 'usage: install.sh --context CONTEXT OUTPUT_DIRECTORY' >&2; exit 2; }
LOCAL_PV_CONTEXT="$2"
LOCAL_PV_OUTPUT="$3"
[[ -n "$LOCAL_PV_CONTEXT" && "$LOCAL_PV_CONTEXT" != -* && -n "$LOCAL_PV_OUTPUT" ]] || exit 2
LOCAL_PV_ROOT="$(cd "$(dirname "$0")" && pwd)"
command -v helm >/dev/null
mkdir -p "$LOCAL_PV_OUTPUT"
helm template awaken-local-storage "$LOCAL_PV_ROOT/chart" --namespace awaken-storage \
  -f "$LOCAL_PV_ROOT/values.yaml" > "$LOCAL_PV_OUTPUT/resources.yaml"
# Reuse a matching deployment only. This storage installer must not adopt a
# foreign StorageClass or redirect an existing class to different volume paths.
kubectl --context "$LOCAL_PV_CONTEXT" get storageclass awaken-local --ignore-not-found -o json > "$LOCAL_PV_OUTPUT/existing-class.json"
if [[ -s "$LOCAL_PV_OUTPUT/existing-class.json" ]]; then
  python3 - "$LOCAL_PV_OUTPUT/existing-class.json" <<'PYCLASS'
import json,sys
v=json.load(open(sys.argv[1]));labels=v['metadata'].get('labels',{})
if labels.get('app.kubernetes.io/instance')!='awaken-local-storage' or v['provisioner']!='kubernetes.io/no-provisioner' or v.get('volumeBindingMode')!='WaitForFirstConsumer' or v.get('reclaimPolicy')!='Delete':raise SystemExit('foreign/conflicting awaken-local StorageClass')
PYCLASS
fi
kubectl --context "$LOCAL_PV_CONTEXT" create namespace awaken-storage --dry-run=client -o yaml \
  | kubectl --context "$LOCAL_PV_CONTEXT" apply --server-side -f - >/dev/null
kubectl --context "$LOCAL_PV_CONTEXT" apply --server-side --field-manager=awaken-local-storage -f "$LOCAL_PV_OUTPUT/resources.yaml" >/dev/null
kubectl --context "$LOCAL_PV_CONTEXT" -n awaken-storage rollout status daemonset/awaken-local-storage --timeout=120s
