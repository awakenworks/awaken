#!/usr/bin/env bash
# Build the production ACP sandbox image without assuming Cargo writes to ./target.
set -euo pipefail

run_with_deadline() {
  local seconds=$1
  shift
  local marker
  marker=$(mktemp "${TMPDIR:-/tmp}/awaken-sandbox-build-timeout.XXXXXX")
  rm -f "$marker"

  "$@" &
  local command_pid=$!
  (
    sleep "$seconds"
    if kill -0 "$command_pid" 2>/dev/null; then
      : >"$marker"
      kill -TERM "$command_pid" 2>/dev/null || true
      sleep 5
      kill -KILL "$command_pid" 2>/dev/null || true
    fi
  ) &
  local watchdog_pid=$!

  local status=0
  wait "$command_pid" || status=$?
  kill "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  if [[ -f "$marker" ]]; then
    rm -f "$marker"
    return 124
  fi
  rm -f "$marker"
  return "$status"
}

# Build-harness cause/effect graph and decision table:
# C1=external command completes before its deadline; C2=deadline expires while
# it is still active. E1=preserve the command's status; E2=terminate it and
# return the stable timeout status 124. H1 C1,!C2=>E1; H2 !C1,C2=>E2. These
# self-tests own both rules without invoking Cargo, Docker, or a package mirror.
if [[ ${1:-} == --self-test ]]; then
  run_with_deadline 2 bash -c 'exit 0'
  timeout_status=0
  run_with_deadline 1 bash -c 'sleep 30' || timeout_status=$?
  [[ $timeout_status -eq 124 ]] || {
    echo "sandbox image build deadline self-test: expected 124, got $timeout_status" >&2
    exit 1
  }
  echo "sandbox image build deadline self-test passed"
  exit 0
fi

repo=$(cd "$(dirname "$0")/../../.." && pwd)
image=${1:-awaken-sandbox:local}
# `${2-...}` intentionally distinguishes an omitted package list (production
# defaults) from an explicitly empty one (hermetic transport/E2E fixture image).
packages=${2-"@agentclientprotocol/claude-agent-acp@0.64.2 @agentclientprotocol/codex-acp@1.1.9 @google/gemini-cli@0.53.1 opencode-ai@1.18.12"}
hermes_package=${3-"hermes-agent[acp,bedrock]==0.19.0"}
# An explicitly empty legacy package list means a transport-only fixture image;
# preserve that contract unless the caller explicitly supplies a Hermes package.
if [[ -z "$packages" && $# -lt 3 ]]; then
  hermes_package=""
fi
engine=${CONTAINER_ENGINE:-docker}
build_timeout_seconds=${AWAKEN_SANDBOX_BUILD_TIMEOUT_SECONDS:-1800}
operation_timeout_seconds=${AWAKEN_SANDBOX_OPERATION_TIMEOUT_SECONDS:-60}
[[ $build_timeout_seconds =~ ^[1-9][0-9]*$ ]] || {
  echo "AWAKEN_SANDBOX_BUILD_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
}
[[ $operation_timeout_seconds =~ ^[1-9][0-9]*$ ]] || {
  echo "AWAKEN_SANDBOX_OPERATION_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
}

# A selected docker-container Buildx driver does not load `docker build` results
# into the local image store unless the output is explicit. The production-image
# acceptance command below must inspect the image that this invocation built,
# never a stale local tag or an unavailable BuildKit-only result.
build_image() {
  if [[ "${engine##*/}" == "docker" ]]; then
    run_with_deadline "$build_timeout_seconds" "$engine" build --load "$@"
  else
    run_with_deadline "$build_timeout_seconds" "$engine" build "$@"
  fi
}
staged="$repo/deploy/images/sandbox/.awaken-sandbox.bin"
cleanup() { rm -f "$staged"; }
trap cleanup EXIT

cd "$repo"
"$repo/deploy/images/sandbox/stage-binary.sh" "$staged" hand
if [[ -n "${AWAKEN_SANDBOX_BUILD_NETWORK:-}" ]]; then
  build_image --network "$AWAKEN_SANDBOX_BUILD_NETWORK" \
    --build-arg ACP_NPM_PACKAGES="$packages" \
    --build-arg HERMES_AGENT_PACKAGE="$hermes_package" \
    -f deploy/images/sandbox/Dockerfile -t "$image" .
else
  build_image --build-arg ACP_NPM_PACKAGES="$packages" \
    --build-arg HERMES_AGENT_PACKAGE="$hermes_package" \
    -f deploy/images/sandbox/Dockerfile -t "$image" .
fi

# Production-image acceptance decision table. Causes: C1 the caller uses any
# umask (including 077); C2 the image runs as UID 10001; C3 the staged binary is
# executable by that UID. Effect E1 the real image can start its hand-capable
# binary. Rule B1 C1+C2+C3=>E1; a staging regression fails the build here instead
# of surfacing later as a closed Session hand channel.
run_with_deadline "$operation_timeout_seconds" \
  "$engine" run --rm --entrypoint /usr/local/bin/awaken-sandbox "$image" hand --stdio </dev/null
run_with_deadline "$operation_timeout_seconds" \
  "$engine" run --rm --entrypoint /bin/sh "$image" -c \
  'command -v curl >/dev/null && curl --version >/dev/null'

# Verify exactly the adapter executables requested for this image. This catches
# package/bin drift before a Worker advertises an adapter and attempts a live
# ACP handshake inside Kubernetes.
expected_acp_commands=()
[[ "$packages" == *"@agentclientprotocol/claude-agent-acp"* ]] && expected_acp_commands+=(claude-agent-acp)
[[ "$packages" == *"@agentclientprotocol/codex-acp"* ]] && expected_acp_commands+=(codex-acp)
[[ "$packages" == *"@google/gemini-cli"* ]] && expected_acp_commands+=(gemini)
[[ "$packages" == *"opencode-ai"* ]] && expected_acp_commands+=(opencode)
[[ -n "$hermes_package" ]] && expected_acp_commands+=(hermes-acp)
if (( ${#expected_acp_commands[@]} > 0 )); then
  command_list=${expected_acp_commands[*]}
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /bin/sh "$image" -c \
    'for executable in $1; do command -v "$executable" >/dev/null || exit 1; done' \
    _ "$command_list"
fi
