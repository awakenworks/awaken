#!/usr/bin/env bash
# Replay every fixture in crates/awaken-eval/fixtures, compare the result
# against a committed baseline, and exit non-zero on regression.
#
# Usage:
#   ./scripts/replay-eval.sh                # default: replay + check
#   ./scripts/replay-eval.sh --record       # overwrite baseline (after review)
#
# Files:
#   crates/awaken-eval/fixtures/        — fixture inputs (committed)
#   crates/awaken-eval/baseline.ndjson  — reference NDJSON (committed)
#   target/awaken-eval/report.ndjson    — fresh run output (transient)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURES_DIR="${REPO_ROOT}/crates/awaken-eval/fixtures"
BASELINE="${REPO_ROOT}/crates/awaken-eval/baseline.ndjson"
REPORT_DIR="${REPO_ROOT}/target/awaken-eval"
REPORT="${REPORT_DIR}/report.ndjson"

mkdir -p "${REPORT_DIR}"

cargo build -p awaken-eval --bin awaken-eval --quiet

BIN="${REPO_ROOT}/target/debug/awaken-eval"

case "${1:-}" in
  --record)
    echo "[replay-eval] recording fresh baseline → ${BASELINE}"
    "${BIN}" replay --fixtures "${FIXTURES_DIR}" --report "${BASELINE}"
    exit 0
    ;;
  ""|--check)
    "${BIN}" replay --fixtures "${FIXTURES_DIR}" --report "${REPORT}"
    if [[ -f "${BASELINE}" ]]; then
      "${BIN}" check --baseline "${BASELINE}" --new "${REPORT}"
    else
      echo "[replay-eval] no baseline at ${BASELINE} — run with --record to seed one"
      exit 0
    fi
    ;;
  *)
    echo "usage: $0 [--check|--record]" >&2
    exit 2
    ;;
esac
