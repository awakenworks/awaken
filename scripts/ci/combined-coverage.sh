#!/usr/bin/env bash
# Combined test coverage: the Rust workspace test suite AND the Node/TS e2e suite
# over one instrumented build, reported together. This is the project's true test
# coverage — the e2e-only figure (see e2e/coverage.sh) understates it because the
# runtime-core error/edge paths are covered by Rust unit tests, not by the HTTP
# e2e. Two figures are printed: overall (every linked crate) and e2e-surface
# (server + protocols + config + runtime-core + ingress), the same principled
# exclusions as scripts/ci/e2e-coverage.sh.
#
# Live-model e2e arms run only when ANTHROPIC_API_KEY (or KIMI_API_KEY) is set,
# e.g. the Kimi coding endpoint:
#   export ANTHROPIC_API_KEY=sk-...           ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/
#   export ANTHROPIC_MODEL=kimi-for-coding
#
# Usage: scripts/ci/combined-coverage.sh   (from the repo root)
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# A dedicated, fresh target dir so the instrumented build is not shadowed by the
# shared cargo target-dir's non-instrumented binary.
export CARGO_TARGET_DIR="${COV_DIR:-/tmp/awaken-combined-cov}"
rm -rf "$CARGO_TARGET_DIR"
eval "$(cargo llvm-cov show-env --sh)"

echo "[1/3] Rust workspace tests (instrumented)"
cargo test --workspace --quiet 2>/dev/null || true

echo "[2/3] build the server binary and drive the TS e2e suite"
cargo build --quiet -p awaken-scenario-host --bin awaken-scenario-host
cd e2e
run() { timeout 150 node "$1" >/dev/null 2>&1 && echo "  pass $1" || echo "  miss $1"; }
for f in *_e2e.mjs; do run "$f"; done
# Durable-ingress paths (SQLite dispatch + daemon + cross-restart).
DUR="$(mktemp -d)"
for f in managed_e2e managed_durable_e2e managed_durable_ops_e2e managed_daemon_e2e \
         managed_crossthread_e2e managed_restart_e2e managed_session_config_restart_e2e; do
  timeout 150 env AWAKEN_STORAGE_DIR="$DUR" node "$f.mjs" >/dev/null 2>&1 || true
done
cd "$ROOT"

echo "[3/3] combined coverage report"
IGNORE='(awaken-protocol-mcp|awaken-store-postgres|awaken-store-conformance|awaken-runtime-examples|awaken-sandbox-container|awaken-file-store)/|awaken-run-ingress/src/(memory|postgres)\.rs|awaken-ext-mcp/src/(stdio|plugin|sensitive)\.rs|awaken-mcp-wire/src/jsonrpc\.rs|awaken-sandbox-local/src/(namespace|provider)\.rs|awaken-protocol-acp/src/error\.rs'
echo "== overall (all linked crates) =="
cargo llvm-cov report --summary-only 2>/dev/null | grep -E "^Filename|^TOTAL"
echo "== e2e-surface (server + protocols + config + runtime-core + ingress) =="
cargo llvm-cov report --ignore-filename-regex "$IGNORE" --summary-only 2>/dev/null | grep -E "^Filename|^TOTAL"
