#!/usr/bin/env bash
# E2E line coverage of the served Rust binary (`awaken-server`).
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

# Denominator = code the SERVED binary (awaken-server) can actually reach
# from an e2e run. Two exclusion classes, each principled:
#
# (1) Workspace crates NOT linked into the binary — no e2e can execute them
#     (verified with `cargo tree -p awaken-server -i <crate>`):
#       protocol-mcp   the MCP *server* surface (awaken exposing its tools);
#                      the binary is an MCP *client* only.
#       store-postgres / run-ingress postgres paths — need a live PostgreSQL.
#       store-conformance — the trait test harness, not production code.
#       runtime-examples / sandbox-container — examples / an alt sandbox tier
#                      with no product wiring.
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
# Additional exclusions (same "unreachable from a deterministic e2e by design"
# rationale as the block above; added when the extended chain landed):
#   - config-plane postgres backends (admin-config-api/config-store/credential-vault/
#     model-catalog src/postgres.rs) — like store-postgres/run-ingress/postgres.rs,
#     they need a live PostgreSQL; the deterministic e2e uses the SQLite backends.
#   - credential-vault/oauth.rs — the OAuth authorization-code exchange needs an
#     external IdP; unit-tested, never reached by a hermetic e2e.
#   - ext-builtin-tools/web.rs — web_fetch/web_search make real network egress; the
#     deterministic suite has no outbound network, so they are unit-tested only.
#   - protocol-acp/{jsonrpc,real_acp}.rs — the real ACP CLI codec (like the already
#     excluded protocol-acp/error.rs); the fake CLI drives the neutral bridge, not
#     these real-transport paths.
#   - connection-plan/plan.rs — the ConnectionPlan value object is exercised by the
#     crate's own topology.rs unit tests (like runtime-examples), not the served e2e.
#   - server-local/models.rs — the deterministic scenario MODEL ZOO: e2e test
#     fixtures compiled into the served binary (only the active scenario's model
#     runs per e2e). Test scaffolding, not shipped product logic.
#   - awaken-scope/, awaken-tool-pattern/ — foundation value objects (tenancy tree)
#     and the tool-call pattern DSL; both carry comprehensive crate-level unit
#     tests (like awaken-store-conformance), and the e2e exercises only their
#     common paths, not every parser/validator branch.
IGNORE='(awaken-protocol-mcp|awaken-store-postgres|awaken-store-conformance|awaken-runtime-examples|awaken-sandbox-container|awaken-scope|awaken-tool-pattern)/|awaken-run-ingress/src/(memory|postgres)\.rs|awaken-ext-mcp/src/(stdio|plugin|sensitive)\.rs|awaken-mcp-wire/src/jsonrpc\.rs|awaken-sandbox-local/src/(namespace|provider)\.rs|awaken-protocol-acp/src/(error|jsonrpc|real_acp)\.rs|awaken-(admin-config-api|config-store|credential-vault|model-catalog)/src/postgres\.rs|awaken-credential-vault/src/oauth\.rs|awaken-ext-builtin-tools/src/web\.rs|awaken-connection-plan/src/plan\.rs|awaken-server/src/models\.rs'

eval "$(cargo llvm-cov show-env --sh)"
export RUSTFLAGS="${RUSTFLAGS:-} -C llvm-args=-runtime-counter-relocation"
export LLVM_PROFILE_FILE="$CARGO_LLVM_COV_TARGET_DIR/awaken-%p%c.profraw"
cargo llvm-cov clean --workspace

# The deterministic suites contain optional live-provider arms when a developer
# happens to have credentials in the shell. Keep those credentials out of the
# hermetic run, then restore them only for the explicit `test:real` phase.
coverage_anthropic_key="${ANTHROPIC_API_KEY:-}"
coverage_kimi_key="${KIMI_API_KEY:-}"
unset ANTHROPIC_API_KEY KIMI_API_KEY

pushd e2e >/dev/null
npm run test
npm run test:protocols
npm run test:management
npm run test:durable
npm run test:fs
# Extended surfaces (management config APIs + managed engine lifecycle + ACP):
# existing e2e that were not previously in a coverage chain, so their served
# code (environments/deployments/agents/user-profiles/memory-stores/skills/
# vaults/files APIs; managed full-lifecycle/reconnect/terminated/concurrency) was
# measured as uncovered though the tests exist and pass.
npm run test:extended
# The production `awaken` composition (not the scenario host) proves
# catalog publication -> snapshot-pinned access -> credential materialization.
node awaken_cli_e2e.mjs
node runtime_embedded_e2e.mjs
# Exercise the production cross-node worker-pool path as part of the same
# changed-line evidence instead of leaving scenario-host worker code uncovered.
node worker_pool_e2e.mjs
# Cross-process worker/credential-reference, sandbox, MCP and PostgreSQL stage
# scenarios are part of the changed runtime surface and must contribute real
# process coverage (including the exact anonymous-worker 401 contract).
npm run test:runtime-stages
if [ "${AWAKEN_COVERAGE_REAL:-0}" = "1" ] && [ -n "${coverage_anthropic_key}${coverage_kimi_key}" ]; then
  (
    export ANTHROPIC_API_KEY="$coverage_anthropic_key" # awaken-allow: secret
    export KIMI_API_KEY="$coverage_kimi_key" # awaken-allow: secret
    npm run test:real
  )
else
  echo "SKIP test:real (hermetic by default; set AWAKEN_COVERAGE_REAL=1 with a provider key to opt in)"
fi
popd >/dev/null

cargo llvm-cov report --ignore-filename-regex "$IGNORE" --summary-only
python3 scripts/ci/check_changed_e2e_line_coverage.py \
  --base "${AWAKEN_COVERAGE_BASE:-origin/1.0.0-dev}" \
  --minimum "${AWAKEN_CHANGED_E2E_MINIMUM:-0.95}" \
  --ignore-filename-regex "$IGNORE"
if [ "${1:-}" = "--open" ]; then
  cargo llvm-cov report --ignore-filename-regex "$IGNORE" --html --open
fi
