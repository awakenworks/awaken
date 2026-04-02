#!/usr/bin/env bash
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "Testing book code examples via awaken-doctest..."
cargo test -p awaken-doctest --doc --manifest-path "$WORKSPACE_ROOT/Cargo.toml"
