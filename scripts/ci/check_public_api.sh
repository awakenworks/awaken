#!/usr/bin/env bash
# Architecture fitness function: public API surface snapshots.
#
# Guards INVARIANTS G2 / G6 / G10: the runtime's public surface stays narrow and
# changes only on purpose. For each workspace crate, regenerate the public API
# and diff it against the committed snapshot in public-api/<crate>.txt. Any drift
# fails the check — a reviewer must look at the diff and `--bless` it on purpose.
# This is what makes the planned simplifications (resolver renaming, RunExecutor
# narrowing) safe: an accidental surface change turns the check red.
#
# Usage:
#   scripts/ci/check_public_api.sh          # check; fails on drift
#   scripts/ci/check_public_api.sh --bless   # accept current surface as the snapshot
#
# Requires cargo-public-api (which uses a nightly rustdoc). If it is not
# installed the check skips with a hint, so the repo works before tooling is set
# up. Wire as required in CI once the toolchain is provisioned.
set -euo pipefail
cd "$(dirname "$0")/../.."

if ! command -v cargo-public-api >/dev/null 2>&1; then
  echo "cargo-public-api not installed; skipping public API check (cargo install cargo-public-api)"
  exit 0
fi

bless=0
[ "${1:-}" = "--bless" ] && bless=1

mkdir -p public-api

# Workspace member package names (no registry resolution).
crates=$(cargo metadata --no-deps --format-version 1 \
  | python3 -c "import json,sys; print('\n'.join(sorted(p['name'] for p in json.load(sys.stdin)['packages'])))")

# Crates excluded from the public-API gate. cargo-public-api needs a nightly
# rustdoc, and awaken-store-postgres pulls awaken-scoped-migration (rust-version
# 1.96) which the available nightly toolchain predates, so the tool cannot build
# it here. Its public surface is small and reviewed in code; re-enable when the
# nightly toolchain reaches 1.96.
excluded="awaken-store-postgres"

fail=0
for c in $crates; do
  case " $excluded " in *" $c "*) echo "skipped $c (excluded from public-API gate)"; continue;; esac
  snap="public-api/$c.txt"
  if ! cur=$(cargo +nightly public-api -p "$c" --simplified 2>/dev/null); then
    echo "✗ failed to compute public API for $c"; fail=1; continue
  fi
  if [ "$bless" = 1 ] || [ ! -f "$snap" ]; then
    printf '%s\n' "$cur" > "$snap"
    echo "blessed $snap"
  elif ! diff -u "$snap" <(printf '%s\n' "$cur") >/dev/null; then
    echo "✗ public API drift in $c:"
    diff -u "$snap" <(printf '%s\n' "$cur") || true
    echo "  -> review the change; run scripts/ci/check_public_api.sh --bless to accept"
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "✗ public API check failed" >&2
  exit 1
fi
echo "✓ public API snapshots match"
