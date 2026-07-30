#!/usr/bin/env bash
# Build the production ACP sandbox image without assuming Cargo writes to ./target.
# The workspace uses a shared target mirror, so resolve the executable from Cargo's
# JSON stream and stage only that binary into the Docker build context.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../.." && pwd)
image=${1:-awaken-sandbox:local}
# `${2-...}` intentionally distinguishes an omitted package list (production
# defaults) from an explicitly empty one (hermetic transport/E2E fixture image).
packages=${2-"@agentclientprotocol/claude-agent-acp@0.44 @agentclientprotocol/codex-acp@1.1 @google/gemini-cli@0.11 opencode-ai@0.6"}
engine=${CONTAINER_ENGINE:-docker}
staged="$repo/deploy/images/sandbox/.awaken-sandbox.bin"
cleanup() { rm -f "$staged"; }
trap cleanup EXIT

if command -v python3 >/dev/null 2>&1 && python3 -c 'import sys' >/dev/null 2>&1; then
  build_python=python3
elif command -v python >/dev/null 2>&1 && python -c 'import sys' >/dev/null 2>&1; then
  build_python=python
else
  echo "a working Python 3 interpreter is required" >&2
  exit 1
fi

cd "$repo"
bin=$(cargo build --release -p awaken-sandbox --features hand --message-format=json 2>/dev/null \
  | "$build_python" -c "import sys,json
for line in sys.stdin:
    try: m=json.loads(line)
    except Exception: continue
    if m.get('reason') == 'compiler-artifact' and m.get('target', {}).get('name') == 'awaken-sandbox' and m.get('executable'):
        print(m['executable'])" \
  | tail -1)
[ -n "$bin" ] || { echo "could not resolve the awaken-sandbox binary" >&2; exit 1; }

cp "$bin" "$staged"
if [[ -n "${AWAKEN_SANDBOX_BUILD_NETWORK:-}" ]]; then
  "$engine" build --network "$AWAKEN_SANDBOX_BUILD_NETWORK" \
    --build-arg ACP_NPM_PACKAGES="$packages" \
    -f deploy/images/sandbox/Dockerfile -t "$image" .
else
  # macOS still ships Bash 3.2, where expanding an empty array under `set -u`
  # raises "unbound variable". Keep the zero-argument case explicit so the
  # hermetic image path works on every supported host shell.
  "$engine" build --build-arg ACP_NPM_PACKAGES="$packages" \
    -f deploy/images/sandbox/Dockerfile -t "$image" .
fi
