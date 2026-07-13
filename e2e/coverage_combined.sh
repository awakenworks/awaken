#!/usr/bin/env bash
# Whole-repo TEST coverage = the Rust unit/integration tests + the TS e2e, merged.
#
# Rationale: the TS HTTP e2e validates the serving/assembly surface end to end
# (protocols, host, config resolver, ingress, enforcement), while the runtime
# KERNEL's error/retry/defensive branches are validated by its own Rust unit +
# integration tests (awaken-runtime has ~120 of them). The kernel is NOT excluded
# from the surface — its coverage is simply provided by the tests designed for it,
# not by black-box HTTP e2e (which fundamentally cannot reach function-level error
# branches). So "全仓测试覆盖率" is the union of both test suites over the same
# instrumented build. Only the leaf crates that carry their OWN unit tests and are
# not part of this surface (ext-*, sandbox, the multi-backend stores) are excluded,
# exactly as coverage.sh does for the e2e-only figure.
#
# Live-model arms self-skip without ANTHROPIC_API_KEY/KIMI_API_KEY.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export CARGO_TARGET_DIR="${COV_DIR:-/tmp/awaken-combined-cov}"
rm -rf "$CARGO_TARGET_DIR"
eval "$(cargo llvm-cov show-env --sh)"

echo "[1/3] Rust unit + integration tests (instrumented)"
cargo test --workspace --quiet 2>&1 | grep -E "test result|error\[" | tail -3

echo "[2/3] instrumented bins + the TS e2e suite"
cargo build --quiet -p awaken-scenario-host --bin awaken-scenario-host
cargo build --quiet -p awaken-cli --bin awaken
cd e2e
for f in *_e2e.mjs; do env timeout 150 node "$f" >/dev/null 2>&1 && echo "ok" || echo "miss"; done \
  | sort | uniq -c
cd "$ROOT"

# Leaf crates covered by their own unit tests, not this surface (same list as
# coverage.sh's e2e-only figure). The runtime KERNEL is intentionally NOT here.
EXC='(awaken-ext-memory|awaken-ext-compact|awaken-ext-mcp|awaken-ext-goal|awaken-ext-skills|awaken-ext-state-machine|awaken-ext-builtin-tools|awaken-ext-permission|awaken-tool-pattern|awaken-mcp-wire|awaken-sandbox-local|awaken-provisioning-contract|awaken-agent-channel|awaken-store-fs|awaken-store-inmem|awaken-credential|awaken-store-schema|awaken-file-store)/'
echo "[3/3] combined coverage"
echo "  overall (all linked crates):"
cargo llvm-cov report --summary-only 2>/dev/null | tail -1
echo "  test-surface (protocols + host + config + runtime kernel + ingress; kernel INCLUDED):"
cargo llvm-cov report --summary-only --ignore-filename-regex "$EXC" 2>/dev/null | tail -1
