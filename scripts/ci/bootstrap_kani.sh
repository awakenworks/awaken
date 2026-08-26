#!/usr/bin/env bash
set -euo pipefail

# Kani 0.67's published bundle contains Rust 1.93, below this workspace's Rust
# 1.96 floor. Pin a reviewed upstream source revision whose own toolchain is
# Rust 1.97 nightly; never bypass Cargo's rust-version check.
readonly kani_revision="e43ae2c255f11dd6b0f91468c40e0b3403b810c5"
readonly kani_nightly="nightly-2026-05-01"
readonly kani_backend_version="0.67.0"
readonly cache_root="${AWAKEN_KANI_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/awaken-kani}"
readonly source_dir="$cache_root/$kani_revision"

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
if [ "$(git -C "$source_dir" rev-parse HEAD)" != "$kani_revision" ]; then
  echo "Kani cache contains an unexpected revision: $source_dir" >&2
  exit 1
fi

rustup toolchain install "$kani_nightly" --profile minimal \
  --component llvm-tools,rustc-dev,rust-src,rustfmt
pushd "$source_dir" >/dev/null
cargo build-dev
popd >/dev/null
ln -sfn "$source_dir" "$cache_root/current"

echo "Pinned Kani is ready at $source_dir"
echo "scripts/ci/check_formal.sh discovers this cache automatically."
