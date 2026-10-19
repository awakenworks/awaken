#!/usr/bin/env bash
# Keep Cargo artifacts inside the current worktree unless the caller selected
# an explicit target directory. Cargo configuration outside this repository can
# otherwise make unrelated worktrees reuse stale package artifacts.

awaken_configure_cargo_target() {
  local repository_root="$1"

  if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="$repository_root/target"
  fi
}

# Public API generation uses a pinned nightly compiler. Keep its rustdoc and
# dependency artifacts separate from the caller's stable builds. The caller's
# selected target remains the base, and an explicit API cache stays supported.
# Invoke this in the API check's child shell; the parent target is unchanged.
awaken_configure_public_api_target() {
  if [ "$#" -ne 1 ] || [[ ! "$1" =~ ^nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
    echo "public API cache requires one pinned nightly toolchain" >&2
    return 2
  fi
  if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    echo "select the checkout Cargo target before its public API cache" >&2
    return 2
  fi
  local toolchain="$1" base="${CARGO_TARGET_DIR%/}" selected
  selected="${AWAKEN_PUBLIC_API_TARGET_DIR:-}"
  if [ -z "$selected" ]; then
    case "$base" in
      */public-api-"$toolchain") selected="$base" ;;
      *) selected="$base/public-api-$toolchain" ;;
    esac
  fi
  export CARGO_TARGET_DIR="$selected"
  # Cargo's intermediate directory must not fall back to the stable cache.
  export CARGO_BUILD_BUILD_DIR="$selected"
}

# Kani's driver reads its goto artifacts from `<target>/kani/debug`, so it
# assumes Cargo's build directory sits inside the target directory. A developer
# or CI that separates them (`build.build-dir`, common when intermediates live
# on another disk) otherwise makes a valid compilation end in a bare ENOENT.
#
# This lives beside the target-directory authority rather than in whichever
# caller happened to need it first, so the batch runner and a person at a
# terminal share one expression instead of two copies that can drift.
awaken_configure_kani_driver_target() {
  export CARGO_BUILD_BUILD_DIR="${CARGO_TARGET_DIR%/}/kani"
}

awaken_cargo_target_self_test() {
  local first_root="/tmp/awaken-worktree-a"
  local second_root="/tmp/awaken-worktree-b"
  local explicit_target="/tmp/awaken-explicit-target"
  local actual

  # Cause/effect graph:
  # C1 caller omits CARGO_TARGET_DIR -> E1 use the current worktree target.
  # C2 caller supplies CARGO_TARGET_DIR -> E2 preserve the caller's target.
  # C3 repository roots differ -> E3 defaults cannot share artifacts.
  # C4 configuration runs twice -> E4 the selected target remains stable.
  # C5 standalone public-API checks -> E5 they reuse this authority instead of
  # falling through to a user-global target shared by unrelated worktrees.
  # C6 E2E binary builds -> E6 their one Cargo artifact resolver applies the
  # same checkout-local default while preserving an explicit target.
  # C7 standalone Kubernetes E2E -> E7 its direct Cargo invocations reuse this
  # authority instead of falling through to user-global Cargo configuration.
  # C8 Managed SDK hooks invoke an all-features Cargo behavior owner -> E8 the
  # package command reuses this authority instead of a cross-worktree rmeta.
  #
  # Decision table:
  # Rule | C1 | C2 | C3 | C4 | Expected effect
  # R1   | Y  | N  | N  | N  | E1
  # R2   | N  | Y  | N  | N  | E2
  # R3   | Y  | N  | Y  | N  | E1 + E3
  # R4   | Y  | N  | N  | Y  | E1 + E4
  # R5   | Y  | N  | N  | N  | E5
  # R6   | Y  | N  | N  | N  | E6
  # R7   | Y  | N  | N  | N  | E7
  actual="$({ unset CARGO_TARGET_DIR; awaken_configure_cargo_target "$first_root"; printf '%s' "$CARGO_TARGET_DIR"; })"
  [ "$actual" = "$first_root/target" ] || return 1

  actual="$({ export CARGO_TARGET_DIR="$explicit_target"; awaken_configure_cargo_target "$first_root"; printf '%s' "$CARGO_TARGET_DIR"; })"
  [ "$actual" = "$explicit_target" ] || return 1

  local first_target second_target
  first_target="$({ unset CARGO_TARGET_DIR; awaken_configure_cargo_target "$first_root"; printf '%s' "$CARGO_TARGET_DIR"; })"
  second_target="$({ unset CARGO_TARGET_DIR; awaken_configure_cargo_target "$second_root"; printf '%s' "$CARGO_TARGET_DIR"; })"
  [ "$first_target" != "$second_target" ] || return 1

  actual="$({ unset CARGO_TARGET_DIR; awaken_configure_cargo_target "$first_root"; awaken_configure_cargo_target "$first_root"; printf '%s' "$CARGO_TARGET_DIR"; })"
  [ "$actual" = "$first_root/target" ] || return 1

  grep -Fq 'source scripts/ci/_cargo_target.sh' scripts/ci/check_public_api.sh || return 1
  grep -Fq 'awaken_configure_cargo_target "$PWD"' scripts/ci/check_public_api.sh || return 1
  grep -Fq 'checkoutLocalCargoEnvironment(cwd, environment)' e2e/cargo_binary.mjs || return 1
  grep -Fq 'source scripts/ci/_cargo_target.sh' scripts/e2e/k8s_container_e2e.sh || return 1
  grep -Fq 'awaken_configure_cargo_target "$PWD"' scripts/e2e/k8s_container_e2e.sh || return 1
  grep -Fq 'awaken_configure_cargo_target "$PWD"' scripts/ci/check_managed_sdk_behavior.sh || return 1
  grep -Fq '../../scripts/ci/check_managed_sdk_behavior.sh' packages/managed-sdk-oracle/package.json || return 1

  awaken_public_api_target_self_test || return 1
  echo "Cargo target isolation self-test passed"
}

awaken_public_api_target_self_test() {
  local pin="nightly-2026-07-14" selected first second status
  local parent_target="${CARGO_TARGET_DIR-}" parent_build="${CARGO_BUILD_BUILD_DIR-}"
  # P1 inherited stable/intermediate caches -> one pinned API child; repeat
  # selection is idempotent and the parent's environment stays unchanged.
  selected="$(
    unset AWAKEN_PUBLIC_API_TARGET_DIR
    export CARGO_TARGET_DIR="/tmp/awaken-stable" CARGO_BUILD_BUILD_DIR="/tmp/global-intermediates"
    awaken_configure_public_api_target "$pin" || exit 1
    [ "$CARGO_TARGET_DIR" = "/tmp/awaken-stable/public-api-$pin" ] || exit 1
    [ "$CARGO_BUILD_BUILD_DIR" = "$CARGO_TARGET_DIR" ] || exit 1
    awaken_configure_public_api_target "$pin" || exit 1
    printf '%s' "$CARGO_TARGET_DIR"
  )" || return 1
  [ "$selected" = "/tmp/awaken-stable/public-api-$pin" ] || return 1
  # P2 explicitly selected API path, including spaces, owns both directories.
  selected="$(
    export CARGO_TARGET_DIR="/tmp/awaken-stable" CARGO_BUILD_BUILD_DIR="/tmp/global-intermediates"
    export AWAKEN_PUBLIC_API_TARGET_DIR="C:/chosen cache/API"
    awaken_configure_public_api_target "$pin" || exit 1
    [ "$CARGO_BUILD_BUILD_DIR" = "$CARGO_TARGET_DIR" ] || exit 1
    printf '%s' "$CARGO_TARGET_DIR"
  )" || return 1
  [ "$selected" = "C:/chosen cache/API" ] || return 1
  # P3 separate worktrees never get the same default API cache.
  first="$(unset AWAKEN_PUBLIC_API_TARGET_DIR; export CARGO_TARGET_DIR=/tmp/awaken-a; awaken_configure_public_api_target "$pin" || exit 1; printf '%s' "$CARGO_TARGET_DIR")" || return 1
  second="$(unset AWAKEN_PUBLIC_API_TARGET_DIR; export CARGO_TARGET_DIR=/tmp/awaken-b; awaken_configure_public_api_target "$pin" || exit 1; printf '%s' "$CARGO_TARGET_DIR")" || return 1
  [ "$first" != "$second" ] || return 1
  # P4 a reviewed compiler upgrade selects a different default cache.
  second="$(unset AWAKEN_PUBLIC_API_TARGET_DIR; export CARGO_TARGET_DIR=/tmp/awaken-a; awaken_configure_public_api_target nightly-2026-08-01 || exit 1; printf '%s' "$CARGO_TARGET_DIR")" || return 1
  [ "$first" != "$second" ] || return 1
  # P5 malformed/extra arguments cannot mutate either selected directory.
  (
    export CARGO_TARGET_DIR=/tmp/awaken-stable CARGO_BUILD_BUILD_DIR=/tmp/stable-intermediates
    status=0; awaken_configure_public_api_target ../unstable >/dev/null 2>&1 || status=$?
    [ "$status" -eq 2 ] || exit 1
    status=0; awaken_configure_public_api_target "$pin" extra >/dev/null 2>&1 || status=$?
    [ "$status" -eq 2 ] || exit 1
    status=0; awaken_configure_public_api_target >/dev/null 2>&1 || status=$?
    [ "$status" -eq 2 ] || exit 1
    [ "$CARGO_TARGET_DIR" = /tmp/awaken-stable ] && [ "$CARGO_BUILD_BUILD_DIR" = /tmp/stable-intermediates ]
  ) || return 1
  (
    unset CARGO_TARGET_DIR
    export CARGO_BUILD_BUILD_DIR=/tmp/stable-intermediates
    status=0; awaken_configure_public_api_target "$pin" >/dev/null 2>&1 || status=$?
    [ "$status" -eq 2 ] && [ -z "${CARGO_TARGET_DIR:-}" ] && [ "$CARGO_BUILD_BUILD_DIR" = /tmp/stable-intermediates ]
  ) || return 1
  # P6 the real snapshot owner must select this cache after tool admission.
  grep -Fq 'awaken_configure_public_api_target "$PUBLIC_API_NIGHTLY"' scripts/ci/check_public_api.sh || return 1
  [ "${CARGO_TARGET_DIR-}" = "$parent_target" ] && [ "${CARGO_BUILD_BUILD_DIR-}" = "$parent_build" ] || return 1
  echo "Public API Cargo cache isolation: six case families passed"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  case "${1:-}" in
    --self-test) awaken_cargo_target_self_test ;;
    *) echo "usage: $0 --self-test" >&2; exit 2 ;;
  esac
fi
