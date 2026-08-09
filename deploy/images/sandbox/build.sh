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

ensure_existing=none
if [[ ${1:-} == --ensure || ${1:-} == --ensure-hand ]]; then
  ensure_existing=${1#--ensure}
  ensure_existing=${ensure_existing#-}
  ensure_existing=${ensure_existing:-full}
  shift
fi

repo=$(cd "$(dirname "$0")/../../.." && pwd)
image=${1:-awaken-sandbox:local}
# `${2-all}` intentionally distinguishes an omitted runtime set (the complete
# production contract) from an explicitly empty one (a hermetic transport/E2E
# fixture). A non-empty override is a comma-separated subset of catalog ids.
runtime_ids=${2-all}
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

# Image-availability FMECA decision table. C1=the selected tag exists;
# C2=it carries the production environment label; C3=its real Hand entry point
# starts; C4=curl exists. E1=reuse exactly that image; E2=build through this
# authoritative script, then require the same acceptance checks.
#
# | Rule | mode | C1 | C2 | C3 | C4 | Effect |
# |---|---|---|---|---|---|---|
# | I1 | build | - | - | - | - | E2, then full acceptance |
# | I2 | ensure | T | T | T | T | E1 |
# | I3 | ensure | otherwise | | | | E2, then full acceptance |
# | I4 | ensure-hand | T | - | T | - | E1 for the Hand-only E2E fixture |
# | I5 | ensure-hand | otherwise | | | | E2, then full acceptance |
accept_hand() {
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /usr/local/bin/awaken-sandbox "$image" hand --stdio </dev/null
}

accept_image() {
  [[ $($engine image inspect --format '{{index .Config.Labels "org.awaken.environment-packages"}}' "$image" 2>/dev/null) == 1 ]] || return 1
  accept_hand || return 1
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /bin/sh "$image" -c \
    'command -v curl >/dev/null && curl --version >/dev/null'
}

if { [[ $ensure_existing == full ]] && accept_image; } \
  || { [[ $ensure_existing == hand ]] && accept_hand; }; then
  echo "reusing accepted sandbox image: $image"
  exit 0
fi

staged="$repo/deploy/images/sandbox/.awaken-sandbox.bin"
generated_contract="$repo/deploy/images/sandbox/.acp-runtimes.generated.json"
cleanup() { rm -f "$staged" "$generated_contract"; }
trap cleanup EXIT

cd "$repo"
"$repo/deploy/images/sandbox/stage-binary.sh" "$staged" hand
# The Rust ACP catalog is the only package/argv/auth authority. Generate its
# image projection immediately before the build so Docker cannot consume a
# stale hand-maintained mirror.
cargo run --quiet -p awaken-run-executor-acp --example image_runtime_contract \
  >"$generated_contract"
if [[ -n "${AWAKEN_SANDBOX_BUILD_NETWORK:-}" ]]; then
  build_image --network "$AWAKEN_SANDBOX_BUILD_NETWORK" \
    --build-arg ACP_RUNTIME_IDS="$runtime_ids" \
    --build-arg ACP_RUNTIME_CONTRACT=deploy/images/sandbox/.acp-runtimes.generated.json \
    -f deploy/images/sandbox/Dockerfile -t "$image" .
else
  build_image --build-arg ACP_RUNTIME_IDS="$runtime_ids" \
    --build-arg ACP_RUNTIME_CONTRACT=deploy/images/sandbox/.acp-runtimes.generated.json \
    -f deploy/images/sandbox/Dockerfile -t "$image" .
fi

# Production-image acceptance decision table. Causes: C1 the caller uses any
# umask (including 077); C2 the image runs as UID 10001; C3 the staged binary is
# executable by that UID. Effect E1 the real image can start its hand-capable
# binary. Rule B1 C1+C2+C3=>E1; a staging regression fails the build here instead
# of surfacing later as a closed Session hand channel.
accept_image

# Perform the production prompt-free initialize + session/new handshake, not
# merely `command -v`. Independent adapters are probed concurrently by the
# image-local verifier, bounding cold-start validation to one probe deadline.
if [[ -n "$runtime_ids" ]]; then
  verify_ids=()
  if [[ "$runtime_ids" != all ]]; then
    IFS=',' read -r -a verify_ids <<<"$runtime_ids"
  fi
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /usr/local/bin/awaken-verify-acp-runtimes \
    "$image" "${verify_ids[@]}"
fi
