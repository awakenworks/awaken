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
# Requires cargo-public-api (which uses a nightly rustdoc).
#
# Tool admission is fail-closed BY DEFAULT. It used to be fail-open: three
# `exit 0` paths (tool absent, wrong tool version, nightly absent) reported
# success, and only the one caller that remembered `--require-tools` demanded
# otherwise. So the gate could report "public API" green having compared zero
# crates, on a machine where nothing could compare any. A caller who genuinely
# wants that has to say so now, with `--allow-absent-tools`, and gets a line
# saying what the run did not check. `--require-tools` is retained because the
# release manifest names it; it is the default and therefore a no-op.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"

# Snapshot text is an output of both cargo-public-api and rustdoc. Keep their
# versions here, at the one snapshot-generation authority, so a tool upgrade is
# reviewed together with the resulting API diff instead of producing alias/order
# churn on whichever moving nightly happens to be installed.
readonly PUBLIC_API_TOOL_VERSION="0.52.0"
readonly PUBLIC_API_NIGHTLY="nightly-2026-07-14"

bless=0
require_tools=1
for argument in "$@"; do
  case "$argument" in
    --bless) bless=1 ;;
    --require-tools) require_tools=1 ;;
    --allow-absent-tools) require_tools=0 ;;
    *)
      echo "usage: scripts/ci/check_public_api.sh [--bless] [--require-tools|--allow-absent-tools]" >&2
      exit 2
      ;;
  esac
done

# A probe has three outcomes, not two. "absent" is a fact about this machine and
# a caller may choose to accept it; "undecidable" is a fact about the probe --
# the tool is here but would not say what it is -- and no caller may accept
# that, because it is not evidence of anything. Both are refusals by default.
absent_tool() {  # absent_tool <what> <how to get it>
  if [ "$require_tools" -eq 1 ]; then
    echo "$1 is required by this gate; pass --allow-absent-tools to accept an unchecked run" >&2
    echo "    install: $2" >&2
    exit 1
  fi
  echo "SKIPPED: $1 is absent, so NO crate's public API was compared ($2)"
  exit 0
}
undecidable_tool() {  # undecidable_tool <what could not be decided>
  echo "the public API gate could not decide its own tooling: $1" >&2
  echo "    this is not an absent tool and --allow-absent-tools does not cover it;" >&2
  echo "    a probe that will not answer is not evidence that anything was checked." >&2
  exit 1
}

if ! command -v cargo-public-api >/dev/null 2>&1; then
  absent_tool "cargo-public-api" \
    "cargo install --locked cargo-public-api --version $PUBLIC_API_TOOL_VERSION"
fi

# The installed tool is here, so its version is a question it must answer. A
# query that fails, or prints something this cannot parse, leaves the pinned
# rendering undecided -- and the snapshots are an output of that exact version.
if ! version_output="$(cargo public-api --version 2>&1)"; then
  undecidable_tool "cargo public-api --version failed: $(printf '%s' "$version_output" | head -n 1)"
fi
installed_public_api_version="$(printf '%s\n' "$version_output" | awk '{print $2}' | head -n 1)"
case "$installed_public_api_version" in
  [0-9]*) ;;
  *) undecidable_tool "cargo public-api --version printed no version: $(printf '%s' "$version_output" | head -n 1)" ;;
esac
if [ "$installed_public_api_version" != "$PUBLIC_API_TOOL_VERSION" ]; then
  absent_tool "cargo-public-api $PUBLIC_API_TOOL_VERSION (found $installed_public_api_version)" \
    "cargo install --locked cargo-public-api --version $PUBLIC_API_TOOL_VERSION"
fi

if ! rustc +"$PUBLIC_API_NIGHTLY" --version >/dev/null 2>&1; then
  absent_tool "$PUBLIC_API_NIGHTLY" \
    "rustup toolchain install $PUBLIC_API_NIGHTLY --profile minimal"
fi

if [ "$bless" = 1 ]; then
  mkdir -p public-api
fi

# The snapshotted crates are the members that declare
# `[package.metadata.awaken] api_snapshot = true`: a surface something outside
# the compiler's reach depends on (an INVARIANTS row, the out-of-workspace fuzz
# crate, the plugin SPI). Every other member's consumers are workspace crates,
# and the compiler already refuses a breaking change there, so a snapshot of it
# was a second, hand-blessed copy of what `cargo check` proves -- and the most
# co-edited file set in the tree (ADR-0081, 2026-09-28 amendment). Selection is
# positive and lives beside the crate, so a new member is unsnapshotted unless
# its manifest says otherwise.
if ! metadata_json="$(cargo metadata --no-deps --format-version 1)"; then
  echo "cargo metadata failed; the public API gate cannot enumerate its crates" >&2
  exit 1
fi
crates=$(printf '%s' "$metadata_json" | python3 -c "
import json, sys
packages = json.load(sys.stdin)['packages']
selected = sorted(
    p['name'] for p in packages
    if ((p.get('metadata') or {}).get('awaken') or {}).get('api_snapshot') is True
)
# Native Windows Python translates print's LF into CRLF. Shell word splitting
# leaves CR attached to all but the final package name; emit protocol bytes.
sys.stdout.buffer.write(('\n'.join(selected) + '\n').encode('utf-8'))
")

# A snapshot file for a crate that is not selected is an orphan: nothing checks
# it, so it would silently rot while looking like a reviewed surface.
orphans=()
if [ -d public-api ]; then
  for snap in public-api/*.txt; do
    [ -e "$snap" ] || continue
    name="$(basename "$snap" .txt)"
    if ! printf '%s\n' $crates | grep -qx "$name"; then
      orphans+=("$snap")
    fi
  done
fi
if [ "${#orphans[@]}" -ne 0 ]; then
  {
    echo "❌ public API snapshots without a selecting crate: ${orphans[*]}"
    echo "   Delete them, or declare api_snapshot = true in the crate's"
    echo "   [package.metadata.awaken] with the reason its surface needs review."
  } >&2
  exit 1
fi

fail=0
drifted=()      # crates whose surface changed
compute_fail=0  # a crate whose surface could not be computed
compare_fail=0  # a baseline whose comparison could not be observed
compared=0      # crates whose surface was actually compared against a snapshot
# Omit the compiler-derived impls (blanket, auto-trait, and auto-derived). These are
# NOT part of the crate's authored surface and their rendering is NIGHTLY-VERSION
# SENSITIVE (e.g. `impl UnsafeUnpin`, `impl Send/Sync/Unpin` blocks appear or vanish
# between rustdoc versions). Without this, a bless on one nightly diffs against a
# check on another by hundreds of spurious lines — the snapshots become
# non-reproducible across machines. Omitting them makes the gate depend only on the
# authored API (structs, fns, real trait impls). The remaining rustdoc spelling
# and ordering is made reproducible by PUBLIC_API_NIGHTLY above.
omit_flags="--omit blanket-impls,auto-trait-impls,auto-derived-impls"
compute_stderr="$(mktemp)"
trap 'rm -f "$compute_stderr"' EXIT
for c in $crates; do
  snap="public-api/$c.txt"
  if ! cur=$(cargo +"$PUBLIC_API_NIGHTLY" public-api -p "$c" --simplified $omit_flags 2>"$compute_stderr"); then
    echo "✗ failed to compute public API for $c"
    # The reason is the actionable part (a missing build tool, a rustdoc error);
    # hiding it turned an environment fault into an unexplained gate failure.
    tail -n 20 "$compute_stderr" | sed 's/^/    /'
    fail=1; compute_fail=1; continue
  fi
  if [ "$bless" = 1 ]; then
    printf '%s\n' "$cur" > "$snap"
    echo "blessed $snap"
  elif [ ! -f "$snap" ]; then
    echo "✗ missing public API snapshot for $c"
    drifted+=("$c")
    fail=1
  else
    compared=$((compared + 1))
    if d=$(diff -u "$snap" <(printf '%s\n' "$cur")); then
      continue
    else
      comparison_status=$?
    fi
    if [ "$comparison_status" -ne 1 ]; then
      echo "✗ failed to compare public API for $c (diff status $comparison_status)"
      fail=1
      compare_fail=1
      continue
    fi
    added=$(printf '%s\n' "$d" | grep -cE '^\+[^+]' || true)
    removed=$(printf '%s\n' "$d" | grep -cE '^-[^-]' || true)
    echo "✗ public API drift in $c  (+$added / -$removed)"
    printf '%s\n' "$d" | sed 's/^/    /'
    drifted+=("$c")
    fail=1
  fi
done

# A run that compared nothing is not a passing run. The loop above can end with
# fail=0 and an empty workspace, an empty crate list, or a `--bless` that only
# wrote: none of those observed a surface, and all of them used to print the
# same "snapshots match" line as a real comparison.
if [ "$bless" = 0 ] && [ "$compared" -eq 0 ] && [ "$fail" -eq 0 ]; then
  {
    echo "❌ the public API gate compared 0 crates and would have reported success."
    echo "   Workspace members enumerated: $(printf '%s\n' $crates | grep -c . || true)."
    echo "   A gate that observed nothing has proved nothing; find out why the member"
    echo "   list or the snapshot directory is empty before trusting this result."
  } >&2
  exit 1
fi

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
    if [ "$compare_fail" -ne 0 ]; then
      echo "❌ could not compare the public API for some crate (see ✗ lines above)."
    fi
  } >&2
  exit 1
fi
if [ "$bless" = 1 ]; then
  echo "✓ public API snapshots blessed"
else
  echo "✓ public API snapshots match ($compared crates compared)"
fi
