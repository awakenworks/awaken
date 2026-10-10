#!/usr/bin/env bash
# Test one crate the way the full gate tests it.
#
# A bare `cargo nextest run -p <crate>` misses what the full gate supplies: the
# features `.config/rust-tests.toml` turns on (Cargo unifies them onto the
# crate), the Hand companion binary some tests launch from beside their own
# executable, and a build directory equal to the target directory so that
# companion is found. Each gap produced a red test that was not a failure.
# scripts/ci/crate_test.py derives all three from the profile and Cargo's own
# resolve, prints every command it runs, and runs them in order.
#
# usage: scripts/ci/test.sh [--dry-run] -p <crate> [nextest arguments...]
#        scripts/ci/test.sh --self-test
set -uo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys
awaken_append_repo_tools

case "${1:-}" in
  --self-test)
    [ "$#" -eq 1 ] || { echo "usage: scripts/ci/test.sh [--dry-run] -p <crate> [nextest arguments...] | --self-test" >&2; exit 2; }
    exec python3 scripts/ci/crate_test.py --self-test
    ;;
  -p|--dry-run) exec python3 scripts/ci/crate_test.py "$@" ;;
  *)
    echo "usage: scripts/ci/test.sh [--dry-run] -p <crate> [nextest arguments...] | --self-test" >&2
    exit 2
    ;;
esac
