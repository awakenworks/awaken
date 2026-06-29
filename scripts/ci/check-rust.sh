#!/usr/bin/env bash
# Run Rust workspace checks when a Rust workspace exists.
set -euo pipefail
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

echo "-> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "-> python3 scripts/ci/check_crate_boundaries.py"
python3 scripts/ci/check_crate_boundaries.py

case "$mode" in
  --quick)
    echo "-> cargo check --workspace --all-targets ${locked[*]}"
    cargo check --workspace --all-targets "${locked[@]}"
    ;;
  --full)
    echo "-> cargo clippy --workspace --all-targets ${locked[*]} -- -D warnings"
    cargo clippy --workspace --all-targets "${locked[@]}" -- -D warnings
    echo "-> cargo test --workspace ${locked[*]}"
    cargo test --workspace "${locked[@]}"
    ;;
  *)
    echo "ERROR: unknown mode: $mode (expected --quick or --full)" >&2
    exit 1
    ;;
esac
