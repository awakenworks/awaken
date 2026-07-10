#!/usr/bin/env bash
# Real-exec e2e for K8sSandboxProvider (B-P5a): provisions a k3d cluster and runs the
# gated `k8s_e2e` test against it, so the declarative Pod path is verified with REAL
# process exec (create → exec exit codes → adopt → dispose), not just structurally.
#
# The k3d node's containerd cannot reach docker.io in this environment (registry
# i/o-timeout), so — like the awaken-cloud k3d e2e — a local k3d registry mirrors
# docker.io: the sandbox's alpine:3 (and the k3s pause/coredns/local-path images) are
# pushed there and pulled from the mirror. Requires: k3d, kubectl, docker, rustc 1.96.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
CLUSTER="awaken-sbx"
REG="awaken-sbx-reg"
REG_PORT=5345
REG_CFG="$(mktemp)"
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.96.0}"

cleanup() {
  echo "== teardown =="
  k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  k3d registry delete "$REG" >/dev/null 2>&1 || true
  rm -f "$REG_CFG"
}
trap cleanup EXIT

# Push <ref> into the registry under its path (mirror resolves docker.io/<path>).
reg_push() {
  docker image inspect "$1" >/dev/null 2>&1 || docker pull -q "$1" >/dev/null
  local path="${1#docker.io/}"
  case "$path" in */*) ;; *) path="library/$path" ;; esac
  docker tag "$1" "localhost:$REG_PORT/$path"
  docker push -q "localhost:$REG_PORT/$path" >/dev/null
}

echo "== 1/4 create registry + push images (alpine:3 + k3s system) =="
k3d registry delete "$REG" >/dev/null 2>&1 || true
k3d registry create "$REG" --port "$REG_PORT" >/dev/null
reg_push alpine:3
reg_push rancher/mirrored-pause:3.6
reg_push rancher/mirrored-coredns-coredns:1.10.1
reg_push rancher/local-path-provisioner:v0.0.28 || true

cat > "$REG_CFG" <<YAML
mirrors:
  "docker.io":
    endpoint: ["http://k3d-$REG:5000"]
configs:
  "k3d-$REG:5000":
    tls:
      insecure_skip_verify: true
YAML

echo "== 2/4 create cluster (docker.io → mirror) =="
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
k3d cluster create "$CLUSTER" --agents 0 \
  --registry-use "k3d-$REG:$REG_PORT" --registry-config "$REG_CFG" \
  --wait --timeout 150s >/dev/null
CTX="k3d-$CLUSTER"
kubectl --context "$CTX" -n kube-system rollout status deploy/coredns --timeout=120s

echo "== 3/4 run the K8sSandboxProvider real-exec test =="
AWAKEN_TEST_K8S_CONTEXT="$CTX" \
  cargo test -p awaken-sandbox-k8s --test k8s_e2e -- --nocapture

echo "== 4/4 =="
echo "SANDBOX K8S E2E PASS: create → exec (exit codes) → adopt → dispose over real k3d Pods."
