#!/usr/bin/env bash
# Cross-layer reliability qualification. Existing domain/conformance tests own
# behavior; this gate composes them once and adds tool-assisted evidence.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys

# One native Session observation composition, reused by PR, release and formal
# qualification. The formal caller supplies a fresh output directory.
if [ "${1:-}" = "--session-observations" ]; then
  evidence_args=(--kind session-observations)
  if [ -n "${AWAKEN_SESSION_EVIDENCE_DIR:-}" ]; then
    evidence_args+=(--directory "$AWAKEN_SESSION_EVIDENCE_DIR")
  fi
  session_evidence_dir="$(python3 scripts/ci/formal_run_evidence.py artifacts-begin "${evidence_args[@]}")"
  export AWAKEN_OBSERVATION_TRACE_DIR="$session_evidence_dir/raw"
  finish_session_evidence() {
    result=$?
    trap - EXIT
    python3 scripts/ci/formal_run_evidence.py artifacts-finish --directory "$session_evidence_dir" --exit-code "$result" || result=1
    exit "$result"
  }
  trap finish_session_evidence EXIT
  echo "Session evidence: $session_evidence_dir"
  {
  cargo test --locked --features awaken-store-sqlite/test-support \
    -p awaken-session-contract -p awaken-session-application \
    -p awaken-store-inmem -p awaken-store-fs -p awaken-store-sqlite -p awaken-protocol-managed
  cargo test --locked -p awaken-coordinator --lib runtime_authority::tests
  pnpm -C e2e exec tsx managed_mcp_recovery_e2e.ts
  python3 scripts/ci/render_runtime_refinement_traces.py --projection session-observations \
    "$AWAKEN_OBSERVATION_TRACE_DIR" "$session_evidence_dir/rendered" --require-complete-transition-coverage
  } 2>&1 | tee "$session_evidence_dir/suite.log"
  exit 0
fi

mode="${1:---quick}"
case "$mode" in
  --quick|--full|--require-tools) ;;
  *) echo "usage: scripts/ci/check-reliability.sh [--quick|--full|--require-tools|--session-observations]" >&2; exit 2 ;;
esac

# cargo-mutants restores the source tree, but an interrupted or timestamp-close
# restoration can leave an incremental artifact compiled from the last mutant.
# Every mode starts from source-owned evidence and the mutation mode repeats the
# cleanup on both success and failure.
cleanup_mutation_artifacts() {
  cargo clean -p awaken-mcp-wire >/dev/null 2>&1 || true
  cargo clean -p awaken-runtime-contract >/dev/null 2>&1 || true
  cargo clean -p awaken-session-contract >/dev/null 2>&1 || true
}
cleanup_mutation_artifacts
python3 scripts/ci/check_reliability.py
cargo test -p awaken-reliability-testkit --lib
cargo test -p awaken-mcp-wire --lib sse::tests
cargo test -p awaken-dispatch --features test-support --test dispatch_conformance \
  memory_and_sqlite_refine_the_same_crash_recovery_history

if [ "$mode" = "--quick" ]; then
  echo "reliability quick gate: passed"
  exit 0
fi

cargo test -p awaken-store-sqlite --features test-support --test failure_atomicity
cargo test -p awaken-session-store --test process_crash_outbox
cargo test -p awaken-session-application --lib tests::creation::time_assurance
cargo test -p awaken-credential-store --features sqlite,sealed-aead,test-support \
  --test process_crash_creation --test process_crash_managed_creation
cargo test -p awaken-config-store --test process_crash_audit

if [ "$mode" = "--full" ]; then
  echo "reliability full executable gate: passed (tool-assisted gates not requested)"
  exit 0
fi

for tool in cargo-fuzz cargo-mutants; do
  if ! cargo install --list | grep -q "^${tool} "; then
    echo "ERROR: $tool is required by the release reliability gate" >&2
    exit 1
  fi
done
if ! rustup toolchain list | grep -q '^nightly'; then
  echo "ERROR: nightly Rust is required by the release reliability gate" >&2
  exit 1
fi
if ! cargo +nightly miri --version >/dev/null 2>&1; then
  echo "ERROR: the miri component is required by the release reliability gate" >&2
  exit 1
fi

fuzz_seconds="${AWAKEN_FUZZ_SECONDS:-15}"
(cd fuzz && cargo +nightly fuzz run sse_chunks -- -max_total_time="$fuzz_seconds")
(cd fuzz && cargo +nightly fuzz run thread_commit_wire -- -max_total_time="$fuzz_seconds")
(cd fuzz && cargo +nightly fuzz run tool_batch_transitions -- -max_total_time="$fuzz_seconds")
cargo +nightly miri test -p awaken-mcp-wire --lib
sanitizer_target="$(rustc +nightly -vV | sed -n 's/^host: //p')"
RUSTFLAGS="-Zsanitizer=address" cargo +nightly test -p awaken-mcp-wire --lib \
  --target "$sanitizer_target" -Zbuild-std
# T1 the exact existing production-cache workload runs under TSan; T2 zero
# selected/ignored/failed tests or tool failure reject. CrashProcess is a helper
# library with no tests, not a concurrent production workload.
tsan_log="target/test-timings/worker-heartbeat-tsan.log"
mkdir -p "$(dirname "$tsan_log")"
RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -p awaken-worker --lib \
  --target "$sanitizer_target" -Zbuild-std \
  credential_liveness::tests::heartbeat_snapshot_never_mixes_concurrent_publication_batches \
  -- --exact 2>&1 | tee "$tsan_log"
python3 scripts/ci/check_reliability.py --tsan-log "$tsan_log"
trap cleanup_mutation_artifacts EXIT
mutation_evidence="${AWAKEN_RELIABILITY_EVIDENCE_DIR:-$CARGO_TARGET_DIR/reliability}"
mkdir -p "$mutation_evidence"
cargo mutants --package awaken-mcp-wire \
  --output "$mutation_evidence/sse" \
  --file crates/runtime/awaken-mcp-wire/src/sse.rs --timeout 30 --no-times -- sse::tests
python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/sse/mutants.out"
cargo mutants --package awaken-runtime-contract \
  --output "$mutation_evidence/tool-batch" \
  --file crates/contract/awaken-runtime-contract/src/tool_batch.rs \
  --re 'permits_call_transition|next_execution_attempt' \
  --timeout 30 --no-times -- tool_batch::tests
python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/tool-batch/mutants.out"
# Shared production metadata/authorization reducers: every selected mutation
# must be caught by the domain decision tables; baseline/build failures do not
# qualify as kills. cargo-mutants owns isolation and the outcome artifact.
cargo mutants --package awaken-session-contract \
  --output "$mutation_evidence/session-observations" \
  --file crates/contract/awaken-session-contract/src/event_batches.rs \
  --file crates/contract/awaken-session-contract/src/session_realization.rs \
  --file crates/contract/awaken-session-contract/src/work_queue.rs \
  --re 'mark_processed_observation|realization_generation_authorizes|bounded_rfc3339_nanos' \
  --timeout 60 --no-times -- --lib
python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/session-observations/mutants.out"
cleanup_mutation_artifacts
trap - EXIT

echo "reliability required-tools gate: passed"
