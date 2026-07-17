#!/usr/bin/env bash
# Smoke-test the awaken-sandbox `acp` bridge as a REAL container entrypoint: build a
# minimal image (host-built binary + ubuntu, no ACP CLIs), run it with a fake stdio
# "CLI" as the container Cmd, dial the published port, and prove the bridge piped the
# wire. This validates the ENTRYPOINT(bridge) + Cmd(cli-argv) composition the
# production sandbox image relies on (docker sets Cmd, keeps the image ENTRYPOINT).
#
# Self-skips when Docker is unreachable. Run: bash deploy/images/sandbox/smoke.sh
set -euo pipefail
cd "$(dirname "$0")/../../.."

if ! docker version >/dev/null 2>&1; then
  echo "SMOKE SKIP: no reachable Docker daemon"
  exit 0
fi

# The workspace may use a custom CARGO_TARGET_DIR (not ./target), so resolve the real
# binary path from cargo's JSON output and stage it INTO the build context.
staged="deploy/images/sandbox/.awaken-sandbox.bin"
bin=$(cargo build --release -p awaken-sandbox --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for line in sys.stdin:
    try: m=json.loads(line)
    except Exception: continue
    if m.get('reason')=='compiler-artifact' and m.get('target',{}).get('name')=='awaken-sandbox' and m.get('executable'):
        print(m['executable'])" | tail -1)
[ -n "$bin" ] || { echo "SMOKE FAIL: could not resolve the awaken-sandbox binary"; exit 1; }

name="awaken-sandbox-smoke-$$"
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; rm -f "$staged"; }
trap cleanup EXIT
docker rm -f "$name" >/dev/null 2>&1 || true   # pre-clean any leftover container only

cp "$bin" "$staged"                             # stage AFTER the pre-clean
docker build -q -f deploy/images/sandbox/Dockerfile.smoke \
  --build-arg BIN="$staged" -t awaken-sandbox-smoke:test . >/dev/null

# Cmd = a fake stdio "CLI" (read one line, reply, exit). The bridge ENTRYPOINT spawns
# it and bridges its stdio to the dialed socket.
docker run -d --name "$name" -p 127.0.0.1::8080 awaken-sandbox-smoke:test \
  sh -c 'read line; printf "reply:%s\n" "$line"' >/dev/null
port=$(docker port "$name" 8080/tcp | head -1 | sed 's/.*://')

got=""
for _ in $(seq 1 50); do
  # bash /dev/tcp connects only once the bridge has bound (else connection refused).
  if exec 3<>"/dev/tcp/127.0.0.1/${port}" 2>/dev/null; then
    printf 'hello\n' >&3
    got=$(timeout 3 head -c 64 <&3 || true)
    exec 3>&- 3<&-
    [ -n "$got" ] && break
  fi
  sleep 0.2
done

echo "got: ${got}"
case "$got" in
  *reply:hello*) echo "SMOKE PASS: bridge entrypoint piped the wire in a real container" ;;
  *) echo "SMOKE FAIL: expected 'reply:hello', got '${got}'"; exit 1 ;;
esac
