#!/usr/bin/env bash
# Combined test coverage: the Rust workspace test suite AND the Node/TS e2e suite
# over one instrumented build, reported together. This is the project's true test
# coverage — the e2e-only figure (see scripts/ci/e2e-coverage.sh) understates it
# because the
# runtime-core error/edge paths are covered by Rust unit tests, not by the HTTP
# e2e. Two figures are printed: overall (every linked crate) and e2e-surface
# (server + protocols + config + runtime-core + ingress), the same principled
# exclusions as scripts/ci/e2e-coverage.sh.
#
# Usage: scripts/ci/combined-coverage.sh   (from the repo root)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# This gate takes no arguments. Accepting one silently is how a caller's
# intent ("only the links", "--quick") disappears into a full run that
# answers a different question; refuse before any work.
if [ "$#" -ne 0 ]; then
  echo "usage: scripts/ci/combined-coverage.sh" >&2
  exit 2
fi
source scripts/ci/_provider_environment.sh
source scripts/ci/_deadline.sh
awaken_unset_ambient_api_keys

# A dedicated, fresh target dir so the instrumented build is not shadowed by the
# shared cargo target-dir's non-instrumented binary.
export CARGO_TARGET_DIR="${COV_DIR:-$ROOT/target/coverage-instrumented}"
export CARGO_LLVM_COV_TARGET_DIR="$CARGO_TARGET_DIR"
export CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR"
eval "$(cargo llvm-cov show-env --sh)"
# Retain the isolated instrumented build cache; only counters must start fresh.
cargo llvm-cov clean --workspace --profraw-only --locked
export LLVM_PROFILE_FILE="$CARGO_LLVM_COV_TARGET_DIR/awaken-%p-%12m.profraw"
workspace_features="$(python3 scripts/ci/rust_test_plan.py --features)"
source_digest="$(python3 scripts/ci/rust_test_plan.py --source-digest)"
source_revision="$(git rev-parse HEAD)"
evidence=$(mktemp -d "$CARGO_TARGET_DIR/combined-evidence.XXXXXX")
cargo metadata --no-deps --locked --format-version 1 --features "$workspace_features" > "$evidence/metadata.json"

echo "[1/3] Rust workspace tests (instrumented)"
cargo nextest run --workspace --locked --features "$workspace_features"
run_with_deadline "${AWAKEN_COVERAGE_DOCTEST_TIMEOUT_SECONDS:-600}" cargo test --workspace --doc --locked --features "$workspace_features"

echo "[2/3] build the server binary and drive the TS e2e suite"
cargo build --locked --quiet --features "$workspace_features" -p awaken-scenario-host --bin awaken-scenario-host
python3 scripts/ci/check_node_dependencies.py
tsx_loader_url="$(cd e2e && node -p "require('node:url').pathToFileURL(require.resolve('tsx')).href")"
export NODE_OPTIONS="${NODE_OPTIONS:+$NODE_OPTIONS }--import=$tsx_loader_url"
npm --prefix e2e run test:deterministic

echo "[3/3] combined coverage report"
IGNORE="$(python3 scripts/ci/check_changed_e2e_line_coverage.py --print-ignore-filename-regex)"
python3 scripts/ci/export_homogeneous_lcov.py --profile-directory "$CARGO_LLVM_COV_TARGET_DIR" --output-directory "$evidence/lcov"
lcov_arguments=()
for report in "$evidence"/lcov/*.lcov; do lcov_arguments+=(--lcov-path "$report"); done
test "$(python3 scripts/ci/rust_test_plan.py --source-digest)" = "$source_digest" || { echo "source changed during combined coverage" >&2; exit 1; }
python3 scripts/ci/coverage_summary.py --metadata "$evidence/metadata.json" "${lcov_arguments[@]}" \
  --revision "$source_revision" --source-digest "$source_digest" --output "$evidence/per-crate.json"
python3 scripts/ci/check_changed_e2e_line_coverage.py --base "${AWAKEN_COVERAGE_BASE:-origin/main}" \
  --minimum "${AWAKEN_CHANGED_E2E_MINIMUM:-0.95}" --ignore-filename-regex "$IGNORE" "${lcov_arguments[@]}" \
  --unreachable-manifest scripts/ci/e2e_unreachable.toml --maximum-unreachable-fraction 0.15
