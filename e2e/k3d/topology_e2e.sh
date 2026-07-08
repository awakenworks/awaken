#!/usr/bin/env bash
# Network-topology e2e (ADR-0044/0045) on a real k3d/k3s cluster.
#
# Deploys each brain/hand topology and drives a managed turn through the brain,
# asserting that `bash` executed in the HAND pod and its output round-tripped —
# proving 手腦分離 works across pods over TCP, not just in-process. Topologies:
#   - Direct  : the brain dials the hand's Service (hand listens).
#   - Reverse : the hand dials the brain's rendezvous (NAT / outbound-only hand).
#
# Requires: k3d, kubectl, docker (daemon up), rustc 1.96 (host build), node (e2e/).
# Usage: e2e/k3d/topology_e2e.sh [direct|reverse|both]   (default: both, from repo root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
CLUSTER="awaken-topo"
IMAGE="awaken-topology:latest"
NS="awaken-topo"
LOCAL_PORT="${TOPO_LOCAL_PORT:-38601}"
MARKER="REMOTE-HAND-OK-9f31"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
NODE="k3d-$CLUSTER-server-0"
WHICH="${1:-all}"
PF_PID=""

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }
ok()  { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  log "teardown: deleting k3d cluster $CLUSTER"
  k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  rm -f "$DEPLOY_DIR/awaken-server-local"
}
trap cleanup EXIT

load_image() {
  docker image inspect "$1" >/dev/null 2>&1 || docker pull -q "$1" >/dev/null
  docker save "$1" | docker exec -i "$NODE" ctr -n k8s.io images import - >/dev/null
}

# Drive one topology: apply its manifest, wait for the brain (its readiness proves
# the executor channel is up), port-forward, run a turn, assert the marker.
run_topology() {
  local name="$1" manifest="$2"
  # A fresh namespace per topology — full isolation, no stale Service endpoints or
  # DNS from a prior topology (which otherwise races the reverse rendezvous).
  local NS="awaken-topo-$name"
  log "topology '$name': apply $manifest (namespace $NS)"
  kubectl delete namespace "$NS" --wait=true >/dev/null 2>&1 || true
  kubectl create namespace "$NS" >/dev/null
  kubectl -n "$NS" apply -f "$DEPLOY_DIR/$manifest" >/dev/null
  echo "waiting for rollouts (brain readiness proves the $name channel)..."
  # Relay: the hand has no listening port (it dials the broker), so wait for its
  # pod to be Running and give it a moment to subscribe before the first request.
  if [ "$name" = relay ]; then
    kubectl -n "$NS" rollout status deploy/nats --timeout=120s || true
    kubectl -n "$NS" wait --for=condition=Ready pod -l app=hand --timeout=120s || true
    sleep 5
  fi
  if ! kubectl -n "$NS" rollout status deploy/brain --timeout=120s; then
    err "topology '$name': brain never became ready"
    kubectl -n "$NS" get pods -o wide || true
    kubectl -n "$NS" logs deploy/brain --tail=15 || true
    kubectl -n "$NS" logs deploy/hand --tail=15 || true
    return 1
  fi

  kubectl -n "$NS" port-forward svc/brain "$LOCAL_PORT":3000 >/tmp/topo_pf.log 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 30); do
    (exec 3<>"/dev/tcp/127.0.0.1/$LOCAL_PORT") 2>/dev/null && { exec 3>&- 3<&-; break; }
    sleep 1
  done

  local result
  result=$(cd "$REPO_ROOT/e2e" && MARKER="$MARKER" PORT="$LOCAL_PORT" node -e '
    import("@anthropic-ai/sdk").then(async (m)=>{
      const c=new m.default({apiKey:"x",baseURL:`http://127.0.0.1:${process.env.PORT}`});
      const B=["managed-agents-2026-04-01"];
      const s=await c.beta.sessions.create({agent:"assistant",environment_id:"env_local",betas:B});
      await c.beta.sessions.events.send(s.id,{events:[{type:"user.message",content:[{type:"text",text:"run the hand"}]}],betas:B});
      const evs=[]; for await(const e of c.beta.sessions.events.list(s.id,{betas:B})) evs.push(e);
      const tool=evs.some(e=>e.type==="agent.tool_use");
      const msgs=evs.filter(e=>e.type==="agent.message").map(e=>(e.content||[]).map(x=>x.text||"").join(""));
      const okk=tool && msgs.some(x=>x.includes(process.env.MARKER));
      console.log(okk?"PASS":("FAIL "+JSON.stringify(evs.map(e=>e.type))));
      process.exit(okk?0:1);
    })' 2>&1 | tail -1)
  kill "$PF_PID" 2>/dev/null || true; PF_PID=""

  if [ "$result" = "PASS" ]; then
    ok "topology '$name' PASS: brain pod ran bash on the hand pod over the cluster network; marker round-tripped."
    return 0
  else
    err "topology '$name' FAIL: $result"
    kubectl -n "$NS" logs deploy/brain --tail=15 || true
    return 1
  fi
}

log "1/5 build the server binary on the host (rustc 1.96)"
RUSTUP_TOOLCHAIN=1.96.0 cargo build -q -p awaken-server-local --bin awaken-server-local
BIN=$(RUSTUP_TOOLCHAIN=1.96.0 cargo build -p awaken-server-local --bin awaken-server-local --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-server-local': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { echo 'could not resolve binary'; exit 1; }
cp "$BIN" "$DEPLOY_DIR/awaken-server-local"

log "2/5 build the topology image (copy-in, no in-container rust build)"
docker build --load -q -t "$IMAGE" -f "$DEPLOY_DIR/Dockerfile" "$DEPLOY_DIR" >/dev/null

log "3/5 create k3d cluster $CLUSTER (single node)"
k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
k3d cluster create "$CLUSTER" --agents 0 --wait --timeout 180s >/dev/null

log "4/5 side-load images into the node's containerd (offline cluster)"
# The cluster nodes have no outbound internet, so containerd cannot pull ANY image
# (app, pod-sandbox 'pause', or CoreDNS). The host does, so pull there and stream
# each image into the node's k8s.io containerd namespace. CoreDNS must be loaded
# too, else the brain's Service-DNS lookup of the hand hangs.
PAUSE_IMG=$(docker exec "$NODE" sh -c 'grep -hoE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null)
PAUSE_IMG=${PAUSE_IMG:-rancher/mirrored-pause:3.6}
COREDNS_IMG=$(kubectl -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
COREDNS_IMG=${COREDNS_IMG:-rancher/mirrored-coredns-coredns:1.10.1}
load_image "$PAUSE_IMG"
load_image "$COREDNS_IMG"
load_image "$IMAGE"
load_image nats:2   # the Relay topology's broker
kubectl -n kube-system delete pod -l k8s-app=kube-dns >/dev/null 2>&1 || true
kubectl -n kube-system rollout status deploy/coredns --timeout=90s
kubectl create namespace "$NS" >/dev/null 2>&1 || true

log "5/5 run topologies: $WHICH"
FAILED=0
case "$WHICH" in
  direct)  run_topology direct  topology-direct.yaml  || FAILED=1 ;;
  reverse) run_topology reverse topology-reverse.yaml || FAILED=1 ;;
  relay)   run_topology relay   topology-relay.yaml   || FAILED=1 ;;
  both)    run_topology direct  topology-direct.yaml  || FAILED=1
           run_topology reverse topology-reverse.yaml || FAILED=1 ;;
  *)       run_topology direct  topology-direct.yaml  || FAILED=1
           run_topology reverse topology-reverse.yaml || FAILED=1
           run_topology relay   topology-relay.yaml   || FAILED=1 ;;
esac

if [ "$FAILED" = 0 ]; then
  ok "\nK3D TOPOLOGY E2E PASS: all requested topologies ($WHICH) executed tools on the remote hand over the cluster network."
  exit 0
else
  err "\nK3D TOPOLOGY E2E FAIL"
  exit 1
fi
