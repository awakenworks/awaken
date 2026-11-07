#!/usr/bin/env bash
# Cross-layer reliability qualification. Existing domain/conformance tests own
# behavior; this gate composes them once and adds tool-assisted evidence.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys
awaken_append_repo_tools

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
  # The suite ends in an e2e TypeScript drill. Name an uninstalled e2e root
  # before fourteen minutes of Rust tests, not as `Command "tsx" not found`.
  python3 scripts/ci/check_node_dependencies.py
  {
  # SessionEventBatch owns processed-first/replay/terminal timestamps. Run its
  # package directly: compiling it only as a dependency cannot execute the
  # observation test required by the same fresh evidence manifest.
  cargo test --locked --features awaken-store-sqlite/test-support \
    -p awaken-session-contract -p awaken-session-configuration-contract \
    -p awaken-session-application-contract -p awaken-session-execution-contract \
     \
    -p awaken-session-configuration-contract -p awaken-worker-contract \
    -p awaken-session-application \
    -p awaken-store-inmem -p awaken-store-fs -p awaken-store-sqlite -p awaken-protocol-managed
  cargo test --locked -p awaken-session-executor --lib projection_convergence::
  cargo test --locked -p awaken-session-realization-driver --lib scheduler_capacity_observes_real_poll_drop_and_two_stage_cancellation
  cargo test --locked -p awaken-session-executor --lib control_deadline_releases_capacity_before_expired_session_mutex_cleanup
  cargo test --locked -p awaken-coordinator --lib runtime_authority::tests
  cargo test --locked -p awaken-dispatch --features test-support --test pool_memory only_the_queue_owner_runs_maintenance_without_disabling_worker_drains
  pnpm -C e2e exec tsx managed_mcp_recovery_e2e.ts
  python3 scripts/ci/render_runtime_refinement_traces.py --projection session-observations \
    "$AWAKEN_OBSERVATION_TRACE_DIR" "$session_evidence_dir/rendered" --require-complete-transition-coverage
  } 2>&1 | tee "$session_evidence_dir/suite.log"
  exit 0
fi

# Every suite is a row of scripts/ci/service_suites.toml, run here by id.
# --affected <base> runs only the rows the change since <base> can reach
# (scripts/ci/service_test_plan.py) and sets up no tool when none is.
mode=--quick
affected_base=""
reliability_usage="usage: scripts/ci/check-reliability.sh [--quick|--full|--require-tools|--session-observations] [--affected <base>]"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --quick|--full|--require-tools) mode="$1" ;;
    --affected)
      if [ "$#" -lt 2 ] || [ -z "$2" ] || [ -n "$affected_base" ]; then echo "$reliability_usage" >&2; exit 2; fi
      affected_base="$2"
      shift ;;
    *) echo "$reliability_usage" >&2; exit 2 ;;
  esac
  shift
done
source scripts/ci/_affected_suites.sh
suites_load reliability
if [ -n "$affected_base" ]; then
  affected_suites_load reliability "$affected_base"
  if ! suites_selected quick full tools mutation; then
    echo "reliability: no suite is reached by the change since $affected_base"
    exit 0
  fi
fi

# cargo-mutants restores the source tree, but an interrupted or timestamp-close
# restoration can leave an incremental artifact compiled from the last mutant.
# Mutation builds use a separate target/build directory. Cleaning those owners
# before/after mutation remains required; ordinary quick/full runs retain their
# incremental cache and can never consume an interrupted mutant compilation.
cleanup_mutation_artifacts() {
  cargo clean -p awaken-mcp-wire >/dev/null 2>&1 || true
  cargo clean -p awaken-runtime-contract >/dev/null 2>&1 || true
  cargo clean -p awaken-tool-contract -p awaken-agent-contract -p awaken-lease-contract -p awaken-time \
    -p awaken-session-types >/dev/null 2>&1 || true
  cargo clean -p awaken-session-application-contract -p awaken-session-configuration-contract \
    -p awaken-session-contract \
     -p awaken-session-execution-contract \
     >/dev/null 2>&1 || true
  cargo clean -p awaken-session-executor >/dev/null 2>&1 || true
}
python3 scripts/ci/check_reliability.py
source scripts/ci/_rust_test_execution.sh
for id in $(suite_ids quick); do suite_run "$id"; done

if [ "$mode" = "--quick" ]; then
  echo "reliability quick gate: passed"
  exit 0
fi

for id in $(suite_ids full); do suite_run "$id"; done

if [ "$mode" = "--full" ]; then
  echo "reliability full executable gate: passed (tool-assisted gates not requested)"
  exit 0
fi

if ! suites_selected tools mutation; then
  echo "reliability: no tool-assisted suite is reached by the change since $affected_base"
  exit 0
fi
# A query that could not run is not an absent tool. Piping the listing
# straight into grep collapsed both into "$tool is required": under load a
# failed fork in the pipeline reported a missing cargo-fuzz on a machine that
# had it installed, so the gate's verdict depended on system load. Capture the
# listing, judge the query first, then judge its content.
if ! installed_tools="$(cargo install --list 2>&1)"; then
  echo "ERROR: could not list installed cargo tools; the reliability gate cannot decide" >&2
  printf '%s\n' "$installed_tools" | sed 's/^/  /' >&2
  exit 1
fi
for tool in cargo-fuzz cargo-mutants; do
  if ! printf '%s\n' "$installed_tools" | grep -q "^${tool} "; then
    echo "ERROR: $tool is required by the release reliability gate" >&2
    exit 1
  fi
done
if ! installed_toolchains="$(rustup toolchain list 2>&1)"; then
  echo "ERROR: could not list Rust toolchains; the reliability gate cannot decide" >&2
  printf '%s\n' "$installed_toolchains" | sed 's/^/  /' >&2
  exit 1
fi
if ! printf '%s\n' "$installed_toolchains" | grep -q '^nightly'; then
  echo "ERROR: nightly Rust is required by the release reliability gate" >&2
  exit 1
fi
if ! cargo +nightly miri --version >/dev/null 2>&1; then
  echo "ERROR: the miri component is required by the release reliability gate" >&2
  exit 1
fi

fuzz_seconds="${AWAKEN_FUZZ_SECONDS:-15}"
# cargo-fuzz has no --locked switch. Resolve its separate workspace through
# Cargo's existing locked reader before any fuzz build can rewrite source.
(cd fuzz && cargo +nightly metadata --locked --format-version 1 > /dev/null)
(cd fuzz && suite_step reliability.fuzz.sse-chunks cargo +nightly fuzz run sse_chunks -- -max_total_time="$fuzz_seconds")
(cd fuzz && suite_step reliability.fuzz.thread-commit-wire cargo +nightly fuzz run thread_commit_wire -- -max_total_time="$fuzz_seconds")
(cd fuzz && suite_step reliability.fuzz.tool-batch-transitions cargo +nightly fuzz run tool_batch_transitions -- -max_total_time="$fuzz_seconds")
suite_step reliability.miri.mcp-wire cargo +nightly miri test -p awaken-mcp-wire --lib
sanitizer_target="$(rustc +nightly -vV | sed -n 's/^host: //p')"
RUSTFLAGS="-Zsanitizer=address" suite_step reliability.asan.mcp-wire cargo +nightly test -p awaken-mcp-wire --lib \
  --target "$sanitizer_target" -Zbuild-std
# T1 the exact existing production-cache workload runs under TSan; T2 zero
# selected/ignored/failed tests or tool failure reject. CrashProcess is a helper
# library with no tests, not a concurrent production workload.
tsan_log="target/test-timings/worker-heartbeat-tsan.log"
mkdir -p "$(dirname "$tsan_log")"
RUSTFLAGS="-Zsanitizer=thread" suite_step reliability.tsan.worker cargo +nightly test -p awaken-worker --lib \
  --target "$sanitizer_target" -Zbuild-std \
  credential_liveness::tests::heartbeat_snapshot_never_mixes_concurrent_publication_batches \
  -- --exact 2>&1 | tee "$tsan_log"
suite_step reliability.tsan.worker python3 scripts/ci/check_reliability.py --tsan-log "$tsan_log"
mutation_evidence="${AWAKEN_RELIABILITY_EVIDENCE_DIR:-$CARGO_TARGET_DIR/reliability}"
(
# Keep BOTH Cargo directories isolated: a runner may configure build.build-dir
# outside target-dir. The subshell also restores the normal graph for callers.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR%/}/reliability-mutation-build"
export CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR"
suites_selected mutation || exit 0
cleanup_mutation_artifacts
trap cleanup_mutation_artifacts EXIT
mkdir -p "$mutation_evidence"
suite_step reliability.mutants.sse cargo mutants --package awaken-mcp-wire \
  --output "$mutation_evidence/sse" \
  --file crates/sdk/providers/awaken-mcp-wire/src/sse.rs --timeout 30 --no-times -- sse::tests
suite_step reliability.mutants.sse python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/sse/mutants.out"
suite_step reliability.mutants.tool-batch cargo mutants --package awaken-tool-contract \
  --output "$mutation_evidence/tool-batch" \
  --file crates/sdk/contracts/awaken-tool-contract/src/tool_batch.rs \
  --re 'permits_call_transition|next_execution_attempt' \
  --timeout 30 --no-times -- tool_batch::tests
suite_step reliability.mutants.tool-batch python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/tool-batch/mutants.out"
# Shared production metadata/authorization reducers: every selected mutation
# must be caught by the domain decision tables; baseline/build failures do not
# qualify as kills. cargo-mutants owns isolation and the outcome artifact.
suite_step reliability.mutants.session-event-observations cargo mutants --package awaken-session-contract \
  --output "$mutation_evidence/session-event-observations" \
  --file crates/platform/session/awaken-session-contract/src/event/mod.rs \
  --re 'mark_processed_observation' \
  --timeout 60 --no-times -- --lib
suite_step reliability.mutants.session-event-observations python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/session-event-observations/mutants.out"
suite_step reliability.mutants.session-realization-observations cargo mutants --package awaken-session-execution-contract \
  --output "$mutation_evidence/session-realization-observations" \
  --file crates/platform/session/awaken-session-execution-contract/src/session_realization/generation_authority.rs \
  --re 'realization_generation_authorizes' \
  --timeout 60 --no-times -- --lib
suite_step reliability.mutants.session-realization-observations python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/session-realization-observations/mutants.out"
suite_step reliability.mutants.session-work-observations cargo mutants --package awaken-time \
  --test-package awaken-time --test-package awaken-worker-contract \
  --output "$mutation_evidence/session-work-observations" \
  --file crates/sdk/support/awaken-time/src/utc.rs \
  --re 'bounded_rfc3339_nanos' \
  --timeout 60 --no-times -- --lib
suite_step reliability.mutants.session-work-observations python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/session-work-observations/mutants.out"
# R1/R3 causes: a newer or stale root revision reaches the canonical shared
# ordering kernel; effects: the complete executor installation advances or
# rejects without rewriting its correlated receipt. The Host convergence test
# owns the observation, while the mutation belongs to the shared type owner.
suite_step reliability.mutants.environment-projection cargo mutants --package awaken-session-types --test-package awaken-session-executor \
  --output "$mutation_evidence/environment-projection" \
  --file crates/platform/session/awaken-session-types/src/monotone.rs \
  --re 'replace > with <|replace > with >=' \
  --timeout 60 --no-times -- --lib projection_convergence
suite_step reliability.mutants.environment-projection python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/environment-projection/mutants.out"
cleanup_mutation_artifacts
trap - EXIT
)

echo "reliability required-tools gate: passed"
