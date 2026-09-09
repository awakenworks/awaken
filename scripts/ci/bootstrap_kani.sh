#!/usr/bin/env bash
set -euo pipefail

# Kani 0.67's published bundle contains Rust 1.93, below this workspace's Rust
# 1.96 floor. Pin a reviewed upstream source revision whose own toolchain is
# Rust 1.97 nightly; never bypass Cargo's rust-version check.
readonly kani_revision="e43ae2c255f11dd6b0f91468c40e0b3403b810c5"
readonly kani_nightly="nightly-2026-05-01"
readonly kani_backend_version="0.67.0"
readonly kani_patch_sha256="556129cbcc5e1d4d8f7f191131b5a3afe8010538d87eaf72cbd5bebe4474c0ba"
readonly script_dir="$(cd "$(dirname "$0")" && pwd)"
readonly compiler_patch="$script_dir/patches/kani-wide-niche.patch"
readonly cache_root="${AWAKEN_KANI_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/awaken-kani}"
# Keep the unmodified source cache intact, including any in-flight proof runs.
source_dir="$cache_root/$kani_revision-${kani_patch_sha256:0:12}"
if [ "$(sha256sum "$compiler_patch" | cut -d ' ' -f 1)" != "$kani_patch_sha256" ]; then
  echo "Kani compiler patch digest differs from its reviewed pin" >&2
  exit 1
fi

# One source check serves both installation and the actual formal runner.
verify_kani_source() {
  if [ "$(git -C "$source_dir" rev-parse HEAD)" != "$kani_revision" ]; then
    echo "Kani cache contains an unexpected revision: $source_dir" >&2
    exit 1
  fi
  git -C "$source_dir" diff --cached --exit-code >/dev/null
  if [ -n "$(git -C "$source_dir" ls-files --others --exclude-standard)" ]; then
    echo "Kani source cache contains untracked inputs: $source_dir" >&2
    exit 1
  fi
  if [ "${1:-}" = "allow-clean" ] && git -C "$source_dir" diff --quiet; then
    return 0
  fi
  if ! git -C "$source_dir" diff --no-ext-diff --no-textconv --no-color --binary HEAD |
       cmp -s "$compiler_patch" -; then
    echo "Kani source cache differs from its reviewed compiler patch: $source_dir" >&2
    exit 1
  fi
}

kani_build_digest() {
  (cd "$source_dir" && find target/kani -type f -print0 |
    sort -z | xargs -0 sha256sum | sha256sum | cut -d ' ' -f 1)
}

verify_kani_build() {
  local receipt="$source_dir/target/awaken-kani-build.sha256"
  if [ ! -f "$receipt" ] || [ "$(cat "$receipt")" != "$(kani_build_digest)" ]; then
    echo "Kani build is missing or differs from its qualified cache: $source_dir" >&2
    exit 1
  fi
}

if [ "${1:-}" = "--verify-source" ] && [ "$#" -eq 2 ]; then
  source_dir="$2"
  verify_kani_source
  verify_kani_build
  printf '%s\n' "source=$source_dir" "revision=$kani_revision" \
    "patch_sha256=$kani_patch_sha256" \
    "build_sha256=$(cat "$source_dir/target/awaken-kani-build.sha256")"
  exit 0
elif [ "$#" -ne 0 ]; then
  echo "usage: bootstrap_kani.sh [--verify-source PATH]" >&2
  exit 1
fi

# The source build supplies the compiler/driver. The released setup supplies
# the matching CBMC/GOTO backend binaries, which developer builds intentionally
# do not download.
backend_dir="${AWAKEN_KANI_BACKEND_DIR:-$HOME/.kani/kani-$kani_backend_version/bin}"
if [ ! -x "$backend_dir/cbmc" ] || [ ! -x "$backend_dir/goto-cc" ]; then
  cargo install --locked kani-verifier --version "$kani_backend_version"
  cargo kani setup
fi
if [ ! -x "$backend_dir/cbmc" ] || [ ! -x "$backend_dir/goto-cc" ]; then
  echo "Kani backend is incomplete: expected cbmc and goto-cc in $backend_dir" >&2
  exit 1
fi

if [ ! -d "$source_dir/.git" ]; then
  mkdir -p "$cache_root"
  git clone --no-checkout https://github.com/model-checking/kani.git "$source_dir"
  git -C "$source_dir" checkout --detach "$kani_revision"
  git -C "$source_dir" submodule update --init --depth 1 charon
fi
# The only local compiler change keeps rustc's full u128 niche offset. Refuse
# unrelated cached source edits rather than silently blessing another compiler.
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
  cargo build-dev
  popd >/dev/null
fi
# Qualify the formerly crashing layout before making this cache discoverable.
PATH="$backend_dir:$PATH" "$source_dir/scripts/kani" \
  "$script_dir/fixtures/kani_wide_niche.rs" \
  --target-dir "$source_dir/target/wide-niche-regression" --output-format terse
if [ ! -f "$source_dir/target/awaken-kani-build.sha256" ]; then
  receipt_tmp="$(mktemp "$source_dir/target/awaken-kani-build.XXXXXX")"
  kani_build_digest > "$receipt_tmp"
  mv "$receipt_tmp" "$source_dir/target/awaken-kani-build.sha256"
fi
ln -sfn "$source_dir" "$cache_root/current"

echo "Pinned Kani is ready at $source_dir"
echo "scripts/ci/check_formal.sh discovers this cache automatically."
