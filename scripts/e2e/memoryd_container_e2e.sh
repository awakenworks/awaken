#!/usr/bin/env bash
# Real-container memoryd sidecar e2e (ADR-0053): build the memoryd image and prove the
# copy realization end-to-end — the harvest→persist→re-materialize round-trip — inside
# a REAL Docker container, over the exact env contract the k8s `pod_plan` sets on the
# `memoryd-<i>` sidecar (AWAKEN_MEMORY_STORE_ID / _MOUNT_PATH / _MEMORY_STORE_DIR /
# _MEMORY_MODE). This exercises the actual image ENTRYPOINT (`awaken-sandbox memoryd`),
# the role's SIGTERM harvest, and the store-owned sqlite persistence across a container
# restart — none of which the in-process copy-cycle unit test covers.
#
#   run1: empty store + MNT1 → materialize 0; write a file; docker stop → SIGTERM harvest
#   run2: SAME store + a FRESH MNT2 → materialize re-creates the file from the store
#
# Proving the file re-appears in a fresh mount proves the sidecar harvested it to durable
# truth AND re-materializes it — the sidecar's whole reason to exist. The FUSE mode needs
# /dev/fuse + SYS_ADMIN (a node capability); the copy mode is portable, so this is the
# reproducible-anywhere proof. Self-skips when Docker is unreachable.
#
# Run: scripts/e2e/memoryd_container_e2e.sh
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
IMAGE="awaken-memoryd:e2e"
MARKER="memoryd-roundtrip-ok-7c31"

ok()  { echo -e "\033[1;32m$*\033[0m"; }
err() { echo -e "\033[1;31m$*\033[0m"; }

if ! docker version >/dev/null 2>&1; then
  echo "E2E SKIP: no reachable Docker daemon."
  exit 0
fi

log() { echo -e "\n\033[1;36m== $* ==\033[0m"; }

log "1/4 build awaken-sandbox --features memoryd (host build, copied into the image)"
BIN=$(cargo build --message-format=json -p awaken-sandbox --bin awaken-sandbox --features memoryd 2>/dev/null \
  | python3 -c "import sys,json
for l in sys.stdin:
 try:
  m=json.loads(l)
  if m.get('executable') and m.get('target',{}).get('name')=='awaken-sandbox': print(m['executable'])
 except Exception: pass" | tail -1)
[ -n "$BIN" ] || { err 'could not resolve the awaken-sandbox binary'; exit 1; }

log "2/4 build the memoryd image"
CTX=$(mktemp -d)
cp "$BIN" "$CTX/awaken-sandbox"
cp deploy/images/sandbox/Dockerfile.memoryd "$CTX/Dockerfile"
docker build -q --build-arg BIN=awaken-sandbox -t "$IMAGE" "$CTX" >/dev/null
rm -rf "$CTX"

STORE=$(mktemp -d); MNT1=$(mktemp -d); MNT2=$(mktemp -d)
chmod 777 "$STORE" "$MNT1" "$MNT2"
cleanup() {
  docker rm -f mmd-e2e-1 mmd-e2e-2 >/dev/null 2>&1 || true
  # The sidecar (root in-container) writes root-owned files into the mounts; drop them
  # via a throwaway container so the host `rm` (unprivileged) can remove the dirs.
  docker run --rm -v "$STORE:/s" -v "$MNT1:/m1" -v "$MNT2:/m2" busybox:latest \
    sh -c 'rm -rf /s/* /m1/* /m2/* 2>/dev/null' >/dev/null 2>&1 || true
  rm -rf "$STORE" "$MNT1" "$MNT2" 2>/dev/null || true
}
trap cleanup EXIT

run_memoryd() { # run_memoryd <name> <mount-host-dir>
  docker run -d --name "$1" -v "$STORE:/store" -v "$2:/mnt" \
    -e AWAKEN_MEMORY_STORE_ID=s -e AWAKEN_MOUNT_PATH=/mnt \
    -e AWAKEN_MEMORY_STORE_DIR=/store -e AWAKEN_MEMORY_MODE=copy \
    "$IMAGE" >/dev/null
}

log "3/4 run1: materialize empty, write a memory file, SIGTERM → harvest to the store"
run_memoryd mmd-e2e-1 "$MNT1"
sleep 2
if [ "$(docker inspect -f '{{.State.Running}}' mmd-e2e-1 2>/dev/null)" != "true" ]; then
  err "the memoryd container failed to start:"; docker logs mmd-e2e-1 2>&1 | tail -5; exit 1
fi
mkdir -p "$MNT1/notes"
printf '%s\n' "$MARKER" > "$MNT1/notes/todo.md"
docker stop mmd-e2e-1 >/dev/null   # SIGTERM → the role harvests before exit
docker logs mmd-e2e-1 2>&1 | grep -i "harvested" || { err "no harvest log — the SIGTERM harvest did not run"; docker logs mmd-e2e-1 2>&1 | tail -5; exit 1; }
docker rm -f mmd-e2e-1 >/dev/null

log "4/4 run2: SAME store, a FRESH mount → the file re-materializes from durable truth"
run_memoryd mmd-e2e-2 "$MNT2"
sleep 2
got=$(cat "$MNT2/notes/todo.md" 2>/dev/null || true)
docker rm -f mmd-e2e-2 >/dev/null

if [ "$got" = "$MARKER" ]; then
  ok "\nMEMORYD CONTAINER E2E PASS: the memoryd sidecar harvested a memory to the store and re-materialized it into a fresh mount across a container restart (ADR-0053, real Docker)."
  exit 0
else
  err "\nMEMORYD CONTAINER E2E FAIL: expected '$MARKER' in the re-materialized mount, got '$got'"
  exit 1
fi
