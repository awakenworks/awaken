#!/usr/bin/env bash
set -euo pipefail

# The one entry point for running a Kani harness, for CI and for a person at a
# terminal alike.
#
# Two facts are needed before `cargo kani` can work in this workspace, and
# neither is discoverable from the error it produces without them:
#
#   1. The compiler and backend are a reviewed source build, not whatever
#      `cargo-kani` happens to be on PATH. `bootstrap_kani.sh` owns those pins.
#   2. Kani's driver reads its goto artifacts from `target/kani/debug`, so it
#      assumes Cargo's build directory sits inside the target directory. A
#      developer or CI that separates them (`build.build-dir`, common when
#      intermediates live on another disk) makes valid compilation end in a bare
#      `No such file or directory`.
#
# Fact 2 used to live only inside one function of check_formal.sh, so every
# direct invocation rediscovered it from that ENOENT. Both facts now have one
# reachable owner instead.
#
# Usage: scripts/ci/kani.sh -p <package> [cargo-kani args...]
#   e.g. scripts/ci/kani.sh -p awaken-ext-permission --harness my_proof

awaken_kani_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$awaken_kani_root"

source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$awaken_kani_root"

kani_selection="$(scripts/ci/bootstrap_kani.sh --paths)"
mapfile -t kani_paths <<< "$kani_selection"
readonly kani_source_dir="${kani_paths[0]}"
readonly kani_backend_dir="${kani_paths[1]}"
export AWAKEN_KANI_BACKEND_DIR="$kani_backend_dir"
if [ -x "$kani_source_dir/scripts/cargo-kani" ]; then
  export PATH="$kani_source_dir/scripts:$kani_backend_dir:$PATH"
fi

# Keep the compiler and the driver in the one tree the driver reads. The rule
# itself belongs to the cargo-target authority, shared with the batch runner.
awaken_configure_kani_driver_target

exec cargo kani "$@"
