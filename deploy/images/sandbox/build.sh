#!/usr/bin/env bash
# Build the production ACP sandbox image without assuming Cargo writes to ./target.
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

cd "$repo"
"$repo/deploy/images/sandbox/stage-binary.sh" "$staged" hand
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

# Production-image acceptance decision table. Causes: C1 the caller uses any
# umask (including 077); C2 the image runs as UID 10001; C3 the staged binary is
# executable by that UID. Effect E1 the real image can start its hand-capable
# binary. Rule B1 C1+C2+C3=>E1; a staging regression fails the build here instead
# of surfacing later as a closed Session hand channel.
"$engine" run --rm --entrypoint /usr/local/bin/awaken-sandbox "$image" hand --stdio </dev/null
