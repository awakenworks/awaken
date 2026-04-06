#!/usr/bin/env bash
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BOOK_ROOT="$WORKSPACE_ROOT/docs/book"
BOOK_OUTPUT_ROOT="$WORKSPACE_ROOT/target/book"
BOOK_STAGE_ROOT="$WORKSPACE_ROOT/target/book-stage"

if ! command -v mdbook >/dev/null 2>&1; then
    echo "error: mdbook is required. Install with: cargo install mdbook --locked"
    exit 1
fi

if ! command -v mdbook-mermaid >/dev/null 2>&1; then
    echo "error: mdbook-mermaid is required. Install with: cargo install mdbook-mermaid --locked"
    exit 1
fi

echo "==> Building cargo doc..."
cargo doc --workspace --no-deps --manifest-path "$WORKSPACE_ROOT/Cargo.toml"

prepare_book_stage() {
    local variant="$1"
    local stage_dir="$BOOK_STAGE_ROOT/$variant"

    rm -rf "$stage_dir"
    mkdir -p "$stage_dir/src"

    cp "$BOOK_ROOT/book.toml" "$stage_dir/book.toml"

    if [ -d "$BOOK_ROOT/theme" ]; then
        cp -a "$BOOK_ROOT/theme" "$stage_dir/theme"
        cp "$BOOK_ROOT/theme/language-switcher.css" "$stage_dir/language-switcher.css"
        cp "$BOOK_ROOT/theme/language-switcher.js" "$stage_dir/language-switcher.js"
    fi

    case "$variant" in
        en)
            cp -a "$BOOK_ROOT/src/." "$stage_dir/src/"
            rm -rf "$stage_dir/src/zh-CN"
            ;;
        zh-CN)
            cp -a "$BOOK_ROOT/src/zh-CN/." "$stage_dir/src/"
            sed -i 's/^language = "en"$/language = "zh-CN"/' "$stage_dir/book.toml"
            ;;
        *)
            echo "error: unknown book variant: $variant"
            exit 1
            ;;
    esac
}

build_book_variant() {
    local variant="$1"
    local destination="$2"
    local stage_dir="$BOOK_STAGE_ROOT/$variant"

    prepare_book_stage "$variant"

    echo "==> Installing Mermaid support for $variant..."
    mdbook-mermaid install "$stage_dir"
    cp "$BOOK_ROOT/theme/mermaid-init.js" "$stage_dir/mermaid-init.js"

    echo "==> Building mdBook ($variant)..."
    mdbook build "$stage_dir" -d "$destination"
}

rm -rf "$BOOK_OUTPUT_ROOT" "$BOOK_STAGE_ROOT"
mkdir -p "$BOOK_OUTPUT_ROOT" "$BOOK_STAGE_ROOT"

# Note: to test book code examples, use scripts/test-book.sh (build and test are separate)
build_book_variant "en" "$BOOK_OUTPUT_ROOT"
build_book_variant "zh-CN" "$BOOK_OUTPUT_ROOT/zh-CN"

# Copy cargo doc output into book output for unified serving
if [ -d "$BOOK_OUTPUT_ROOT" ] && [ -d "$WORKSPACE_ROOT/target/doc" ]; then
    cp -r "$WORKSPACE_ROOT/target/doc" "$BOOK_OUTPUT_ROOT/doc"
    echo "==> Unified docs at: target/book/index.html"
    echo "    API docs at:     target/book/doc/awaken/index.html"
    echo "    Chinese docs at: target/book/zh-CN/index.html"
fi
