#!/usr/bin/env bash
# C5 slice-1 FULL acceptance: run the REAL `awaken-sandbox hand` inside a
# `--network none` container, and have a brain on the host run a real `bash` tool on it
# over a unix-socket bind-mount rendezvous. This combines the two proofs — the transport
# crosses the network-denied boundary (hand_docker) AND the hand serves real tools
# (hand_role) — into the end-to-end the C5 acceptance asks for.
#
# The brain client speaks the hand wire directly (length-delimited JSON: a 4-byte
# big-endian length prefix + a serde `HandRequest`), so no Rust brain crate is needed.
# Self-skips when Docker is unreachable. Run: bash deploy/images/sandbox/hand-smoke.sh
set -euo pipefail
cd "$(dirname "$0")/../../.."

if ! docker version >/dev/null 2>&1; then
  echo "HAND-SMOKE SKIP: no reachable Docker daemon"
  exit 0
fi
if ! command -v python3 >/dev/null; then
  echo "HAND-SMOKE SKIP: no python3 for the brain client"
  exit 0
fi

# Build the hand-featured binary and stage it into the build context.
staged="deploy/images/sandbox/.awaken-sandbox-hand.bin"
bin=$(cargo build --release -p awaken-sandbox --features hand --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for line in sys.stdin:
    try: m=json.loads(line)
    except Exception: continue
    if m.get('reason')=='compiler-artifact' and m.get('target',{}).get('name')=='awaken-sandbox' and m.get('executable'):
        print(m['executable'])" | tail -1)
[ -n "$bin" ] || { echo "HAND-SMOKE FAIL: could not resolve the hand binary"; exit 1; }

rv=$(mktemp -d /tmp/awaken-hand-rv.XXXXXX)
name="awaken-hand-smoke-$$"
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; rm -f "$staged"; rm -rf "$rv"; }
trap cleanup EXIT
docker rm -f "$name" >/dev/null 2>&1 || true

cp "$bin" "$staged"
docker build -q -f deploy/images/sandbox/Dockerfile.hand-smoke \
  --build-arg BIN="$staged" -t awaken-sandbox-hand-smoke:test . >/dev/null

# Run the REAL hand under --network none with the rendezvous bind-mount.
docker run -d --name "$name" --network none -v "$rv:/rv" awaken-sandbox-hand-smoke:test >/dev/null

# Brain on the host: dial the unix socket and run a bash tool over the hand wire.
got=$(python3 - "$rv/hand.sock" <<'PY'
import socket, struct, json, sys, time
sock_path = sys.argv[1]
req = json.dumps({
    "correlation_id": 1,
    "call": {"call_id": "c1", "tool_id": "bash",
             "arguments": {"command": "echo hand-in-a-denied-container"}},
}).encode()
deadline = time.time() + 15
s = None
while time.time() < deadline:
    try:
        s = socket.socket(socket.AF_UNIX); s.connect(sock_path); break
    except OSError:
        s = None; time.sleep(0.2)
if s is None:
    print("NO_SOCKET"); sys.exit(0)
s.sendall(struct.pack(">I", len(req)) + req)
hdr = b""
while len(hdr) < 4: hdr += s.recv(4 - len(hdr))
n = struct.unpack(">I", hdr)[0]
payload = b""
while len(payload) < n: payload += s.recv(n - len(payload))
print(payload.decode("utf-8", "replace"))
PY
)

echo "reply: ${got}"
case "$got" in
  *hand-in-a-denied-container*)
    echo "HAND-SMOKE PASS: real hand ran a bash tool in a --network none container over the unix rendezvous" ;;
  NO_SOCKET) echo "HAND-SMOKE FAIL: brain never reached the hand's unix socket"; exit 1 ;;
  *) echo "HAND-SMOKE FAIL: unexpected hand reply: ${got}"; exit 1 ;;
esac
