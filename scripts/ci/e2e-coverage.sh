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

# Denominator = code the SERVED binary (awaken-server-local) can actually reach
# from an e2e run. Two exclusion classes, each principled:
#
# (1) Workspace crates NOT linked into the binary — no e2e can execute them
#     (verified with `cargo tree -p awaken-server-local -i <crate>`):
#       protocol-mcp   the MCP *server* surface (awaken exposing its tools);
#                      the binary is an MCP *client* only.
#       store-postgres / run-ingress postgres paths — need a live PostgreSQL.
#       store-conformance — the trait test harness, not production code.
#       runtime-examples / sandbox-container — examples / an alt sandbox tier
#                      with no product wiring.
#       file-store     not wired into the served composition yet.
# (2) Alternate-backend / reference / real-provider modules inside LINKED crates,
#     unreachable from a deterministic e2e by design:
#       run-ingress/memory.rs   in-memory reference impl (the server uses SQLite).
#       ext-mcp/{stdio,plugin,sensitive}.rs + mcp-wire/jsonrpc.rs  the MCP
#                      transport machinery the served path does not execute: the
#                      host drives the HTTP client's request path via
#                      `connect_tools`, not the stdio transport, the McpPlugin
#                      composition API, the sensitive-field marking (a plugin
#                      concern), or the JSON-RPC peer (stdio + sampling). All are
#                      covered by ext-mcp's own unit tests.
#       sandbox-local/{namespace,provider}.rs  the ADR-0041 provisioning-contract
#                      tiers; the served host uses the pre-contract
#                      LocalSandboxProvider, so neither is reached (verified).
#       protocol-acp/error.rs   the provider-error taxonomy (auth/rate-limit/…)
#                      only fires on a REAL CLI's output; the fake CLI cannot
#                      inject provider text, so it is unit-tested, not e2e.
# Revisit an exclusion when its wiring changes.
IGNORE='(awaken-protocol-mcp|awaken-store-postgres|awaken-store-conformance|awaken-runtime-examples|awaken-sandbox-container|awaken-file-store)/|awaken-run-ingress/src/(memory|postgres)\.rs|awaken-ext-mcp/src/(stdio|plugin|sensitive)\.rs|awaken-mcp-wire/src/jsonrpc\.rs|awaken-sandbox-local/src/(namespace|provider)\.rs|awaken-protocol-acp/src/error\.rs'

eval "$(cargo llvm-cov show-env --export-prefix)"
export RUSTFLAGS="${RUSTFLAGS:-} -C llvm-args=-runtime-counter-relocation"
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
