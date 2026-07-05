#!/usr/bin/env bash
# Run Rust workspace checks when a Rust workspace exists.
#
# Output contract: one "✓ <step>" line per passing step; on failure, only the
# de-noised error block plus a "→ fix:" line with the exact command that
# resolves it. Cargo's Checking/Compiling progress is stripped so a failure is
# never buried under build spam.
set -uo pipefail
cd "$(dirname "$0")/../.."

mode="${1:---full}"

if [ ! -f Cargo.toml ]; then
  echo "OK - no Cargo.toml; skipping Rust checks."
  exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: Cargo is required for Rust checks." >&2
  exit 1
fi

locked=()
if [ -f Cargo.lock ]; then
  locked=(--locked)
fi

# Cargo progress lines carry no diagnostic value; drop them so the error block
# is the only thing left on screen when a step fails.
NOISE='^[[:space:]]*(Checking|Compiling|Blocking|Downloading|Downloaded|Updating|Locking|Adding|Removing|Finished|Building|Fresh|Installing) '

fail=0
# step "<label>" "<fix hint>" <command...>
#   success → one "✓ <label>" line, command output suppressed.
#   failure → "✗ <label>", the de-noised output, then "→ fix: <hint>".
# Returns the command's real exit code so callers can branch (skip tests when
# the build is already broken).
step() {
  local label="$1" fix="$2"; shift 2
  local out rc
  if out=$("$@" 2>&1); then
    echo "✓ $label"
    return 0
  fi
  rc=$?
  {
    echo ""
    echo "✗ $label"
    printf '%s\n' "$out" | grep -Ev "$NOISE"
    echo ""
    echo "→ fix: $fix"
  } >&2
  fail=1
  return "$rc"
}

step "fmt" "cargo fmt --all" \
  cargo fmt --all -- --check
step "crate-boundaries" "remove the illegal dependency shown above (a lower layer must not import a higher one)" \
  python3 scripts/ci/check_crate_boundaries.py

case "$mode" in
  --quick)
    step "check" "fix the compile errors shown above" \
      cargo check --workspace --all-targets "${locked[@]}"
    ;;
  --full)
    if step "clippy" "review the errors above; auto-fixable lints: cargo clippy --fix --all-targets --allow-dirty" \
      cargo clippy --workspace --all-targets "${locked[@]}" -- -D warnings; then
      step "test" "re-run a single failure with: cargo test --workspace <test_name> -- --nocapture" \
        cargo test --workspace "${locked[@]}"
    else
      echo "   (skipping tests until clippy compiles)" >&2
    fi
    ;;
  *)
    echo "ERROR: unknown mode: $mode (expected --quick or --full)" >&2
    exit 1
    ;;
esac

[ "$fail" -eq 0 ] || exit 1
