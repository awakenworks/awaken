#!/usr/bin/env bash
# E2E line coverage of the served Rust binary (`awaken-server-local`).
#
# Instruments the workspace with cargo-llvm-cov (continuous mode, so profiles
# survive SIGINT/SIGKILL of spawned servers), drives every DETERMINISTIC e2e
# chain against the instrumented binary, and reports line coverage. The
# real-model chain (`test:real`) is additionally run when ANTHROPIC_API_KEY /
# KIMI_API_KEY is set.
#
# Scope: coverage is measured over the code REACHABLE from the served binary.
# Files excluded below are unreachable BY DESIGN from any e2e run — each with
# the reason; revisit an exclusion when its wiring changes:
#   - awaken-run-ingress/src/memory.rs    in-memory reference implementation
#     ("executable specification" per its module doc); the server always uses
#     the SQLite dispatch store.
#   - awaken-run-ingress/src/postgres.rs  requires a live PostgreSQL.
#   - awaken-ext-mcp/src/stdio.rs         the server wires the HTTP MCP
#     transport only; stdio is covered by the crate's own Rust tests.
#   - awaken-sandbox-local/src/namespace.rs  no product wiring selects the
#     namespace sandbox tier yet (ADR-0041 later slice).
#
# Usage: scripts/ci/e2e-coverage.sh [--open]   (from the repo root)
set -euo pipefail
cd "$(dirname "$0")/../.."

IGNORE='awaken-run-ingress/src/(memory|postgres)\.rs|awaken-ext-mcp/src/stdio\.rs|awaken-sandbox-local/src/namespace\.rs'

eval "$(cargo llvm-cov show-env --export-prefix)"
export RUSTFLAGS="$RUSTFLAGS -C llvm-args=-runtime-counter-relocation"
export LLVM_PROFILE_FILE="$CARGO_LLVM_COV_TARGET_DIR/awaken-%p%c.profraw"
cargo llvm-cov clean --workspace

pushd e2e >/dev/null
npm run test
npm run test:protocols
npm run test:management
npm run test:durable
npm run test:fs
if [ -n "${ANTHROPIC_API_KEY:-}${KIMI_API_KEY:-}" ]; then
  npm run test:real
else
  echo "SKIP test:real (no ANTHROPIC_API_KEY/KIMI_API_KEY)"
fi
popd >/dev/null

cargo llvm-cov report --ignore-filename-regex "$IGNORE" --summary-only
if [ "${1:-}" = "--open" ]; then
  cargo llvm-cov report --ignore-filename-regex "$IGNORE" --html --open
fi
