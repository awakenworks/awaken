#!/usr/bin/env bash
# Test one crate the way the full gate tests it.
#
# A bare `cargo nextest run -p <crate>` misses what the full gate supplies: the
# whole authored graph `.config/rust-tests.toml` selects (only root features
# can miss a peer's contribution), the Hand companion some tests launch beside
# executable, and a build directory equal to the target directory so that
# companion is found. Each gap produced a red test that was not a failure.
# scripts/ci/crate_test.py derives all three from the profile and Cargo's own
# resolve, prints every command it runs, and runs them in order.
#
# `--doc` runs the crate's doctests (or, given a failing doctest's name, its file's)
# the way the gate's doctest step does: whole graph, RUSTC_BOOTSTRAP=1, so a
# compile_fail block's pinned E-code is checked. Every failing gate test,
# test-build and doctest step prints the command here that reproduces it.
#
# usage: scripts/ci/test.sh [--dry-run] -p <crate> [nextest arguments...]
#        scripts/ci/test.sh [--dry-run] --doc -p <crate> ['<doctest name>']
#        scripts/ci/test.sh --self-test
#        scripts/ci/test.sh --affected --base <revision>
#        scripts/ci/test.sh --benchmark [ordinary benchmark arguments...]
set -uo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys
awaken_append_repo_tools

case "${1:-}" in
  --affected)
    shift
    exec python3 scripts/ci/rust_test_plan.py --run "$@"
    ;;
  --benchmark)
    shift
    exec python3 scripts/ci/benchmark_rust_build.py "$@"
    ;;
  --self-test)
    [ "$#" -eq 1 ] || { echo "usage: scripts/ci/test.sh [--dry-run] [--doc] -p <crate> [nextest arguments...] | --self-test" >&2; exit 2; }
    exec python3 scripts/ci/crate_test.py --self-test
    ;;
  -p|--dry-run|--doc) exec python3 scripts/ci/crate_test.py "$@" ;;
  *)
    echo "usage: scripts/ci/test.sh [--dry-run] [--doc] -p <crate> [nextest arguments... | '<doctest name>'] | --affected --base <revision> | --benchmark [args...] | --self-test" >&2
    exit 2
    ;;
esac
