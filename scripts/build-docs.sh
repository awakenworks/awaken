#!/usr/bin/env bash
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if ! command -v mdbook >/dev/null 2>&1; then
    echo "error: mdbook is required. Install with: cargo install mdbook --locked --version 0.5.2"
    exit 1
fi

if ! command -v mdbook-mermaid >/dev/null 2>&1; then
    echo "error: mdbook-mermaid is required. Install with: cargo install mdbook-mermaid --locked"
    exit 1
fi

echo "==> Building cargo doc..."
cargo doc --workspace --no-deps --manifest-path "$WORKSPACE_ROOT/Cargo.toml"

echo "==> Installing Mermaid support..."
mdbook-mermaid install "$WORKSPACE_ROOT/docs/book"

DOC_TEST_TARGET_DIR="$(mktemp -d "$WORKSPACE_ROOT/target/mdbook-doctest.XXXXXX")"
trap 'rm -rf "$DOC_TEST_TARGET_DIR"' EXIT

echo "==> Building mdBook doctest support crates..."
CARGO_TARGET_DIR="$DOC_TEST_TARGET_DIR" cargo build \
    --manifest-path "$WORKSPACE_ROOT/Cargo.toml" \
    -p tirea-state \
    -p tirea-contract \
    -p tirea

echo "==> Testing mdBook Rust snippets..."
mdbook test "$WORKSPACE_ROOT/docs/book" -L "$DOC_TEST_TARGET_DIR/debug/deps"

echo "==> Building mdBook..."
mdbook build "$WORKSPACE_ROOT/docs/book"

# Copy cargo doc output into book output for unified serving
if [ -d "$WORKSPACE_ROOT/target/book" ] && [ -d "$WORKSPACE_ROOT/target/doc" ]; then
    cp -r "$WORKSPACE_ROOT/target/doc" "$WORKSPACE_ROOT/target/book/doc"
    echo "==> Unified docs at: target/book/index.html"
    echo "    API docs at:     target/book/doc/tirea_state/index.html"
fi
