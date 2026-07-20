#!/usr/bin/env bash
# Build the production ACP sandbox image without assuming Cargo writes to ./target.
# The workspace uses a shared target mirror, so resolve the executable from Cargo's
# JSON stream and stage only that binary into the Docker build context.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../.." && pwd)
image=${1:-awaken-sandbox:local}
packages=${2:-"@agentclientprotocol/claude-agent-acp@0.44 @agentclientprotocol/codex-acp@1.1 @google/gemini-cli@0.11 opencode-ai@0.6"}
engine=${CONTAINER_ENGINE:-docker}
staged="$repo/deploy/images/sandbox/.awaken-sandbox.bin"
cleanup() { rm -f "$staged"; }
trap cleanup EXIT

cd "$repo"
bin=$(cargo build --release -p awaken-sandbox --message-format=json 2>/dev/null \
  | python3 -c "import sys,json
for line in sys.stdin:
    try: m=json.loads(line)
    except Exception: continue
    if m.get('reason') == 'compiler-artifact' and m.get('target', {}).get('name') == 'awaken-sandbox' and m.get('executable'):
        print(m['executable'])" \
  | tail -1)
[ -n "$bin" ] || { echo "could not resolve the awaken-sandbox binary" >&2; exit 1; }

cp "$bin" "$staged"
build_args=()
if [[ -n "${AWAKEN_SANDBOX_BUILD_NETWORK:-}" ]]; then
  build_args+=(--network "$AWAKEN_SANDBOX_BUILD_NETWORK")
fi
"$engine" build "${build_args[@]}" --build-arg ACP_NPM_PACKAGES="$packages" \
  -f deploy/images/sandbox/Dockerfile -t "$image" .
