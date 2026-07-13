#!/usr/bin/env bash
# TS e2e coverage: instruments the Rust server (awaken-server-local), drives it
# through the full Node e2e suite (deterministic + durable + fs + live-model
# variants), and reports Rust line/region coverage attributable to the e2e.
#
# Live-model arms run only when ANTHROPIC_API_KEY (or KIMI_API_KEY) is exported;
# e.g. the Kimi coding endpoint:
#   export ANTHROPIC_API_KEY=sk-...           # Kimi coding key
#   export ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/
#   export ANTHROPIC_MODEL=kimi-k2-0711-preview
#
# Two figures are reported:
#   * overall      — every crate linked into the server binary.
#   * e2e-surface  — the surface the HTTP e2e is designed to exercise (server,
#                    protocols, config, runtime-core, ingress). Excludes runtime
#                    EXTENSIONS (memory/compact/mcp/tool-pattern/…), the ACP/sandbox
#                    execution substrate, and the multi-backend content-addressed
#                    store (awaken-file-store: the e2e only drives its in-memory
#                    backend; Fs/Pg/S3 have their own Rust unit tests) — all covered
#                    by their own Rust unit/integration tests, not this HTTP e2e.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# A dedicated, fresh target dir so the instrumented build is not shadowed by a
# cached non-instrumented binary from the shared cargo target-dir.
export CARGO_TARGET_DIR="${COV_DIR:-/tmp/acp-cov}"
rm -rf "$CARGO_TARGET_DIR"
eval "$(cargo llvm-cov show-env --sh)"

echo "[build] instrumented awaken-server-local into $CARGO_TARGET_DIR"
cargo build --quiet -p awaken-server-local --bin awaken-server-local
# The aggregated `awaken` binary the awaken_*_e2e drive (each spawns its own
# process); build it instrumented into the same target dir so its coverage is
# attributed to the e2e too.
cargo build --quiet -p awaken-cli --bin awaken

cd e2e
run() { # file, label, extra-env...
  local f="$1"; shift; local label="$1"; shift
  if env "$@" timeout 150 node "$f" 2>&1 | grep -q "E2E PASS"; then
    echo "PASS $f [$label]"
  else
    echo "MISS $f [$label]"
  fi
}

# Deterministic + live-model suite (live arms self-skip without a key).
for f in *_e2e.mjs; do run "$f" default; done
# Durable ingress paths (run-ingress durable branch).
DUR="$(mktemp -d)"
for f in managed_e2e managed_durable_e2e managed_durable_ops_e2e managed_supersede_e2e \
         durable_worker_hitl_deny_e2e durable_worker_cancel_e2e durable_worker_supersede_e2e \
         managed_scheduled_e2e managed_daemon_e2e managed_crossthread_e2e \
         managed_livecontrol_e2e managed_restart_e2e durable_trace_propagation_e2e \
         durable_soak_fairness_e2e durable_worker_metrics_e2e dispatch_metrics_export_e2e; do
  run "$f.mjs" durable AWAKEN_STORAGE_DIR="$DUR"
done
# Filesystem store backend.
FS="$(mktemp -d)"
for f in managed_statemachine_e2e ai_sdk_e2e ag_ui_e2e a2a_e2e managed_restart_e2e; do
  run "$f.mjs" fs AWAKEN_STORE=fs AWAKEN_STORAGE_DIR="$FS"
done

cd "$ROOT"
echo
echo "[coverage] overall (all linked crates):"
cargo llvm-cov report --summary-only 2>/dev/null | tail -1
echo "[coverage] e2e-surface (server + protocols + config + runtime-core + ingress):"
EXC='(awaken-ext-memory|awaken-ext-compact|awaken-ext-mcp|awaken-ext-goal|awaken-ext-skills|awaken-ext-state-machine|awaken-ext-builtin-tools|awaken-ext-permission|awaken-tool-pattern|awaken-mcp-wire|awaken-sandbox-local|awaken-provisioning-contract|awaken-agent-channel|awaken-store-fs|awaken-store-inmem|awaken-credential|awaken-store-schema|awaken-file-store)/'
cargo llvm-cov report --summary-only --ignore-filename-regex "$EXC" 2>/dev/null | tail -1
# The OPEN single-machine serving surface — the wiring the aggregated `awaken`
# e2e (awaken_durability_e2e / awaken_cli_e2e / awaken_per_component_db_e2e)
# validate end to end: the protocol adapters, the neutral host, the config
# resolver, and the enforcement guard. The runtime KERNEL (engine/retry/breaker),
# the ext-* extensions, the stores and the sandbox are unit-tested for their logic
# (not this HTTP e2e's job), and the BuSL management plane (admin authoring /
# iam-server) is out of the open single-machine form entirely — all excluded here.
echo "[coverage] open single-machine serving surface (protocols + host + config-resolver + authz-enforce):"
SURF='(awaken-authz-enforce|awaken-protocol-managed|awaken-protocol-ai-sdk|awaken-protocol-ag-ui|awaken-protocol-a2a|awaken-protocol-transport|awaken-runtime-host|awaken-config-resolver)/src'
cargo llvm-cov report 2>/dev/null | grep -E "$SURF" | grep -v runtime-examples \
  | awk '{lines+=$8; missed+=$9} END {printf "  lines: %d  missed: %d  cover: %.2f%%\n", lines, missed, (lines-missed)/lines*100}'
