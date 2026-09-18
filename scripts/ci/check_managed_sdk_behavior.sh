#!/usr/bin/env bash
# Compile the Managed SDK behavior owner against this exact checkout. The
# package command is also invoked directly by pre-commit, outside check-rust,
# so it must establish the repository's one Cargo target boundary itself.
set -euo pipefail
cd "$(dirname "$0")/../.."

# This gate takes no arguments. Accepting one silently is how a caller's
# intent ("only the links", "--quick") disappears into a full run that
# answers a different question; refuse before any work.
if [ "$#" -ne 0 ]; then
  echo "usage: scripts/ci/check_managed_sdk_behavior.sh" >&2
  exit 2
fi
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"

cargo test --manifest-path Cargo.toml \
  -p awaken-protocol-managed --all-features --locked
