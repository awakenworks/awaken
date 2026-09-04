#!/usr/bin/env bash
# Compile the Managed SDK behavior owner against this exact checkout. The
# package command is also invoked directly by pre-commit, outside check-rust,
# so it must establish the repository's one Cargo target boundary itself.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"

cargo test --manifest-path Cargo.toml \
  -p awaken-protocol-managed --all-features --locked
