#!/usr/bin/env bash
# Network-topology e2e (ADR-0044/0045) on a real k3d/k3s cluster.
#
# Deploys the Direct topology — a BRAIN pod that routes every tool call to a HAND
# pod over the cluster network — then drives a managed turn through the brain and
# asserts that `bash` executed in the HAND pod and its output round-tripped. This
# proves 手腦分離 works across pods over TCP, not just in-process.
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/topology_e2e.sh   (from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
CLUSTER="awaken-topo"
IMAGE="awaken-topology:latest"
NS="awaken-topo"
LOCAL_PORT="${TOPO_LOCAL_PORT:-38601}"
MARKER="REMOTE-HAND-OK-9f31"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
PF_PID=""

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }

cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  rm -f "$DEPLOY_DIR/awaken-server-local"
}
trap cleanup EXIT

log "1/7 build the server binary on the host (rustc 1.96)"
RUSTUP_TOOLCHAIN=1.96.0 cargo build -q -p awaken-server-local --bin awaken-server-local
BIN=$(RUSTUP_TOOLCHAIN=1.96.0 cargo build -p awaken-server-local --bin awaken-server-local --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-server-local': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { echo "could not resolve binary"; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server-local"

log "2/7 build the topology image (copy-in, no in-container rust build)"
# --load: with the buildx docker-container driver the result otherwise stays in
# the build cache only and never lands in the daemon for k3d to import.
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null

log "3/7 create k3d cluster $CLUSTER (single node)"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
k3d cluster create "$CLUSTER" --agents 0 --wait --timeout 180s >/dev/null
NODE="k3d-$CLUSTER-server-0"

log "4/7 load images into the node's containerd (offline cluster)"
# The cluster nodes have NO outbound internet, so containerd cannot pull ANY image
# — not the app, not the pod-sandbox (pause), not CoreDNS. The host DOES have
# internet, so we pull there and stream each image straight into the node's k8s.io
# containerd namespace via `ctr import`. (`k3d image import` mis-handles the
# pause/coredns layers, so use ctr directly.) Without CoreDNS the brain's Service
# DNS lookup of `hand` would hang, so CoreDNS must be loaded too.
load_image() {
  docker image inspect "$1" >/dev/null 2>&1 || docker pull -q "$1" >/dev/null
  docker save "$1" | docker exec -i "$NODE" ctr -n k8s.io images import - >/dev/null
}
PAUSE_IMG=$(docker exec "$NODE" sh -c 'grep -oE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null)
PAUSE_IMG=${PAUSE_IMG:-rancher/mirrored-pause:3.6}
COREDNS_IMG=$(kubectl -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
COREDNS_IMG=${COREDNS_IMG:-rancher/mirrored-coredns-coredns:1.10.1}
load_image "$PAUSE_IMG"
load_image "$COREDNS_IMG"
load_image "$IMAGE"
# Restart CoreDNS so it picks up the now-local image, and wait for cluster DNS.
kubectl -n kube-system delete pod -l k8s-app=kube-dns >/dev/null 2>&1 || true
kubectl -n kube-system rollout status deploy/coredns --timeout=90s

log "5/7 apply the Direct-topology manifests"
kubectl create namespace "$NS" >/dev/null 2>&1 || true
kubectl -n "$NS" apply -f "$DEPLOY_DIR/topology-direct.yaml" >/dev/null
echo "waiting for hand + brain rollouts..."
diag() {
  echo "--- DIAGNOSTICS ($1) ---"
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" describe pod -l "app=$1" | sed -n '/Events:/,$p' | tail -20 || true
  kubectl -n "$NS" logs -l "app=$1" --tail=20 --prefix || true
}
if ! kubectl -n "$NS" rollout status deploy/hand --timeout=90s; then diag hand; exit 1; fi
if ! kubectl -n "$NS" rollout status deploy/brain --timeout=90s; then diag brain; diag hand; exit 1; fi

log "6/7 port-forward the brain Service and drive a managed turn"
kubectl -n "$NS" port-forward svc/brain "$LOCAL_PORT":3000 >/tmp/topo_pf.log 2>&1 &
PF_PID=$!
for i in $(seq 1 30); do
  curl -s "http://127.0.0.1:$LOCAL_PORT/health" >/dev/null 2>&1 && break || true
  # /health may not exist; a TCP connect success is enough for the SDK.
  (exec 3<>"/dev/tcp/127.0.0.1/$LOCAL_PORT") 2>/dev/null && { exec 3>&- 3<&-; break; }
  sleep 1
done

RESULT=$(cd "$REPO_ROOT/e2e" && MARKER="$MARKER" PORT="$LOCAL_PORT" node -e '
import("@anthropic-ai/sdk").then(async (m)=>{
  const Anthropic=m.default;
  const c=new Anthropic({apiKey:"x",baseURL:`http://127.0.0.1:${process.env.PORT}`});
  const B=["managed-agents-2026-04-01"];
  const s=await c.beta.sessions.create({agent:"assistant",environment_id:"env_local",betas:B});
  await c.beta.sessions.events.send(s.id,{events:[{type:"user.message",content:[{type:"text",text:"run the hand"}]}],betas:B});
  const evs=[]; for await(const e of c.beta.sessions.events.list(s.id,{betas:B})) evs.push(e);
  const toolUse=evs.some(e=>e.type==="agent.tool_use");
  const msgs=evs.filter(e=>e.type==="agent.message").map(e=>(e.content||[]).map(x=>x.text||"").join(""));
  const ok=toolUse && msgs.some(x=>x.includes(process.env.MARKER));
  console.log(ok?"PASS":("FAIL "+JSON.stringify(msgs)));
  process.exit(ok?0:1);
});' )
echo "brain turn result: $RESULT"

log "7/7 verify the tool actually executed in the HAND pod (not the brain)"
HAND_POD=$(kubectl -n "$NS" get pod -l app=hand -o jsonpath='{.items[0].metadata.name}')
echo "hand pod: $HAND_POD"
kubectl -n "$NS" logs "deploy/hand" | tail -3 || true

if [ "$RESULT" = "PASS" ]; then
  echo -e "\n\033[1;32mK3D TOPOLOGY E2E PASS: Direct topology — brain pod ran bash on the hand pod over the cluster network; marker round-tripped.\033[0m"
  exit 0
else
  echo -e "\n\033[1;31mK3D TOPOLOGY E2E FAIL\033[0m"
  echo "--- brain logs ---"; kubectl -n "$NS" logs deploy/brain | tail -20 || true
  echo "--- hand logs ---"; kubectl -n "$NS" logs deploy/hand | tail -20 || true
  exit 1
fi
