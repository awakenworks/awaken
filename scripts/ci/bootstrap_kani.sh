#!/usr/bin/env bash
set -euo pipefail

# Kani 0.67's published bundle contains Rust 1.93, below this workspace's Rust
# 1.96 floor. Pin a reviewed upstream source revision whose own toolchain is
# Rust 1.97 nightly; never bypass Cargo's rust-version check.
readonly kani_revision="e43ae2c255f11dd6b0f91468c40e0b3403b810c5"
readonly kani_nightly="nightly-2026-05-01"
readonly kani_backend_version="0.67.0"
readonly kani_patch_sha256="3238739bb566d4ec122ccc1224f01a45576a4b86b187ba95dc690104ac30e1fd"
# The compiler still uses the release-matched GOTO tools. Only CBMC's reviewed
# SMT encoder/model reader is replaced; this script owns both source identities.
readonly cbmc_revision="cdee49cb1a32c6d6703cebf6ae67161977264ad4"
readonly cbmc_patch_sha256="959fd1118cddf97f08d21affd0c621ecf608bf3b3242ee795dd387c714ca973d"
readonly cbmc_build_options="-G Ninja -DCMAKE_BUILD_TYPE=Release -DWITH_JBMC=OFF -Dsat_impl=cadical"
readonly script_dir="$(cd "$(dirname "$0")" && pwd)"
readonly backend_patch="$script_dir/patches/cbmc-smt-type-dependencies.patch"
readonly compiler_patch="$script_dir/patches/kani-wide-niche.patch"
readonly cache_root="${AWAKEN_KANI_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/awaken-kani}"
# The qualification fixtures run the real solver, and the receipt records which
# one. A non-login shell does not see the bundled Z3, and its absence showed up
# as six failed SMT assertions rather than as a missing tool.
source "$script_dir/_provider_environment.sh"
awaken_append_repo_tools
# Keep the unmodified source cache intact, including any in-flight proof runs.
source_dir="$cache_root/$kani_revision-${kani_patch_sha256:0:12}"
qualification_sha256="$(sha256sum "$0" "$script_dir/fixtures/cbmc_recursive_alias.c" "$script_dir/fixtures/kani_wide_niche.rs" \
  "$script_dir/_cargo_target.sh" \
  "$script_dir/fixtures/kani_constructor/Cargo.toml" "$script_dir/fixtures/kani_constructor/Cargo.lock" \
  "$script_dir/fixtures/kani_constructor/model/Cargo.toml" "$script_dir/fixtures/kani_constructor/model/src/lib.rs" \
  "$script_dir/fixtures/kani_constructor/src/lib.rs" |
  cut -d ' ' -f 1 | sha256sum | cut -d ' ' -f 1)"
# One receipt per qualification procedure. Worktrees at different revisions of
# this script share one built package; a single receipt made each of them
# withdraw the other's judgement on every run. Each procedure now owns its own
# receipt beside the artifact, and none can invalidate another's.
qualified_receipt="qualified-${qualification_sha256:0:12}.txt"
qualified_pending="qualified-${qualification_sha256:0:12}.pending"
# The cache directory names the *artifact*: exactly the three inputs that decide
# the bytes of cbmc/goto-cc/goto-instrument. The judgement that those bytes are
# trustworthy is a separate fact, recorded in `qualified-<procedure>.txt` inside the
# directory and still bound to `qualification_sha256`. Folding the judgement
# into the directory name made every edit of this script — a comment, a locale
# fix — discard a built CBMC and demand a rebuild that needs cmake and ninja,
# so a machine without them lost the whole formal gate. Now a changed procedure
# invalidates the receipt and re-runs the qualification fixtures against the
# backend that is already there; only a changed revision, patch or build option
# rebuilds.
derive_backend_key() {
  printf 'cbmc-%s-%s-%s' "$1" "${2:0:12}" "$(printf '%s' "$3" | sha256sum | cut -c 1-12)"
}
backend_key="$(derive_backend_key "$cbmc_revision" "$cbmc_patch_sha256" "$cbmc_build_options")"
backend_dir="${AWAKEN_KANI_BACKEND_DIR:-$cache_root/$backend_key/bin}"
backend_dir="$(readlink -m "$backend_dir")"
backend_root="$(dirname "$backend_dir")"
cbmc_source="$backend_root/source"
for component in compiler backend; do
  if [ "$component" = compiler ]; then
    patch="$compiler_patch"; expected="$kani_patch_sha256"
  else
    patch="$backend_patch"; expected="$cbmc_patch_sha256"
  fi
  if [ "$(sha256sum "$patch" | cut -d ' ' -f 1)" != "$expected" ]; then
    echo "$component patch digest differs from its reviewed pin" >&2
    exit 1
  fi
done

# The same complete source comparison serves both builds and the actual runner.
verify_source() {
  local label="$1" directory="$2" revision="$3" patch="$4"
  if [ "$(git -C "$directory" rev-parse HEAD)" != "$revision" ]; then
    echo "$label cache contains an unexpected revision: $directory" >&2
    exit 1
  fi
  git -C "$directory" diff --cached --exit-code >/dev/null
  if [ -n "$(git -C "$directory" ls-files --others --exclude-standard)" ]; then
    echo "$label source cache contains untracked inputs: $directory" >&2
    exit 1
  fi
  if [ "${5:-}" = "allow-clean" ] && git -C "$directory" diff --quiet; then
    return 0
  fi
  if ! git -C "$directory" diff --no-ext-diff --no-textconv --no-color --binary HEAD |
       cmp -s "$patch" -; then
    echo "$label source cache differs from its reviewed patch: $directory" >&2
    exit 1
  fi
}

verify_kani_source() {
  verify_source Kani "$source_dir" "$kani_revision" "$compiler_patch" "${1:-}"
}

# Paths as well as dereferenced bytes form the package identity: retargeting a
# symlink, even to equal bytes, cannot silently change an invocation's backend.
#
# What the built artifact is. Every line here is either an input that decides
# the bytes or an observation of the bytes themselves; nothing about how those
# bytes were judged. This is what `built.txt` records, and matching it is what
# lets a bootstrap skip the expensive build.
build_identity() {
  local name path digest configuration source_path
  source_path="$(readlink -e "$cbmc_source")" || {
    echo "Kani backend is incomplete: no CBMC source at $cbmc_source" >&2
    return 1
  }
  configuration="$(sha256sum "$backend_root/build/CMakeCache.txt")" || {
    echo "Kani backend is incomplete: no CMake configuration at $backend_root/build" >&2
    return 1
  }
  printf '%s\n' "backend_source=$source_path" \
    "cbmc_revision=$cbmc_revision" "cbmc_patch_sha256=$cbmc_patch_sha256" \
    "cbmc_build_options=$cbmc_build_options" "goto_release=$kani_backend_version"
  for name in cbmc goto-cc goto-instrument; do
    path="$(readlink -e "$backend_dir/$name")" || {
      echo "Kani backend is incomplete: $name is missing from $backend_dir" >&2
      return 1
    }
    [ -x "$path" ] || { echo "Kani backend is incomplete: $name is not executable" >&2; return 1; }
    digest="$(sha256sum "$path")" || return 1
    printf '%s\n' "${name}_path=$path" "${name}_sha256=${digest%% *}"
  done
  printf '%s\n' "configuration_sha256=${configuration%% *}"
}

# What makes those bytes trusted: the build fact, then the procedure that
# judged it and the external solver and fixture that procedure ran. The build
# fact is an exact prefix, so a receipt always names the artifact it blessed.
backend_identity() {
  local path digest
  build_identity || return 1
  path="$(command -v z3)" || {
    echo "Kani backend is incomplete: z3 is not on PATH (the tlapm bundle ships one under" >&2
    echo "  \$HOME/.local/share/awaken-formal/tools/tlapm/lib/tlapm/backends/bin)" >&2
    return 1
  }
  path="$(readlink -e "$path")" || return 1
  digest="$(sha256sum "$path")" || return 1
  printf '%s\n' "qualification_sha256=$qualification_sha256" \
    "qualification_z3_path=$path" "qualification_z3_sha256=${digest%% *}"
  digest="$(sha256sum "$script_dir/fixtures/cbmc_recursive_alias.c")" || return 1
  printf '%s\n' "fixture_sha256=${digest%% *}"
}

verify_backend() {
  verify_source CBMC "$cbmc_source" "$cbmc_revision" "$backend_patch"
  local identity name selected
  identity="$(backend_identity)" || exit 1
  if [ ! -f "$backend_root/$qualified_receipt" ]; then
    echo "Kani backend was never qualified by this procedure: $backend_root/$qualified_receipt" >&2
    echo "  run scripts/ci/bootstrap_kani.sh to re-run the qualification fixtures" >&2
    exit 1
  fi
  if [ "$(cat "$backend_root/$qualified_receipt")" != "$identity" ]; then
    echo "Kani backend differs from its qualified cache: $backend_root" >&2
    echo "  run scripts/ci/bootstrap_kani.sh to re-run the qualification fixtures" >&2
    exit 1
  fi
  for name in cbmc goto-cc goto-instrument; do
    selected="$(PATH="$source_dir/scripts:$backend_dir:$PATH" command -v "$name")"
    if [ "$(readlink -e "$selected")" != "$(readlink -e "$backend_dir/$name")" ]; then
      echo "Kani driver PATH selects a different $name: $selected" >&2
      exit 1
    fi
  done
}

kani_build_digest() {
  # The digest names a set of files, so the order they are hashed in must not
  # depend on who runs it. `sort` collates by locale, so an operator whose
  # LANG is not C would compute a different digest for the same bytes and read
  # an intact qualified cache as tampered with.
  (cd "$source_dir" && find target/kani -type f -print0 |
    LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -d ' ' -f 1)
}

verify_kani_build() {
  local receipt="$source_dir/target/awaken-kani-build.sha256"
  if [ ! -f "$receipt" ] || [ "$(cat "$receipt")" != "$(kani_build_digest)" ]; then
    echo "Kani build is missing or differs from its qualified cache: $source_dir" >&2
    exit 1
  fi
}

if [ "${1:-}" = "--build-identity" ] && [ "$#" -eq 1 ]; then
  # Read-only companion to --verify-source: print what this package's build
  # receipt says, so "differs from its build receipt" can be diffed against the
  # recorded file instead of guessed at. Claims nothing about qualification.
  build_identity
  exit $?
elif [ "${1:-}" = "--solver-tools" ] && [ "$#" -eq 1 ]; then
  # Read-only: the released Kani bundle ships the external SAT solvers (kissat)
  # that the source-built driver execs by name. It claims nothing about
  # qualification; a caller only appends it to PATH.
  printf '%s\n' "$HOME/.kani/kani-$kani_backend_version/bin"
  exit 0
elif [ "${1:-}" = "--paths" ] && [ "$#" -eq 1 ]; then
  # Selection is read-only and does not claim qualification. The runner invokes
  # --verify-source before any proof, using these frozen physical directories.
  printf '%s\n' "$(readlink -m "${AWAKEN_KANI_SOURCE_DIR:-$cache_root/current}")" "$backend_dir"
  exit 0
elif [ "${1:-}" = "--verify-source" ] && [ "$#" -eq 2 ]; then
  source_dir="$(readlink -e "$2")"
  verify_kani_source
  verify_kani_build
  verify_backend
  printf '%s\n' "source=$source_dir" "revision=$kani_revision" \
    "patch_sha256=$kani_patch_sha256" \
    "build_sha256=$(cat "$source_dir/target/awaken-kani-build.sha256")"
  backend_identity
  exit 0
elif [ "$#" -ne 0 ]; then
  echo "usage: bootstrap_kani.sh [--build-identity | --solver-tools | --paths | --verify-source PATH]" >&2
  exit 1
fi

# An explicitly selected backend must already be qualified. Bootstrap never
# imports an arbitrary binary or mutates a selected external package in place.
#
# Its own cache is different. A receipt that no longer matches was written by a
# different qualification procedure, and the answer is to judge again rather
# than to refuse: the artifact is still here, and re-running the fixtures costs
# seconds. Refusing was what removed the formal gate from any machine without
# cmake the moment this script was edited.
if [ -n "${AWAKEN_KANI_BACKEND_DIR:-}" ]; then
  verify_backend
elif [ -f "$backend_root/$qualified_receipt" ] &&
     [ "$(cat "$backend_root/$qualified_receipt")" = "$(backend_identity 2>/dev/null)" ]; then
  verify_backend
else
  if [ -f "$backend_root/$qualified_receipt" ]; then
    echo "Re-qualifying: the CBMC package no longer matches this procedure's receipt" >&2
    # Withdraw the claim before re-judging: between here and publication this
    # package is exactly as unqualified as a fresh one, and every rule below
    # treats it that way.
    rm -f -- "$backend_root/$qualified_receipt"
  fi
  # A symlink cannot redirect the only writable, unqualified build destination.
  if [ "$backend_root" != "$(readlink -m "$cache_root")/$backend_key" ]; then
    echo "Refusing to build through a redirected backend package" >&2
    exit 1
  fi
  if [ -L "$cbmc_source" ]; then
    echo "Refusing to patch a redirected CBMC source" >&2
    exit 1
  fi
  # These are exclusively owned unqualified outputs. Unlink destinations before
  # any writer opens them, preserving old targets of symlinks and hard links.
  rm -f -- "$backend_root/$qualified_pending" "$backend_root"/{unit.log,z3.log,vla1.log,Pointer_Arithmetic1.log,Pointer_Arithmetic17.log,leaf.log,internal.log,leaf.goto,internal.goto}
  # The artifact and the judgement are separate facts. When a build receipt is
  # already here and still describes exactly these bytes, this procedure has
  # nothing to rebuild: it re-runs the qualification fixtures below against the
  # backend that exists. A receipt that no longer describes the bytes is not a
  # reason to refuse — the bytes are what is missing, and refusing would wedge
  # the package against every later attempt, which is precisely the failure a
  # half-finished earlier run leaves behind. Such a receipt is discarded and the
  # package is rebuilt. Nothing is trusted either way: the qualification
  # fixtures below still run against whatever bytes end up published.
  reusable=0
  if [ -f "$backend_root/built.txt" ] && [ -d "$cbmc_source/.git" ]; then
    verify_source CBMC "$cbmc_source" "$cbmc_revision" "$backend_patch"
    if [ "$(cat "$backend_root/built.txt")" = "$(build_identity 2>/dev/null)" ]; then
      reusable=1
    else
      echo "Rebuilding: the CBMC package no longer matches its build receipt" >&2
      rm -f -- "$backend_root/built.txt"
    fi
  fi
  if [ "$reusable" -eq 1 ]; then
    echo "Reusing the qualified-source CBMC build at $backend_root"
  else
    # The released GOTO tools are only needed to produce the package.
    released_backend="$HOME/.kani/kani-$kani_backend_version/bin"
    if [ ! -x "$released_backend/goto-cc" ] || [ ! -x "$released_backend/goto-instrument" ]; then
      cargo install --locked kani-verifier --version "$kani_backend_version"
      cargo kani setup
    fi
    for name in goto-cc goto-instrument; do
      [ -x "$released_backend/$name" ] || { echo "Missing released $name" >&2; exit 1; }
    done
    if [ ! -d "$cbmc_source/.git" ]; then
      mkdir -p "$backend_root"
      mkdir -p "$cbmc_source"
      git -C "$cbmc_source" init
      git -C "$cbmc_source" remote add origin https://github.com/diffblue/cbmc.git
      git -C "$cbmc_source" fetch --depth 1 origin "$cbmc_revision"
      git -C "$cbmc_source" checkout --detach "$cbmc_revision"
    fi
    verify_source CBMC "$cbmc_source" "$cbmc_revision" "$backend_patch" allow-clean
    if git -C "$cbmc_source" diff --quiet; then
      git -C "$cbmc_source" apply --check "$backend_patch"
      git -C "$cbmc_source" apply "$backend_patch"
    fi
    verify_source CBMC "$cbmc_source" "$cbmc_revision" "$backend_patch"
    # Only this unqualified, canonically derived build directory is disposable.
    # A failed attempt must not leave extra CMake flags in the next build.
    rm -rf -- "$backend_root/build"
    read -r -a build_options <<< "$cbmc_build_options"
    cmake -S "$cbmc_source" -B "$backend_root/build" "${build_options[@]}"
    cmake --build "$backend_root/build" --target cbmc unit -j 2
    mkdir -p "$backend_dir"
    # Replace owned destination links rather than following them into an older
    # package; this also prevents an unqualified hard link from changing it.
    cp --remove-destination -L "$backend_root/build/bin/cbmc" "$backend_dir/cbmc"
    for name in goto-cc goto-instrument; do
      cp --remove-destination -L "$released_backend/$name" "$backend_dir/$name"
    done
    # The bytes exist and came from the reviewed source. Record that fact before
    # any fixture runs: what the fixtures decide is trust, not existence.
    build_identity > "$backend_root/built.txt"
  fi
  # The reviewed patch owns its semantic tests, including opt-in real Z3 SAT,
  # UNSAT and typed model reads. A zero exit is required before publication.
  "$backend_root/build/bin/unit" '[smt2]' | tee "$backend_root/unit.log"
  "$backend_root/build/bin/unit" '[smt2-z3]' | tee "$backend_root/z3.log"
  for scenario in vla1 Pointer_Arithmetic1 Pointer_Arithmetic17; do
    "$backend_dir/cbmc" --z3 --verbosity 8 \
      "$cbmc_source/regression/cbmc/$scenario/main.c" > "$backend_root/$scenario.log"
    grep -q 'VERIFICATION SUCCESSFUL' "$backend_root/$scenario.log"
  done
  for order in leaf internal; do
    defines=()
    [ "$order" != internal ] || defines=(-DSTART_INTERNAL)
    "$backend_dir/goto-cc" "${defines[@]}" "$script_dir/fixtures/cbmc_recursive_alias.c" \
      -o "$backend_root/$order.goto"
    result=0
    "$backend_dir/cbmc" --z3 --verbosity 8 "$backend_root/$order.goto" \
      > "$backend_root/$order.log" 2>&1 || result=$?
    # This assertion is intentionally false. Neither an encoder error (6), a
    # solver error nor a vacuous green run qualifies the repaired backend.
    [ "$result" -eq 10 ]
    grep -q 'main.assertion.1.*FAILURE' "$backend_root/$order.log"
  done
  backend_identity > "$backend_root/$qualified_pending"
fi

if [ ! -d "$source_dir/.git" ]; then
  mkdir -p "$cache_root"
  git clone --no-checkout https://github.com/model-checking/kani.git "$source_dir"
  git -C "$source_dir" checkout --detach "$kani_revision"
  git -C "$source_dir" submodule update --init --depth 1 charon
fi
# The reviewed compiler patch preserves full u128 niches and accepts imported
# constructor shims in diagnostic statistics. Refuse unrelated cached edits.
verify_kani_source allow-clean
if git -C "$source_dir" diff --quiet; then
  git -C "$source_dir" apply --check "$compiler_patch"
  git -C "$source_dir" apply "$compiler_patch"
fi
verify_kani_source

# Qualified builds are immutable, including when current already selects them.
# Never overwrite their compiler or libraries during a repeated bootstrap.
if [ -f "$source_dir/target/awaken-kani-build.sha256" ]; then
  verify_kani_build
else
  rustup toolchain install "$kani_nightly" --profile minimal \
    --component llvm-tools,rustc-dev,rust-src,rustfmt
  pushd "$source_dir" >/dev/null
  # Cargo's admitted external-command parent may carry the workspace toolchain
  # in RUSTUP_TOOLCHAIN. It outranks Kani's rust-toolchain.toml after pushd.
  # Bind this build and its nested Cargo calls to Kani's reviewed nightly only.
  RUSTUP_TOOLCHAIN="$kani_nightly" cargo build-dev
  popd >/dev/null
fi
# Qualify the formerly crashing layout before making this cache discoverable.
PATH="$backend_dir:$PATH" "$source_dir/scripts/kani" \
  "$script_dir/fixtures/kani_wide_niche.rs" \
  --target-dir "$source_dir/target/wide-niche-regression" --output-format terse
# This second fixture crosses a real crate boundary. A same-crate constructor
# or foreign function cannot reproduce the missing original-MIR condition.
constructor_log="$(mktemp "$source_dir/target/constructor-regression.XXXXXX.log")"
constructor_lock_sha256="$(sha256sum "$script_dir/fixtures/kani_constructor/Cargo.lock" | cut -d ' ' -f 1)"
(
  # Cargo's global build-dir may point elsewhere; use the same driver-layout
  # authority as check_formal.sh and kani.sh, scoped to this fixture only.
  source "$script_dir/_cargo_target.sh"
  export CARGO_TARGET_DIR="$source_dir/target/constructor-regression"
  awaken_configure_kani_driver_target
  # This Kani revision has no --locked option. Native Cargo admits the exact
  # local-only dependency graph first; the lock bytes must also survive proof.
  cargo metadata --locked --no-deps --format-version 1 \
    --manifest-path "$script_dir/fixtures/kani_constructor/Cargo.toml" \
    > "$source_dir/target/constructor-metadata.json"
  PATH="$source_dir/scripts:$backend_dir:$PATH" cargo kani \
    --manifest-path "$script_dir/fixtures/kani_constructor/Cargo.toml" \
    --exact --harness imported_constructor_preserves_its_value \
    --target-dir "$CARGO_TARGET_DIR" --output-format terse --verbose
) 2>&1 | tee "$constructor_log"
# A zero process status is insufficient when a filter selects no harness.
grep -Fxq 'Checking harness imported_constructor_preserves_its_value...' "$constructor_log"
grep -Fxq 'Complete - 1 successfully verified harnesses, 0 failures, 1 total.' "$constructor_log"
if [ "$(sha256sum "$script_dir/fixtures/kani_constructor/Cargo.lock" | cut -d ' ' -f 1)" != "$constructor_lock_sha256" ]; then
  echo "constructor fixture lock changed during qualification" >&2
  exit 1
fi
if [ ! -f "$source_dir/target/awaken-kani-build.sha256" ]; then
  receipt_tmp="$(mktemp "$source_dir/target/awaken-kani-build.XXXXXX")"
  kani_build_digest > "$receipt_tmp"
  mv "$receipt_tmp" "$source_dir/target/awaken-kani-build.sha256"
fi
if [ ! -f "$backend_root/$qualified_receipt" ]; then
  # The actual Kani fixture must also accept this exact GOTO/backend package.
  # Recheck bytes after execution, then publish; no failed fixture can bless it.
  identity="$(backend_identity)"
  [ "$identity" = "$(cat "$backend_root/$qualified_pending")" ]
  mv -T "$backend_root/$qualified_pending" "$backend_root/$qualified_receipt"
fi
ln -sfnT "$source_dir" "$cache_root/current"

echo "Pinned Kani is ready at $source_dir"
echo "scripts/ci/check_formal.sh discovers this cache automatically."
