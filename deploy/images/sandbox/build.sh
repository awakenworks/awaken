#!/usr/bin/env bash
# Build the production ACP sandbox image without assuming Cargo writes to ./target.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
source "$repo_root/scripts/ci/_deadline.sh"
# These arguments name Linux image paths. Git Bash must not rewrite them to
# its Windows installation root when invoking the native deadline/engine CLI.
# Host-side staging and build-context paths still receive normal conversion.
export MSYS2_ARG_CONV_EXCL="${MSYS2_ARG_CONV_EXCL:+$MSYS2_ARG_CONV_EXCL;}/usr/local/;/bin/sh"

# The argument vocabulary is exactly: an optional mode flag, then an optional
# image reference and runtime-id set. Anything else used to fall through to a
# real image build, so a misspelled `--ensure-curent` spent minutes in Docker
# and then reported success for a full rebuild nobody asked for -- the opposite
# of what the caller wanted, with the same exit code.
usage() {
  echo "usage: deploy/images/sandbox/build.sh [--self-test]" >&2
  echo "       deploy/images/sandbox/build.sh [--ensure|--ensure-hand|--ensure-current] [<image>] [<runtime-ids>]" >&2
  exit 2
}
case "${1:-}" in
  ""|--self-test|--ensure|--ensure-hand|--ensure-current) ;;
  -*) usage ;;
esac
if [ "${1:-}" = "--self-test" ] && [ "$#" -ne 1 ]; then
  usage
fi

release_image_labels=()
build_provenance_args=()

configure_release_image_labels() {
  release_image_labels=()
  local source=${AWAKEN_SANDBOX_IMAGE_SOURCE:-}
  local revision=${AWAKEN_SANDBOX_IMAGE_REVISION:-}
  local version=${AWAKEN_SANDBOX_IMAGE_VERSION:-}
  if [[ -z $source && -z $revision && -z $version ]]; then
    return 0
  fi
  if [[ -z $source || -z $revision || -z $version ]]; then
    echo "sandbox release image labels must be supplied as one complete set" >&2
    return 2
  fi
  release_image_labels=(
    --label "org.opencontainers.image.source=$source"
    --label "org.opencontainers.image.revision=$revision"
    --label "org.opencontainers.image.version=$version"
  )
}

configure_build_provenance() {
  build_provenance_args=()
  local value=${AWAKEN_SANDBOX_BUILD_PROVENANCE:-}
  [[ -z $value ]] && return 0
  if [[ ${engine##*/} != docker || $value != true && $value != false ]]; then
    echo "AWAKEN_SANDBOX_BUILD_PROVENANCE requires Docker and true or false" >&2
    return 2
  fi
  build_provenance_args=("--provenance=$value")
}

make_public_build_input() {
  chmod 0644 "$1"
}

file_mode() {
  if stat -c '%a' "$1" >/dev/null 2>&1; then
    stat -c '%a' "$1"
  else
    stat -f '%Lp' "$1"
  fi
}

# Build-harness cause/effect graph and decision table:
# C1=external command completes before its deadline; C2=deadline expires while
# it is still active; C3=a secret-free generated Docker input inherits a
# restrictive caller umask; C4=release OCI source/revision/version labels are
# all absent, all present, or partial. E1=preserve the command's status;
# E2=terminate it and return 124; E3=make the copied contract readable by the
# image's non-root user; E4=project the complete label set or reject a partial
# set.
# H1 C1,!C2=>E1; H2 !C1,C2=>E2; H3 C3=>E3; H4 absent=>no labels;
# H5 complete=>three labels; H6 partial=>refuse. H7 absent provenance=>engine
# default; H8 Docker + false=>one deterministic local-build flag; H9 invalid
# value or unsupported engine=>refuse. These self-tests own every rule without
# invoking Cargo, Docker, or a package mirror.
if [[ ${1:-} == --self-test ]]; then
  run_with_deadline 2 bash -c 'exit 0'
  timeout_status=0
  run_with_deadline 1 bash -c 'sleep 30' || timeout_status=$?
  [[ $timeout_status -eq 124 ]] || {
    echo "sandbox image build deadline self-test: expected 124, got $timeout_status" >&2
    exit 1
  }
  descendant_pid_file=$(mktemp "${TMPDIR:-/tmp}/awaken-deadline-descendant.XXXXXX")
  timeout_status=0
  AWAKEN_DEADLINE_DESCENDANT_PID_FILE="$descendant_pid_file" \
    run_with_deadline 1 bash -c 'sleep 30 & echo $! > "$AWAKEN_DEADLINE_DESCENDANT_PID_FILE"; wait' \
    || timeout_status=$?
  descendant_pid=$(cat "$descendant_pid_file")
  rm -f "$descendant_pid_file"
  [[ $timeout_status -eq 124 ]] || {
    echo "sandbox image build descendant deadline self-test: expected 124, got $timeout_status" >&2
    exit 1
  }
  if kill -0 "$descendant_pid" 2>/dev/null; then
    echo "sandbox image build deadline self-test left descendant $descendant_pid running" >&2
    exit 1
  fi
  contract=$(mktemp "${TMPDIR:-/tmp}/awaken-sandbox-contract.XXXXXX")
  chmod 0600 "$contract"
  make_public_build_input "$contract"
  [[ $(file_mode "$contract") == 644 ]] || {
    echo "sandbox image build input self-test: generated contract is not mode 0644" >&2
    rm -f "$contract"
    exit 1
  }
  rm -f "$contract"
  unset AWAKEN_SANDBOX_IMAGE_SOURCE AWAKEN_SANDBOX_IMAGE_REVISION \
    AWAKEN_SANDBOX_IMAGE_VERSION
  configure_release_image_labels
  [[ ${#release_image_labels[@]} -eq 0 ]] || {
    echo "sandbox image label self-test: absent labels must stay absent" >&2
    exit 1
  }
  AWAKEN_SANDBOX_IMAGE_SOURCE=https://github.com/awakenworks/awaken
  AWAKEN_SANDBOX_IMAGE_REVISION=$(printf 'a%.0s' {1..40})
  AWAKEN_SANDBOX_IMAGE_VERSION=v1.2.3
  configure_release_image_labels
  [[ ${#release_image_labels[@]} -eq 6 ]] \
    && [[ ${release_image_labels[1]} == org.opencontainers.image.source=https://github.com/awakenworks/awaken ]] \
    && [[ ${release_image_labels[3]} == org.opencontainers.image.revision="$AWAKEN_SANDBOX_IMAGE_REVISION" ]] \
    && [[ ${release_image_labels[5]} == org.opencontainers.image.version=v1.2.3 ]] || {
    echo "sandbox image label self-test: complete labels were not projected exactly" >&2
    exit 1
  }
  unset AWAKEN_SANDBOX_IMAGE_REVISION
  if configure_release_image_labels; then
    echo "sandbox image label self-test: partial labels must be refused" >&2
    exit 1
  fi
  engine=docker
  unset AWAKEN_SANDBOX_BUILD_PROVENANCE
  configure_build_provenance
  [[ ${#build_provenance_args[@]} -eq 0 ]] || exit 1
  AWAKEN_SANDBOX_BUILD_PROVENANCE=false
  configure_build_provenance
  [[ ${build_provenance_args[*]} == --provenance=false ]] || exit 1
  AWAKEN_SANDBOX_BUILD_PROVENANCE=invalid
  ! configure_build_provenance || exit 1
  engine=podman
  AWAKEN_SANDBOX_BUILD_PROVENANCE=false
  ! configure_build_provenance || exit 1
  echo "sandbox image build deadline self-test passed"
  exit 0
fi

ensure_existing=none
if [[ ${1:-} == --ensure || ${1:-} == --ensure-hand || ${1:-} == --ensure-current ]]; then
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
default_rust_toolchain_image=docker.io/library/rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663
rust_toolchain_image=${AWAKEN_SANDBOX_RUST_TOOLCHAIN_IMAGE:-$default_rust_toolchain_image}
[[ $build_timeout_seconds =~ ^[1-9][0-9]*$ ]] || {
  echo "AWAKEN_SANDBOX_BUILD_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
}
[[ $operation_timeout_seconds =~ ^[1-9][0-9]*$ ]] || {
  echo "AWAKEN_SANDBOX_OPERATION_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
}
configure_release_image_labels
configure_build_provenance

# A caller may supply an already accepted Sandbox image as the Rust-toolchain
# build input when the external component mirror is unavailable. This reuses
# only the pinned toolchain bytes: the current Hand binary and generated ACP
# contract are still staged below, Docker still runs the component/version
# assertion, and the completed image still passes the canonical acceptance.

# A selected docker-container Buildx driver does not load `docker build` results
# into the local image store unless the output is explicit. The production-image
# acceptance command below must inspect the image that this invocation built,
# never a stale local tag or an unavailable BuildKit-only result.
build_image() {
  if [[ "${engine##*/}" == "docker" ]]; then
    run_with_deadline "$build_timeout_seconds" "$engine" build --load --platform linux/amd64 \
      "${build_provenance_args[@]}" "$@"
  else
    run_with_deadline "$build_timeout_seconds" "$engine" build --platform linux/amd64 "$@"
  fi
}

# Image-availability FMECA decision table. C1=the selected tag exists;
# C2=it carries the current production environment label; C3=its real Hand
# entry point starts; C4=curl exists; C5=the pinned Rust quality components
# exist; C6=the same configured execution binary exposes atomic no-replace
# Repository publication; C7=that binary exposes the descriptor-relative
# nofollow tree reader used for terminal participant recovery; C8=each selected
# ACP runtime passes the canonical prompt-free handshake (empty selection has
# no ACP requirement). Every full-acceptance row below additionally requires C8.
# The executable cause/effect cases live in e2e/fixtures/sandbox_image.test.mjs.
# E1=reuse exactly
# that image; E2=build through this authoritative script, then require the same
# acceptance checks.
#
# | Rule | mode | C1 | C2 | C3 | C4 | C5 | C6 | C7 | Effect |
# |---|---|---|---|---|---|---|---|---|---|
# | I1 | build | - | - | - | - | - | - | - | E2, then full acceptance |
# | I2 | ensure | T | T | T | T | T | T | T | E1 |
# | I3 | ensure | otherwise | | | | | | | E2, then full acceptance |
# | I4 | ensure-hand | T | - | T | - | - | T | T | E1 for the Hand-only E2E fixture |
# | I5 | ensure-hand | otherwise | | | | | | | E2, then full acceptance |
# | I6 | ensure-current | I1 checks + exact current binary/contract bytes | | | | | | | E1 |
# | I7 | ensure-current | otherwise | | | | | | | E2, then full acceptance |
accept_hand() {
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /usr/local/bin/awaken-sandbox "$image" hand --stdio </dev/null || return 1
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /usr/local/bin/awaken-sandbox "$image" \
    repository-publish-noreplace --check || return 1
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /usr/local/bin/awaken-sandbox "$image" \
    read-tree-nofollow --check || return 1

  # A Hand-capable image is also the source of every Pod-local helper role.
  # Probe the closed role vocabulary so an older image cannot pass `--ensure-hand`
  # and then fail only after a control-forwarder sidecar has been scheduled.
  local role_output role_status=0
  role_output=$(run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /usr/local/bin/awaken-sandbox "$image" \
    definitely-not-a-role 2>&1) || role_status=$?
  [[ $role_status -ne 0 ]] || return 1
  for role in acp hand git-credential control-forwarder control-forwarder-ready \
    repository-publish-noreplace read-tree-nofollow channel-listen channel-connect \
    scoped-exec repository-effect; do
    grep -Eq "(^|[ |(])${role}([ |)]|$)" <<<"$role_output" || return 1
  done
}

accept_managed_agents_base() {
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /bin/sh "$image" -c \
    '. /etc/os-release && test "$ID" = ubuntu && test "$VERSION_ID" = 24.04 && test "$(uname -m)" = x86_64 && test "$(id -u)" = 10001 && test "$HOME" = /home/agent && test "$PWD" = /workspace && test "$(node -p '\''process.versions.node.split(`.`)[0]'\'')" = 22 && test "$(go env GOVERSION | cut -d. -f1-2)" = go1.24 && test "$(ruby -e '\''print RUBY_VERSION.split(".")[0,2].join(".")'\'')" = 3.3'
}

image_exists() {
  "$engine" image inspect "$image" >/dev/null 2>&1
}

# The configured runtime selection is part of image admission, including reuse.
# An environment label/Hand alone does not prove an ACP CLI exists or negotiates.
accept_runtimes() {
  if [[ -n "$runtime_ids" ]]; then
    local verify_ids=()
    if [[ "$runtime_ids" != all ]]; then
      IFS=',' read -r -a verify_ids <<<"$runtime_ids"
    fi
    run_with_deadline "$operation_timeout_seconds" \
      "$engine" run --rm --entrypoint /usr/local/bin/awaken-verify-acp-runtimes \
      "$image" "${verify_ids[@]}"
  fi
}

accept_image() {
  [[ $($engine image inspect --format '{{index .Config.Labels "org.awaken.environment-packages"}}' "$image" 2>/dev/null) == 3 ]] || return 1
  [[ $($engine image inspect --format '{{index .Config.Labels "org.awaken.managed-agents-base-contract"}}' "$image" 2>/dev/null) == ubuntu-24.04-amd64-v1 ]] || return 1
  [[ $($engine image inspect --format '{{index .Config.Labels "org.awaken.sandbox-role-contract"}}' "$image" 2>/dev/null) == 2 ]] || return 1
  accept_managed_agents_base || return 1
  accept_hand || return 1
  run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --entrypoint /bin/sh "$image" -c \
    'command -v curl >/dev/null && curl --version >/dev/null && cargo clippy --version >/dev/null && rustfmt --version >/dev/null' || return 1
  accept_runtimes
}

generated_context=""
staged=""
generated_contract=""
current_input_fingerprint=""
cleanup() { [[ -z $generated_context ]] || rm -rf -- "$generated_context"; }
trap cleanup EXIT

prepare_current_inputs() {
  [[ -z $generated_context ]] || return 0
  generated_context=$(mktemp -d "${TMPDIR:-/tmp}/awaken-sandbox-build.XXXXXX")
  staged="$generated_context/.awaken-sandbox.bin"
  generated_contract="$generated_context/.acp-runtimes.generated.json"
  "$repo/deploy/images/sandbox/stage-binary.sh" "$staged" hand
  # The Rust ACP catalog is the only package/argv/auth authority. Generate its
  # image projection immediately before admission/build so neither path can
  # consume a stale hand-maintained mirror.
  (cd "$repo" && cargo run --quiet -p awaken-run-executor-acp --example image_runtime_contract) \
    >"$generated_contract"
  make_public_build_input "$generated_contract"
  current_input_fingerprint=$(
    {
      printf 'runtime_ids=%s\n' "$runtime_ids"
      for input in \
        "$repo/deploy/images/sandbox/Dockerfile" \
        "$repo/deploy/images/sandbox/Dockerfile.dockerignore" \
        "$repo/deploy/images/sandbox/hermes-acp-bootstrap.py" \
        "$repo/deploy/images/sandbox/install-acp-runtimes.py" \
        "$repo/deploy/images/sandbox/verify-acp-runtimes.py" \
        "$staged" \
        "$generated_contract"; do
        sha256sum "$input" | cut -d ' ' -f 1
      done
    } | sha256sum | cut -d ' ' -f 1
  )
}

current_inputs_match() {
  local image_binary image_contract image_fingerprint
  image_fingerprint=$(
    "$engine" image inspect --format \
      '{{index .Config.Labels "org.awaken.build-input-fingerprint"}}' "$image" 2>/dev/null
  ) || return 1
  image_binary=$(run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --network none --entrypoint sha256sum "$image" \
    /usr/local/bin/awaken-sandbox) || return 1
  image_contract=$(run_with_deadline "$operation_timeout_seconds" \
    "$engine" run --rm --network none --entrypoint sha256sum "$image" \
    /usr/local/share/awaken/acp-runtimes.json) || return 1
  [[ $image_fingerprint == "$current_input_fingerprint" ]] \
    && [[ ${image_binary%% *} == "$(sha256sum "$staged" | cut -d ' ' -f 1)" ]] \
    && [[ ${image_contract%% *} == "$(sha256sum "$generated_contract" | cut -d ' ' -f 1)" ]]
}

if [[ $ensure_existing == current ]]; then
  prepare_current_inputs
fi

accepted_image=0
if image_exists && accept_image; then
  accepted_image=1
fi
if { [[ $ensure_existing == full ]] && [[ $accepted_image == 1 ]]; } \
  || { [[ $ensure_existing == hand ]] && image_exists && accept_hand; } \
  || { [[ $ensure_existing == current ]] && [[ $accepted_image == 1 ]] && current_inputs_match; }; then
  echo "reusing accepted sandbox image: $image"
  exit 0
fi

# A digest can be admitted unchanged, but cannot be the destination of repair.
# I8 immutable + accepted/current => the reuse branch above; I9 immutable +
# failed admission/current inputs => refuse before any Docker build. Operators
# build and publish a mutable tag, then select its resulting immutable digest.
if [[ $image == *@* ]]; then
  echo "immutable sandbox image did not pass requested admission: $image; build and publish a current image under a mutable tag, then supply its new digest" >&2
  exit 2
fi

# Current-source rebuild decision table. C1 the target image passed the full
# production admission above; C2 current binary/contract bytes differ; C3 the
# caller did/did not explicitly select a toolchain seed. Effects: C1+C2+!C3
# reuses that accepted image's already verified Rust/Clippy/Rustfmt bytes;
# C3 retains the explicit seed; !C1 uses the one digest-pinned public default.
# The output still rebuilds and revalidates every current-source input. There is
# no mirror fallback and an unaccepted image can never become a toolchain owner.
if [[ $ensure_existing == current ]] \
  && [[ $accepted_image == 1 ]] \
  && [[ -z ${AWAKEN_SANDBOX_RUST_TOOLCHAIN_IMAGE:-} ]]; then
  rust_toolchain_image=$image
fi

cd "$repo"
prepare_current_inputs
if [[ -n "${AWAKEN_SANDBOX_BUILD_NETWORK:-}" ]]; then
  build_image --network "$AWAKEN_SANDBOX_BUILD_NETWORK" \
    "${release_image_labels[@]}" \
    --build-context awaken_generated="$generated_context" \
    --build-arg AWAKEN_BUILD_INPUT_FINGERPRINT="$current_input_fingerprint" \
    --build-arg RUST_TOOLCHAIN_IMAGE="$rust_toolchain_image" \
    --build-arg ACP_RUNTIME_IDS="$runtime_ids" \
    -f deploy/images/sandbox/Dockerfile -t "$image" deploy/images/sandbox
else
  build_image "${release_image_labels[@]}" \
    --build-context awaken_generated="$generated_context" \
    --build-arg AWAKEN_BUILD_INPUT_FINGERPRINT="$current_input_fingerprint" \
    --build-arg RUST_TOOLCHAIN_IMAGE="$rust_toolchain_image" \
    --build-arg ACP_RUNTIME_IDS="$runtime_ids" \
    -f deploy/images/sandbox/Dockerfile -t "$image" deploy/images/sandbox
fi

# Production-image acceptance decision table. Causes: C1 the caller uses any
# umask (including 077); C2 the image runs as UID 10001; C3 the staged binary is
# executable by that UID. Effect E1 the real image can start its hand-capable
# binary. Rule B1 C1+C2+C3=>E1; a staging regression fails the build here instead
# of surfacing later as a closed Session hand channel.
accept_image
current_inputs_match
