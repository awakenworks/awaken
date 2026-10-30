#!/usr/bin/env bash
# One isolated production instance: shared K3D lifecycle/image helpers own
# infrastructure; managed_skill_lazy_real_e2e.mjs owns all journey assertions.
# P1 missing key/image/tool => reject before mutation. P2 fixture succeeds =>
# preserve evidence, delete only its unique cluster. P3 fails => same cleanup
# plus diagnostics. Existing clusters and Sessions never enter this owner.
set -euo pipefail
[[ "$#" -eq 0 ]] || { echo "usage: managed_skill_lazy_real_e2e.sh (configuration through environment variables)" >&2; exit 2; }
LAZY_REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$LAZY_REPO_ROOT"
source e2e/k3d/harness.sh
k3d_require_tools
[[ -n "${DEEPSEEK_API_KEY:-}${DEEPSEEK_API_KEY_FILE:-}" ]] || { echo "DeepSeek key required" >&2; exit 2; }
LAZY_SANDBOX_IMAGE="${AWAKEN_LAZY_SANDBOX_IMAGE:?immutable Sandbox image required}"
k3d_validate_image_digest "$LAZY_SANDBOX_IMAGE" || exit 2
LAZY_CLUSTER="awaken-skill-lazy-$$"
LAZY_POD="$LAZY_CLUSTER"
LAZY_CONTEXT="k3d-$LAZY_CLUSTER"
LAZY_PORT="${AWAKEN_LAZY_API_PORT:-18441}"
LAZY_PREWARM="${AWAKEN_LAZY_IMAGE_PREWARM:-0}"
[[ "$LAZY_PREWARM" = 0 || "$LAZY_PREWARM" = 1 ]] || { echo "AWAKEN_LAZY_IMAGE_PREWARM must be 0 or 1" >&2; exit 2; }
LAZY_STORAGE="${AWAKEN_LAZY_STORAGE_PROFILE:-local-path}"
[[ "$LAZY_STORAGE" = local-path || "$LAZY_STORAGE" = local-static ]] || { echo 'AWAKEN_LAZY_STORAGE_PROFILE must be local-path or local-static' >&2; exit 2; }
LAZY_COMMIT="$(git rev-parse HEAD)"
LAZY_RUNTIME_COMMIT="${AWAKEN_LAZY_RUNTIME_SOURCE_COMMIT:-$LAZY_COMMIT}"
[[ "$LAZY_RUNTIME_COMMIT" =~ ^[0-9a-f]{40}$ ]] || exit 2
LAZY_DIRECTORY_INIT="${AWAKEN_LAZY_DIRECTORY_INIT:-0}"
[[ "$LAZY_DIRECTORY_INIT" = 0 || "$LAZY_DIRECTORY_INIT" = 1 ]] || exit 2
LAZY_IMAGE="${AWAKEN_LAZY_RUNTIME_IMAGE:-awaken-skill-lazy:$LAZY_COMMIT}"
LAZY_OUTPUT="${AWAKEN_EVAL_ARTIFACT_DIR:-$LAZY_REPO_ROOT/target/test-evidence/skill-lazy-live}"
LAZY_TEMP="$(mktemp -d)"
LAZY_CREATED=0
LAZY_RUNTIME_CREATED=0
LAZY_DATA_VOLUME=""
LAZY_PRIOR_CONTEXT="$(kubectl config current-context 2>/dev/null || true)"
cleanup() {
  local status=$?
  if [[ "$LAZY_CREATED" = 1 ]]; then
    if [[ "$LAZY_RUNTIME_CREATED" = 1 ]]; then docker logs "$LAZY_POD" > "$LAZY_OUTPUT/service.log" 2>&1 || true; fi
    if [[ "$status" != 0 ]]; then
      kubectl --context "$LAZY_CONTEXT" get pods -A -o wide > "$LAZY_OUTPUT/pods.txt" 2>&1 || true
      kubectl --context "$LAZY_CONTEXT" -n awaken-system get events > "$LAZY_OUTPUT/kubernetes-events.txt" 2>&1 || true
    fi
  fi
  [[ "$LAZY_RUNTIME_CREATED" = 0 ]] || docker rm -f -v "$LAZY_POD" >/dev/null || status=1
  [[ -z "$LAZY_DATA_VOLUME" ]] || docker volume rm "$LAZY_DATA_VOLUME" >/dev/null || status=1
  if [[ "$LAZY_CREATED" = 1 ]]; then
    # Undo only this fixture's attachment to the pre-existing Registry. K3D
    # leaves that external container on the otherwise empty network; deleting
    # the Registry itself would affect other clusters. The shared deletion
    # owner below still verifies complete absence and reports any real failure.
    docker network disconnect "k3d-$LAZY_CLUSTER" k3d-awaken-registry.localhost 2>/dev/null || true
    if [[ "$LAZY_STORAGE" = local-static ]]; then
      kubectl --context "$LAZY_CONTEXT" delete namespace awaken-system awaken-storage --ignore-not-found --wait=true --timeout=90s >/dev/null 2>&1 || true
      docker exec "k3d-$LAZY_CLUSTER-server-0" sh -ec 'for path in /mnt/awaken-local/volume-*; do test -d "$path" || continue; umount "$path"; done' >/dev/null 2>&1 || true
    fi
    k3d_delete_cluster "$LAZY_CLUSTER" || status=1
  fi
  if [[ -n "$LAZY_PRIOR_CONTEXT" && "$(kubectl config current-context 2>/dev/null || true)" = "$LAZY_CONTEXT" ]]; then
    kubectl config use-context "$LAZY_PRIOR_CONTEXT" >/dev/null || status=1
  fi
  rm -rf "$LAZY_TEMP"
  exit "$status"
}
mkdir -p "$LAZY_OUTPUT"
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
if [[ -z "${AWAKEN_LAZY_RUNTIME_IMAGE:-}" ]]; then
  cargo build --release -p awaken-cli --bins -p awaken-worker --features awaken-cli/container-k8s,awaken-worker/container-k8s
  cp target/release/awaken "$LAZY_TEMP/awaken-server"
  cp target/release/awaken "$LAZY_TEMP/awaken"
  cp target/release/awaken-coordinator "$LAZY_TEMP/awaken-coordinator"
  cp target/release/awaken-worker "$LAZY_TEMP/awaken-worker"
  k3d_docker_build --load -f deploy/k3d/Dockerfile --build-arg AUXILIARY_EXECUTABLE=awaken \
    --label "org.opencontainers.image.revision=$LAZY_COMMIT" -t "$LAZY_IMAGE" "$LAZY_TEMP"
fi
[[ "$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$LAZY_IMAGE")" = "$LAZY_RUNTIME_COMMIT" ]] || { echo "runtime image does not match declared source commit" >&2; exit 2; }
if [[ -n "${AWAKEN_LAZY_UPGRADE_RUNTIME_IMAGE:-}" ]]; then
  [[ "$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$AWAKEN_LAZY_UPGRADE_RUNTIME_IMAGE")" = "$LAZY_COMMIT" ]] || { echo 'upgrade image does not match fixture commit' >&2; exit 2; }
fi
# Unique PID name is still checked before calling the shared create owner,
# whose normal scenario policy replaces its explicitly named previous fixture.
if k3d cluster list -o json | python3 -c 'import json,sys; name=sys.argv[1]; sys.exit(0 if any(v["name"] == name for v in json.load(sys.stdin)) else 1)' "$LAZY_CLUSTER"; then
  echo "unique test cluster already exists; refusing replacement" >&2; exit 2
fi
LAZY_CREATED=1
k3d_create_cluster "$LAZY_CLUSTER" 0 128Mi k3d-awaken-registry.localhost:5000 "$LAZY_IMAGE"
k3d_import_images "$LAZY_CLUSTER" "$LAZY_IMAGE"
k3d_import_storage_images "$LAZY_CLUSTER"
if [[ "$LAZY_STORAGE" = local-static ]]; then
  command -v helm >/dev/null
  k3d_install_local_storage "$LAZY_CLUSTER" "$LAZY_OUTPUT/storage" 8
fi
kubectl --context "$LAZY_CONTEXT" config view --minify --raw --flatten > "$LAZY_TEMP/kubeconfig"
export LAZY_POD LAZY_IMAGE LAZY_SANDBOX_IMAGE LAZY_TEMP LAZY_CONTEXT LAZY_PORT LAZY_PREWARM LAZY_STORAGE
python3 - <<'PY'
import json,os,subprocess
pod,image,sandbox=(os.environ[k] for k in ['LAZY_POD','LAZY_IMAGE','LAZY_SANDBOX_IMAGE'])
config='\n'.join([
 'data_dir = "/var/lib/awaken"', 'bind = "127.0.0.1:'+os.environ['LAZY_PORT']+'"', 'internal_bind = "127.0.0.1:'+str(int(os.environ['LAZY_PORT'])+1)+'"',
 'identity_mode = "no-login"', 'run_local_pool = true', 'no_browser = true', 'acp_clis = []',
 'sandbox_tier = "k8s"', 'k8s_namespace = "awaken-system"', 'worker_max_concurrent = 4',
 'k8s_continuation_volume = { storage_class_name = "local-path", size = "1Gi" }',
 'container_hand_residency = "resident"', 'container_image = '+json.dumps(sandbox),
 'admin_listen = \"127.0.0.1:'+str(int(os.environ['LAZY_PORT'])+2)+'\"',
 'control_seal_key = '+json.dumps(os.urandom(32).hex()),
])+'\n'
if os.environ['LAZY_STORAGE']=='local-static':config=config.replace('storage_class_name = "local-path"','storage_class_name = "awaken-local"')
if os.environ['LAZY_PREWARM']=='1':config+='k8s_image_prewarm = { max_images = 8 }\n'
# RBAC and network policy remain owned by the canonical product overlay.
rendered=subprocess.check_output(['kubectl','kustomize','deploy/k3d/product-backend'])
converted=subprocess.check_output(['kubectl','--context',os.environ['LAZY_CONTEXT'],'create','--dry-run=client','--validate=false','-f','-','-o','json'],input=rendered)
remaining=converted.decode(); base=[]; decoder=json.JSONDecoder()
while remaining.strip():
 item,end=decoder.raw_decode(remaining.lstrip());base.append(item);remaining=remaining.lstrip()[end:]
items=[item for item in base if item['kind'] in ['Namespace','ServiceAccount','Role','RoleBinding','ClusterRole','ClusterRoleBinding','NetworkPolicy']]
open(os.environ['LAZY_TEMP']+'/config.toml','w').write(config)
json.dump({'apiVersion':'v1','kind':'List','items':items},open(os.environ['LAZY_TEMP']+'/fixture.json','w'))
PY
kubectl --context "$LAZY_CONTEXT" apply -f "$LAZY_TEMP/fixture.json"
# The production AllInOne and its Worker use host networking; their filesystem
# is isolated in this owned container. Only Sandbox execution runs in K3D.
# This avoids inheriting a developer Docker bridge's DNS/egress failures while
# preserving the same production runtime, official SDK and Kubernetes provider.
LAZY_DATA_VOLUME="$(docker volume create --label awaken.test.case=skill-lazy-real --label awaken.e2e.owner=managed-skill-lazy-real "awaken-e2e-skill-lazy-$$")"
docker run --rm --network host --name "$LAZY_POD" --label awaken.test.case=skill-lazy-real \
  -v "$LAZY_TEMP/config.toml:/etc/awaken/config.toml:ro" -v "$LAZY_DATA_VOLUME:/var/lib/awaken" \
  --entrypoint /usr/local/bin/awaken "$LAZY_IMAGE" database migrate --config /etc/awaken/config.toml \
  --initialize-installation --initialization-reference "$LAZY_POD"
docker run -d --network host --name "$LAZY_POD" --label awaken.test.case=skill-lazy-real \
  -e KUBECONFIG=/etc/awaken/kubeconfig -v "$LAZY_TEMP/kubeconfig:/etc/awaken/kubeconfig:ro" \
  -v "$LAZY_TEMP/config.toml:/etc/awaken/config.toml:ro" -v "$LAZY_DATA_VOLUME:/var/lib/awaken" \
  --entrypoint /usr/local/bin/awaken "$LAZY_IMAGE" all-in-one --config /etc/awaken/config.toml >/dev/null
LAZY_RUNTIME_CREATED=1
AWAKEN_LAZY_TEST_URL="http://127.0.0.1:$LAZY_PORT" AWAKEN_LAZY_KUBE_CONTEXT="$LAZY_CONTEXT" \
AWAKEN_LAZY_SERVICE_CONTAINER="$LAZY_POD" AWAKEN_LAZY_SOURCE_COMMIT="$LAZY_COMMIT" \
AWAKEN_LAZY_RUNTIME_SOURCE_COMMIT="$LAZY_RUNTIME_COMMIT" AWAKEN_LAZY_DIRECTORY_INIT="$LAZY_DIRECTORY_INIT" \
AWAKEN_LAZY_STORAGE_PROFILE="$LAZY_STORAGE" \
AWAKEN_EVAL_ARTIFACT_DIR="$LAZY_OUTPUT" npm --prefix e2e run test:real:skill-lazy:driver
