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
# Requires cargo-public-api (which uses a nightly rustdoc). Local checks may
# explicitly observe a missing-tool skip; the release gate passes
# ``--require-tools`` and fails closed.
set -euo pipefail
cd "$(dirname "$0")/../.."

bless=0
require_tools=0
for argument in "$@"; do
  case "$argument" in
    --bless) bless=1 ;;
    --require-tools) require_tools=1 ;;
    *) echo "usage: scripts/ci/check_public_api.sh [--bless] [--require-tools]" >&2; exit 2 ;;
  esac
done

if ! command -v cargo-public-api >/dev/null 2>&1; then
  if [ "$require_tools" -eq 1 ]; then
    echo "cargo-public-api is required by the release gate" >&2
    exit 1
  fi
  echo "cargo-public-api not installed; skipping public API check (cargo install cargo-public-api)"
  exit 0
fi

mkdir -p public-api

# Workspace member package names (no registry resolution).
crates=$(cargo metadata --no-deps --format-version 1 \
  | python3 -c "import json,sys; print('\n'.join(sorted(p['name'] for p in json.load(sys.stdin)['packages'])))")

# Nightly 1.99 can document every current workspace member, including the
# rust-version 1.96 scoped-migration dependency. Product adapters and startup
# crates are intentionally included: a changing product surface still needs an
# explicit reviewed snapshot update rather than an untracked exception.

fail=0
drifted=()      # crates whose surface changed
compute_fail=0  # a crate whose surface could not be computed
# Omit the compiler-derived impls (blanket, auto-trait, and auto-derived). These are
# NOT part of the crate's authored surface and their rendering is NIGHTLY-VERSION
# SENSITIVE (e.g. `impl UnsafeUnpin`, `impl Send/Sync/Unpin` blocks appear or vanish
# between rustdoc versions). Without this, a bless on one nightly diffs against a
# check on another by hundreds of spurious lines — the snapshots become
# non-reproducible across machines. Omitting them makes the gate depend only on the
# authored API (structs, fns, real trait impls), so any nightly yields the same file.
omit_flags="--omit blanket-impls,auto-trait-impls,auto-derived-impls"
for c in $crates; do
  snap="public-api/$c.txt"
  if ! cur=$(cargo +nightly public-api -p "$c" --simplified $omit_flags 2>/dev/null); then
    echo "✗ failed to compute public API for $c"; fail=1; compute_fail=1; continue
  fi
  if [ "$bless" = 1 ] || [ ! -f "$snap" ]; then
    printf '%s\n' "$cur" > "$snap"
    echo "blessed $snap"
  else
    d=$(diff -u "$snap" <(printf '%s\n' "$cur") || true)
    if [ -n "$d" ]; then
      added=$(printf '%s\n' "$d" | grep -cE '^\+[^+]' || true)
      removed=$(printf '%s\n' "$d" | grep -cE '^-[^-]' || true)
      echo "✗ public API drift in $c  (+$added / -$removed)"
      printf '%s\n' "$d" | sed 's/^/    /'
      drifted+=("$c")
      fail=1
    fi
  fi
done

if [ "$fail" -ne 0 ]; then
  {
    echo ""
    if [ "${#drifted[@]}" -ne 0 ]; then
      echo "❌ public API changed in: ${drifted[*]}"
      echo "   If the change is intended, accept the new surface with:"
      echo "       scripts/ci/check_public_api.sh --bless"
      echo "   then commit the updated public-api/*.txt snapshots."
    fi
    if [ "$compute_fail" -ne 0 ]; then
      echo "❌ could not compute the public API for some crate (see ✗ lines above)."
    fi
  } >&2
  exit 1
fi
echo "✓ public API snapshots match"
