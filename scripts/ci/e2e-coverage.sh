#!/usr/bin/env bash
# Changed production line coverage driven only through TypeScript/JavaScript API
# scenarios against served Rust processes.
#
# Instruments the workspace with cargo-llvm-cov (continuous mode, so profiles
# survive SIGINT/SIGKILL of spawned servers), drives every DETERMINISTIC e2e
# chain against the instrumented binary and reports line coverage. The
# real-model chain (`test:real`) is additionally run when ANTHROPIC_API_KEY /
# KIMI_API_KEY is set.
#
# Scope: coverage is measured over the code REACHABLE from the served binary.
# Files excluded below are unreachable BY DESIGN from a hermetic API E2E run — each
# with the reason; revisit an exclusion when its wiring changes. The MCP server,
# file store, local/namespace providers, and container providers are intentionally
# included: all are linked into and selected by the production composition.
#   - awaken-run-ingress/src/memory.rs    in-memory reference implementation
#     ("executable specification" per its module doc); the server always uses
#     the SQLite dispatch store.
#   - awaken-ext-mcp/src/stdio.rs         the server wires the HTTP MCP
#     transport only; stdio is covered by the crate's own Rust tests.
#
# Usage: scripts/ci/e2e-coverage.sh [--open]   (from the repo root)
set -euo pipefail
cd "$(dirname "$0")/../.."

if command -v python3 >/dev/null 2>&1 && python3 -c 'import sys' >/dev/null 2>&1; then
  coverage_python=python3
elif command -v python >/dev/null 2>&1 && python -c 'import sys' >/dev/null 2>&1; then
  coverage_python=python
else
  echo "ERROR: a working Python 3 interpreter is required" >&2
  exit 1
fi

# Denominator = code the SERVED binary (awaken-coordinator) can actually reach
# from an e2e run. Two exclusion classes, each principled:
#
# (1) Workspace crates NOT linked into the binary — no e2e can execute them
#     (verified with `cargo tree -p awaken-coordinator -i <crate>`):
#       store-conformance — the trait test harness, not production code.
#       runtime-examples — examples, not production composition.
#       awaken-eval — the offline benchmark/replay CLI; it is intentionally not
#                     linked into either served production composition.
#       awaken-scenario-host — the deterministic E2E fixture server. Its code is
#                     test orchestration, while the production crates it composes
#                     remain inside the denominator.
# (2) Alternate-backend / reference / real-provider modules inside LINKED crates,
#     unreachable from a deterministic e2e by design:
#       run-ingress/memory.rs   in-memory reference impl (the server uses SQLite).
#       config-resolver/reference_stores.rs — process-local reference adapters
#                     used by tests and local fixtures; production composition
#                     injects the durable config repositories.
#       ext-mcp/{stdio,plugin,sensitive}.rs + mcp-wire/jsonrpc.rs  the MCP
#                      transport machinery the served path does not execute: the
#                      host drives the HTTP client's request path via
#                      `connect_tools`, not the stdio transport, the McpPlugin
#                      composition API, the sensitive-field marking (a plugin
#                      concern), or the JSON-RPC peer (stdio + sampling). All are
#                      covered by ext-mcp's own unit tests.
#       protocol-acp/error.rs   the provider-error taxonomy (auth/rate-limit/…)
#                      only fires on a REAL CLI's output; the fake CLI cannot
#                      inject provider text, so it is unit-tested, not e2e.
# The stage gate provisions a disposable PostgreSQL and drives the Postgres
# dispatch, history, wake, and resource-plane paths. The extended gate drives
# ACP JSON-RPC, so neither surface is excluded from changed-line evidence.
# Revisit an exclusion when its wiring changes.
# Additional exclusions (same "unreachable from a deterministic e2e by design"
# rationale as the block above; added when the extended chain landed):
#   - protocol-acp/real_acp.rs — the real external ACP CLI transport; the API E2E
#     drives the same neutral bridge through the deterministic JSON-RPC fixture.
#   - server-local/models.rs — the deterministic scenario MODEL ZOO: e2e test
#     fixtures compiled into the served binary (only the active scenario's model
#     runs per e2e). Test scaffolding, not shipped product logic.
# Reachable production modules are deliberately not excluded merely because they
# need PostgreSQL, a local OAuth/HTTP fixture, or uncommon validation input.
# Individual zero-hit lines that cannot be selected through any served API are
# reviewed in e2e_unreachable.toml. The checker validates every range/reason,
# rejects stale entries, still counts hits inside those ranges, and caps the
# audited share at 15% so the manifest cannot become an unbounded escape hatch.
IGNORE='(awaken-store-conformance|awaken-runtime-examples|awaken-eval|awaken-scenario-host)/|awaken-run-ingress/src/memory\.rs|awaken-config-resolver/src/reference_stores\.rs|awaken-ext-mcp/src/(stdio|plugin|sensitive)\.rs|awaken-mcp-wire/src/jsonrpc\.rs|awaken-protocol-acp/src/(error|real_acp)\.rs|awaken-coordinator/src/models\.rs'

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/awaken-e2e-coverage}"
export CARGO_LLVM_COV_TARGET_DIR="${CARGO_LLVM_COV_TARGET_DIR:-$CARGO_TARGET_DIR}"
eval "$(cargo llvm-cov show-env --sh)"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    # MSVC cannot link instrumented proc-macro/build-script binaries with runtime
    # counter relocation (LNK1105/code 1224). Use ordinary per-process profiles;
    # the E2E harness closes server stdin so Windows performs a real graceful
    # shutdown and LLVM flushes counters instead of Node force-terminating it.
    export LLVM_PROFILE_FILE="$CARGO_LLVM_COV_TARGET_DIR/awaken-%p-%12m.profraw"
    ;;
  Darwin)
    # Relocated counters must be page-aligned. Mach-O links this workspace's
    # instrumented binaries at offsets that violate LLVM's 16 KiB requirement,
    # producing profiles that look runnable but cannot be trusted. Every harness
    # child is terminated gracefully, so ordinary per-process profiles are safe.
    export LLVM_PROFILE_FILE="$CARGO_LLVM_COV_TARGET_DIR/awaken-%p-%12m.profraw"
    ;;
  *)
    export RUSTFLAGS="${RUSTFLAGS:-} -C llvm-args=-runtime-counter-relocation"
    export LLVM_PROFILE_FILE="$CARGO_LLVM_COV_TARGET_DIR/awaken-%p-%12m%c.profraw"
    ;;
esac
if [ "${AWAKEN_COVERAGE_RESUME:-0}" = "1" ]; then
  echo "RESUME coverage profiles and instrumented build cache"
else
  cargo llvm-cov clean --workspace
fi

# The deterministic suites contain optional live-provider arms when a developer
# happens to have credentials in the shell. Keep those credentials out of the
# hermetic run, then restore them only for the explicit `test:real` phase.
coverage_anthropic_key="${ANTHROPIC_API_KEY:-}"
coverage_kimi_key="${KIMI_API_KEY:-}"
unset ANTHROPIC_API_KEY KIMI_API_KEY

# Node 22 does not execute `.ts` entry points directly. Load the repository's
# pinned TypeScript runner once for every npm/Node child in this coverage chain.
# Use a file URL so stage tests that intentionally set cwd to the repository root
# do not lose package resolution from e2e/node_modules (especially on Windows).
pushd e2e >/dev/null
tsx_loader_url="$(node -p "require('node:url').pathToFileURL(require.resolve('tsx')).href")"
export NODE_OPTIONS="${NODE_OPTIONS:+$NODE_OPTIONS }--import=$tsx_loader_url"

run_coverage_suites() {
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
  npm run test:environment-matrix
  # The production `awaken` composition (not the scenario host) proves
  # catalog publication -> snapshot-pinned access -> credential materialization.
  # `test:extended` uses 38411 immediately before this process. Give the CLI
  # composition a distinct port so a shutting-down scenario server cannot satisfy
  # its readiness probe and then disappear between requests.
  E2E_PORT=39411 node awaken_cli_e2e.mjs
  node runtime_embedded_e2e.mjs
  # Exercise the production cross-node worker-pool path as part of the same
  # changed-line evidence instead of leaving scenario-host worker code uncovered.
  node worker_pool_e2e.mjs
  npm run test:coordinator-authority
  # Cross-process worker/credential-reference, sandbox, MCP and PostgreSQL stage
  # scenarios are part of the changed runtime surface and must contribute real
  # process coverage (including the exact anonymous-worker 401 contract).
  npm run test:runtime-stages
  # Deterministic production-composition and lifecycle scenarios that are not part
  # of the historical aggregate suites. Keeping the list in package.json makes the
  # exact changed-line evidence runnable locally without invoking the reporter.
  npm run test:coverage-gaps
  if [ "${AWAKEN_COVERAGE_REAL:-0}" = "1" ] && [ -n "${coverage_anthropic_key}${coverage_kimi_key}" ]; then
    (
      export ANTHROPIC_API_KEY="$coverage_anthropic_key" # awaken-allow: secret
      export KIMI_API_KEY="$coverage_kimi_key" # awaken-allow: secret
      npm run test:real
    )
  else
    echo "SKIP test:real (hermetic by default; set AWAKEN_COVERAGE_REAL=1 with a provider key to opt in)"
  fi
}

run_coverage_suites
popd >/dev/null

lcov_directory="$CARGO_LLVM_COV_TARGET_DIR/awaken-homogeneous-lcov"
mapfile -t lcov_reports < <(
  "$coverage_python" scripts/ci/export_homogeneous_lcov.py \
    --profile-directory "$CARGO_LLVM_COV_TARGET_DIR" \
    --output-directory "$lcov_directory" \
    --ignore-filename-regex "$IGNORE"
)
lcov_arguments=()
for report in "${lcov_reports[@]}"; do
  lcov_arguments+=(--lcov-path "$report")
done
"$coverage_python" scripts/ci/check_changed_e2e_line_coverage.py \
  --base "${AWAKEN_COVERAGE_BASE:-origin/1.0.0-dev}" \
  --minimum "${AWAKEN_CHANGED_E2E_MINIMUM:-0.95}" \
  --ignore-filename-regex "$IGNORE" \
  "${lcov_arguments[@]}" \
  --unreachable-manifest scripts/ci/e2e_unreachable.toml \
  --maximum-unreachable-fraction 0.15
if [ "${1:-}" = "--open" ]; then
  echo "HTML export is unavailable for heterogeneous process profiles; use the LCOV reports in $lcov_directory"
fi
