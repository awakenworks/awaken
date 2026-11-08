#!/usr/bin/env bash
# Keep Cargo artifacts inside the current worktree unless the caller selected
# an explicit target directory. Cargo configuration outside this repository can
# otherwise make unrelated worktrees reuse stale package artifacts.

awaken_configure_cargo_target() {
  local repository_root="$1"

  if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="$repository_root/target"
  fi
  awaken_configure_gate_temp "$repository_root" || exit $?
}

# Gate temporary files go to the disk that holds the build, not to /tmp.
#
# On the shared host / filled to 100% while the build disk had 200 GB free:
# test tempdirs, SQLite tempdirs and `_deadline.sh`'s own deadline directory
# then failed with ENOSPC, tests failed at random and a 48-minute formal run
# died. The target directory is where this checkout already decided its bulk
# writes go, so temporary files follow it:
#
# | Rule | caller's TMPDIR      | target directory             | TMPDIR for this gate             |
# |------|----------------------|------------------------------|----------------------------------|
# | T1   | set, writable dir    | any                          | unchanged: an explicit choice wins |
# | T2   | set, not writable    | any                          | refuse (exit 74), naming TMPDIR  |
# | T3   | unset or empty       | outside the checkout (a symlinked or relocated target) | <physical target>/gate-tmp/<pid> |
# | T4   | unset or empty       | inside the checkout          | <checkout parent>/.<checkout>.gate-tmp/<pid> |
# | T5   | unset, root unwritable | any                        | refuse (exit 74), naming the directory |
# | T6   | native Windows shell | any                          | unchanged: native tools read TEMP/TMP, not TMPDIR |
#
# Never inside the checkout (T4): a temporary crate there joins this Cargo
# workspace ("believes it's in a workspace when it's not") and a temporary
# directory there is inside this Git repository. Each gate run owns one <pid>
# directory, so concurrent gates in one worktree never share or delete each
# other's files; at the next start, directories whose process is gone are
# removed. A nested entry point inherits the exported TMPDIR (T1), so one run
# keeps one directory. The checkout parent is on the target's disk whenever the
# target is inside the checkout.
awaken_gate_temp_root() {
  local repository_root="$1" target anchor checkout
  target="${CARGO_TARGET_DIR:-$repository_root/target}"
  mkdir -p -- "$target" 2>/dev/null || return 1
  anchor=$(cd -- "$target" && pwd -P) || return 1
  checkout=$(cd -- "$repository_root" && pwd -P) || return 1
  case "$anchor/" in
    "$checkout"/*) printf '%s/.%s.gate-tmp\n' "$(dirname -- "$checkout")" "$(basename -- "$checkout")" ;;
    *) printf '%s/gate-tmp\n' "$anchor" ;;
  esac
}

# A test can leave a read-only directory behind (a fixture made with
# `set_readonly`), and `rm` cannot unlink entries of a directory it may not
# write. Restore owner access to every directory first; `find` does not follow
# symlinks, so nothing outside the stale entry changes mode. `\;` runs chmod
# before find descends, which also opens a directory left without `x`.
awaken_remove_stale_gate_temp() {
  find "$1" -type d ! -perm -u=rwx -exec chmod u+rwx -- {} \; 2>/dev/null
  rm -rf -- "$1"
}

awaken_configure_gate_temp() {
  local repository_root="$1" temp_root entry run
  case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) return 0 ;; esac
  if [ -n "${TMPDIR:-}" ]; then
    [ -d "$TMPDIR" ] && [ -w "$TMPDIR" ] && return 0
    echo "gate temp: TMPDIR=$TMPDIR is not a writable directory; create it, point TMPDIR at a" \
      "writable directory on the build disk, or unset it to use one beside the Cargo target" >&2
    return 74
  fi
  if ! temp_root=$(awaken_gate_temp_root "$repository_root") || ! mkdir -p -- "$temp_root" 2>/dev/null; then
    echo "gate temp: cannot create a temporary directory beside the Cargo target" \
      "(${CARGO_TARGET_DIR:-$repository_root/target}${temp_root:+ -> $temp_root});" \
      "make it writable or set TMPDIR to a writable directory on the build disk" >&2
    return 74
  fi
  for entry in "$temp_root"/*; do
    run="${entry##*/}"
    [[ "$run" =~ ^[0-9]+$ ]] && [ -d "$entry" ] && [ ! -L "$entry" ] || continue
    kill -0 "$run" 2>/dev/null || ps -p "$run" >/dev/null 2>&1 || awaken_remove_stale_gate_temp "$entry"
  done
  if ! mkdir -p -- "$temp_root/$$" 2>/dev/null || [ ! -w "$temp_root/$$" ]; then
    echo "gate temp: $temp_root/$$ is not writable; make $temp_root writable or set TMPDIR" \
      "to a writable directory on the build disk" >&2
    return 74
  fi
  export TMPDIR="$temp_root/$$"
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
  # These rows are about the target only; an explicit TMPDIR (T1) keeps the
  # gate-temp placement from creating directories under the fictional roots.
  local -x TMPDIR="${TMPDIR:-/tmp}"

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
  awaken_gate_temp_self_test || return 1
  echo "Cargo target isolation self-test passed"
}

awaken_gate_temp_self_test() (
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
      [ "$(TMPDIR=; awaken_configure_gate_temp /nonexistent; printf '%s' "${TMPDIR-}")" = "" ] || return 1
      echo "Gate temp placement: T6 native Windows keeps TEMP/TMP"; return 0 ;;
  esac
  local fixture checkout outside actual status dead live
  # Rows clear TMPDIR with `TMPDIR=` (empty reads as unset), never `unset`: the
  # caller holds TMPDIR as a local, and unsetting a local re-exposes the hook's
  # exported value instead of clearing it.
  fixture=$(cd -- "$(mktemp -d)" && pwd -P) || return 1
  trap 'chmod -R u+w -- "$fixture" 2>/dev/null; rm -rf -- "$fixture"' EXIT
  checkout="$fixture/work tree"
  outside="$fixture/build disk/target"
  mkdir -p "$checkout" "$outside"
  # T4 target inside the checkout -> a sibling of the checkout, never inside it.
  actual=$(TMPDIR=; unset CARGO_TARGET_DIR; awaken_configure_gate_temp "$checkout" && printf '%s' "$TMPDIR") || return 1
  [ "$actual" = "$fixture/.work tree.gate-tmp/$$" ] && [ -d "$actual" ] && [ -w "$actual" ] ||
    { echo "T4 in-checkout target placed temp at $actual" >&2; return 1; }
  # T3 a target outside the checkout (relocated, or reached through a symlink).
  rm -rf -- "$checkout/target"; ln -s "$outside" "$checkout/target"
  actual=$(TMPDIR=; unset CARGO_TARGET_DIR; awaken_configure_gate_temp "$checkout" && printf '%s' "$TMPDIR") || return 1
  [ "$actual" = "$outside/gate-tmp/$$" ] && [ -d "$actual" ] ||
    { echo "T3 relocated target placed temp at $actual" >&2; return 1; }
  actual=$(TMPDIR=; CARGO_TARGET_DIR="$outside"; awaken_configure_gate_temp "$checkout" && printf '%s' "$TMPDIR") || return 1
  [ "$actual" = "$outside/gate-tmp/$$" ] || { echo "T3 empty TMPDIR was not treated as unset" >&2; return 1; }
  # T1 an explicit writable TMPDIR wins, creates nothing, and a nested entry keeps it.
  actual=$(export TMPDIR="$fixture"; unset CARGO_TARGET_DIR
    awaken_configure_gate_temp "$fixture/elsewhere" && awaken_configure_gate_temp "$checkout" && printf '%s' "$TMPDIR") || return 1
  [ "$actual" = "$fixture" ] && [ ! -e "$fixture/.elsewhere.gate-tmp" ] ||
    { echo "T1 an explicit TMPDIR was replaced by $actual" >&2; return 1; }
  # T2 an explicit TMPDIR that is not a writable directory is refused by name.
  status=0
  actual=$(export TMPDIR="$fixture/absent"; awaken_configure_gate_temp "$checkout" 2>&1) || status=$?
  [ "$status" -eq 74 ] && [[ "$actual" == *"TMPDIR=$fixture/absent is not a writable directory"* ]] ||
    { echo "T2 a missing TMPDIR was not refused: $status $actual" >&2; return 1; }
  # Stale runs: a finished process's directory goes, a live one and a foreign name stay.
  # S2 a stale run holding a read-only tree (a test's `set_readonly` fixture) is
  # still removed, a symlink inside it keeps its target's mode, and a live run's
  # read-only tree is untouched.
  sleep 0 & dead=$!; wait "$dead"
  sleep 30 & live=$!
  mkdir -p "$outside/gate-tmp/$dead/x" "$outside/gate-tmp/$live/ro/inner" "$outside/gate-tmp/keep-me" "$fixture/linked"
  mkdir -p "$outside/gate-tmp/$dead/.tmpA/.skills/example/scripts"
  : > "$outside/gate-tmp/$dead/.tmpA/.skills/example/scripts/run.sh"
  ln -s "$fixture/linked" "$outside/gate-tmp/$dead/.tmpA/.skills/example/escape"
  chmod 555 "$fixture/linked" "$outside/gate-tmp/$dead/.tmpA/.skills/example/scripts" \
    "$outside/gate-tmp/$dead/.tmpA/.skills/example" "$outside/gate-tmp/$live/ro/inner" "$outside/gate-tmp/$live/ro"
  (TMPDIR=; unset CARGO_TARGET_DIR; awaken_configure_gate_temp "$checkout" 2>/dev/null) || { kill "$live"; return 1; }
  [ ! -e "$outside/gate-tmp/$dead" ] && [ -d "$outside/gate-tmp/$live" ] && [ -d "$outside/gate-tmp/keep-me" ] ||
    { kill "$live"; echo "stale-run cleanup removed the wrong directories or left a read-only stale run" >&2; return 1; }
  [ ! -w "$outside/gate-tmp/$live/ro" ] && [ -d "$outside/gate-tmp/$live/ro/inner" ] && [ ! -w "$fixture/linked" ] ||
    { kill "$live"; echo "stale-run cleanup changed a live run or followed a symlink out of the entry" >&2; return 1; }
  kill "$live"; wait "$live" 2>/dev/null
  chmod 755 "$outside/gate-tmp/$live/ro" "$outside/gate-tmp/$live/ro/inner" "$fixture/linked"
  # T5 a temp root that cannot be created is refused by name (root ignores modes).
  if [ "$(id -u)" -ne 0 ]; then
    rm -rf -- "$outside/gate-tmp"; chmod 555 "$outside"; status=0
    actual=$(TMPDIR=; unset CARGO_TARGET_DIR; awaken_configure_gate_temp "$checkout" 2>&1) || status=$?
    chmod 755 "$outside"
    [ "$status" -eq 74 ] && [[ "$actual" == *"cannot create a temporary directory beside the Cargo target"* ]] ||
      { echo "T5 an unwritable temp root was not refused: $status $actual" >&2; return 1; }
  fi
  # doctor.sh --self-test (T8) proves the entry points that select no Cargo
  # target still call this; naming them here would make every edit to them
  # trigger the Rust hook, whose inputs include this file.
  echo "Gate temp placement: T1-T5 and stale-run cleanup (read-only stale trees included) passed"
)

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
