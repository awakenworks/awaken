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

  echo "Cargo target isolation self-test passed"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  case "${1:-}" in
    --self-test) awaken_cargo_target_self_test ;;
    *) echo "usage: $0 --self-test" >&2; exit 2 ;;
  esac
fi
