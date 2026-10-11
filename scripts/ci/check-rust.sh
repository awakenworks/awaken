#!/usr/bin/env bash
# Run Rust workspace checks when a Rust workspace exists.
#
# Output contract: one "✓ <step>" line per passing step; on failure, only the
# de-noised error block plus a "→ fix:" line with the exact command that
# resolves it. Cargo's Checking/Compiling progress is stripped so a failure is
# never buried under build spam.
set -uo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys
awaken_append_repo_tools

# Argument admission happens here, before a single check runs. A misspelled
# mode used to reach the `case` at the bottom of this file, so `--ful` first
# executed every static scan and both neutral-graph compiles and only then
# refused; a caller reading the tail of that output saw work, not a rejection.
# Extra arguments were discarded outright, so `--full --require-tools` ran
# `--full` and silently dropped the second word.
mode="${1:---full}"
case "$mode" in
  --quick|--push|--full|--self-test) ;;
  *)
    echo "usage: scripts/ci/check-rust.sh [--quick|--push|--full|--self-test]" >&2
    exit 2
    ;;
esac
if [ "$#" -gt 1 ]; then
  echo "usage: scripts/ci/check-rust.sh [--quick|--push|--full|--self-test]" >&2
  echo "       (this gate takes exactly one mode; '${*:2}' is not part of it)" >&2
  exit 2
fi

if [ ! -f Cargo.toml ]; then
  echo "OK - no Cargo.toml; skipping Rust checks."
  exit 0
fi

locked=()
if [ -f Cargo.lock ]; then
  locked=(--locked)
fi

# Cargo progress lines carry no diagnostic value; drop them so the error block
# is the only thing left on screen when a step fails.
NOISE='^[[:space:]]*(Checking|Compiling|Blocking|Downloading|Downloaded|Updating|Locking|Adding|Removing|Finished|Building|Fresh|Installing) '

fail=0
# step "<label>" "<fix hint>" <command...>
#   success → one "✓ <label>" line, command output suppressed.
#   failure → "✗ <label>", the de-noised output, then "→ fix: <hint>".
# Returns the command's real exit code so callers can branch (skip tests when
# the build is already broken).
step() {
  local label="$1" fix="$2"; shift 2
  local out="" rc started=$SECONDS
  if [ -n "${rust_step_evidence:-}" ]; then
    # Spool while the command is alive: an outer deadline must not discard
    # its unfinished build/test output with a shell command-substitution buffer.
    if ! printf '%s\n' "$@" > "$rust_step_evidence/$label.arguments.txt"; then
      fail=1
      return 1
    fi
    if "$@" > "$rust_step_evidence/$label.log" 2>&1; then
      rc=0
    else
      rc=$?
    fi
    if ! printf '%s\t%s\t%s\n' "$label" "$((SECONDS - started))" "$rc" \
        >> "$rust_step_evidence/timings.tsv"; then
      echo "ERROR: could not retain Rust step timing evidence" >&2
      # An evidence fault rejects an otherwise successful command, but cannot
      # replace an existing failure (especially admission's stop status 75).
      [ "$rc" -ne 0 ] || rc=1
    fi
    [ "$rc" -eq 0 ] || out=$(cat "$rust_step_evidence/$label.log")
  elif out=$("$@" 2>&1); then
    rc=0
  else
    rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    echo "✓ $label"
    return 0
  fi
  {
    echo ""
    echo "✗ $label"
    printf '%s\n' "$out" | grep -Ev "$NOISE"
    echo ""
    if [ "$rc" -eq 75 ]; then
      echo "→ restore: host resource qualification; this command was not admitted"
    else
      echo "→ fix: $fix"
    fi
  } >&2
  fail=1
  if [ "$rc" -eq 75 ]; then
    echo "resource qualification unavailable; remaining Rust checks were not started" >&2
    exit 75
  fi
  return "$rc"
}

if [ "$mode" = "--self-test" ]; then
  # Causes: S1 the wrapped command succeeds; F1 it fails while a later step is
  # eligible. Effects: S1 returns zero and leaves the failure aggregate clear;
  # F1 returns the exact nonzero status, marks the aggregate, and prevents the
  # caller's success branch. Constraint: the probe must exercise this same
  # `step` function without invoking Cargo. Decision rules: S1 -> continue;
  # F1 -> fail and skip dependent work.
  if ! (
    fail=0
    step "self-test-success" "not applicable" true >/dev/null 2>&1
    [ "$fail" -eq 0 ]
  ); then
    echo "check-rust self-test: a successful step did not remain successful" >&2
    exit 1
  fi
  if ! (
    fail=0
    rc=0
    step "self-test-failure" "not applicable" sh -c 'exit 23' >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 23 ] && [ "$fail" -eq 1 ]
  ); then
    echo "check-rust self-test: a failed step lost its status or failure aggregate" >&2
    exit 1
  fi
  # Evidence decision table: C1 output precedes command completion; C2 the
  # command exits 23; C3 the timing sink is a directory; C4 admission exits 75.
  # E1 bytes are already on disk inside the running command; E2 exact status
  # and aggregate survive; E3 a successful command cannot claim a logged pass;
  # E4 no dependent command starts. These execute this same step function.
  mkdir -p "$CARGO_TARGET_DIR/test-timings" || exit 1
  self_test_evidence=$(mktemp -d "$CARGO_TARGET_DIR/test-timings/rust-step-self-test.XXXXXX") || exit 1
  if ! (
    rust_step_evidence="$self_test_evidence/recorded"
    mkdir -p "$rust_step_evidence"
    fail=0
    step "streamed" "not applicable" sh -c \
      'printf "started\n"; test -s "$1" || exit 1; printf "stderr\n" >&2' \
      probe "$rust_step_evidence/streamed.log" >/dev/null 2>&1 || exit 1
    [ "$fail" -eq 0 ] || exit 1
    grep -Fxq started "$rust_step_evidence/streamed.log" || exit 1
    grep -Fxq stderr "$rust_step_evidence/streamed.log" || exit 1
    rc=0
    step "failed" "not applicable" sh -c 'printf "failure\n" >&2; exit 23' \
      >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 23 ] && [ "$fail" -eq 1 ] || exit 1
    grep -Fxq failure "$rust_step_evidence/failed.log" || exit 1
    awk -F '\t' '$1 == "failed" && $3 == 23 { matched=1 } END { exit !matched }' \
      "$rust_step_evidence/timings.tsv" || exit 1
    rc=0
    (step "resource" "not applicable" sh -c 'exit 75' >/dev/null 2>&1
      touch "$rust_step_evidence/dependent-started") || rc=$?
    [ "$rc" -eq 75 ] && [ ! -e "$rust_step_evidence/dependent-started" ] || exit 1
    rust_step_evidence="$self_test_evidence/unwritable"
    mkdir -p "$rust_step_evidence/timings.tsv"
    fail=0
    rc=0
    step "unrecorded" "not applicable" true >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] && [ "$fail" -eq 1 ] || exit 1
    rc=0
    step "unrecorded-failure" "not applicable" sh -c 'exit 23' >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 23 ] || exit 1
    rc=0
    (step "unrecorded-resource" "not applicable" sh -c 'exit 75' >/dev/null 2>&1
      touch "$rust_step_evidence/dependent-started") || rc=$?
    [ "$rc" -eq 75 ] && [ ! -e "$rust_step_evidence/dependent-started" ]
  ); then
    echo "check-rust self-test: streamed evidence or refusal lost its boundary" >&2
    exit 1
  fi
  echo "check-rust self-test: success, failure and streamed evidence are exact"
  exit 0
fi

# A full run builds the whole workspace plus every test binary. When the target
# filesystem cannot hold that, the first symptom is a linker or codegen error on
# an arbitrary crate, which reads as a source defect. Check the named cause.
if [ "$mode" = "--full" ]; then
  python3 scripts/ci/gate_tools.py disk --stage rust-full || exit $?
fi

# These are diagnostics, never a full-gate qualification receipt. Every run
# retains its own source/patch, argv, original output and elapsed/status rows.
mkdir -p "$CARGO_TARGET_DIR/test-timings" || exit 1
rust_step_evidence=$(mktemp -d "$CARGO_TARGET_DIR/test-timings/rust-gate.XXXXXX") || exit 1
git rev-parse HEAD > "$rust_step_evidence/source.txt" || exit 1
git -c core.fsmonitor=false diff --binary HEAD > "$rust_step_evidence/working-tree.patch" || exit 1
printf '%s\n' "$mode" > "$rust_step_evidence/mode.txt" || exit 1
printf 'stage\telapsed_seconds\texit_code\n' > "$rust_step_evidence/timings.tsv" || exit 1
echo "Rust stage evidence: $rust_step_evidence"

step "dependency-sources" "use an allowlisted HTTPS Git repository and pin a full 40-character rev" \
  python3 scripts/ci/check_dependency_sources.py
step "authority-arithmetic" "replace lossy or saturating authority arithmetic with checked conversions" \
  python3 scripts/ci/check_authority_arithmetic.py
step "feature-matrix" "add the crate to [workspace.members] or register the feature's substitute verifier" \
  python3 scripts/ci/check_feature_matrix.py

# The sole Rust gate runs toolchain-independent checks even without Cargo.
# A missing toolchain still fails; it cannot suppress the static evidence or
# become a successful skip after removal of the redundant standalone hooks.
if ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: Cargo is required for Rust checks." >&2
  exit 1
fi

# The workspace suite runs under nextest because nextest is the only runner
# that can bound a single test. libtest cannot: a test that awaits a signal no
# one will send parks its runtime forever, and one such test wedges this whole
# gate with no output and no end. The deadline lives in .config/nextest.toml.
# Falling back to `cargo test` here would silently restore the unbounded run,
# so a missing runner fails the gate instead of quietly weakening it.
if [ "$mode" = "--full" ] && ! cargo nextest --version >/dev/null 2>&1; then
  echo "ERROR: cargo-nextest is required to run the test suite under a per-test" >&2
  echo "       deadline. Install it with: cargo install cargo-nextest --locked" >&2
  exit 1
fi

step "fmt-batch-self-test" "fix the native workspace formatting planner" \
  python3 scripts/ci/check_rustfmt.py --self-test
step "fmt" "cargo fmt -p <reported-package>" \
  python3 scripts/ci/check_rustfmt.py

# Cause/effect rule CR1: when Runtime Host is resolved without default
# features (cause), its neutral credential-refresh port must still compile
# without acquiring Vault authority (effect). Checking it separately keeps
# workspace feature unification from masking this product-consumer graph.
step "runtime-host-neutral" "keep neutral Runtime Host ports available without enabling authority dependencies" \
  cargo check -p awaken-session-executor --no-default-features "${locked[@]}"

# Compile-surface decision table: CR10 the Managed HTTP library is a production
# root without test-support; CR11 it calls a fixture-only Session accessor;
# CR12 workspace tests enable test-support through feature unification.
# R1 CR10 + CR11 -> reject the missing production method; R2 CR10 + the narrowed
# Session roles -> compile; R3 CR12 may compile either implementation and cannot
# substitute for R1/R2. Keep this in the one Rust gate, before workspace checks,
# so quick, push and full exercise the same real production dependency graph.
step "managed-protocol-production" "use production Session roles rather than fixture-only accessors" \
  cargo check -p awaken-protocol-managed --lib --no-default-features "${locked[@]}"

# Process assembly compile-surface decision table: C1 the default product graph
# has neither cfg(test) nor test-support; C2 fixture builds enable one of them.
# E1 C1 must exclude fixture-only URLs and imports without dead-code warnings;
# E2 C2 retains the same constructor and ControlStores type from their owners.
# P1=C1 -> warning-free production libraries below; P2=C2 -> the existing
# workspace test/all-feature gates exercise the unchanged fixture callers.
# A workspace-only check can unify C2 into C1 and falsely pass P1.
step "process-assembly-production" "keep fixture-only Coordinator and CLI helpers out of the production graph" \
  cargo clippy -p awaken-coordinator -p awaken-cli --lib "${locked[@]}" -- -D warnings

# The fuzz crate is a separate workspace and the only Rust consumer of this
# repository's crates outside it, so `cargo check --workspace` never compiles it.
# It is why three crates keep reviewed public API snapshots (ADR-0081, 2026-09-28);
# compiling it is the check that its surface still builds.
step "fuzz-consumer" "update the fuzz targets to the changed contract surface" \
  cargo check --manifest-path fuzz/Cargo.toml --bins "${locked[@]}"

# Compile-surface decision table: CR2 a database-less Worker consumes Ingress
# without durable/test support -> compile its production library warning-free;
# CR3 a workspace peer enables durable -> that unified graph cannot substitute
# for CR2; CR4 store/unit/Kani consumers still use the same bounded arithmetic.
# This separate root detects orphaned authority helpers that cargo check on a
# consumer only warns about. Default/all-target and formal gates retain CR4.
# CR5 cfg(test) activates memory fixtures, not SQL adapters: execute the same
# pure kernels there; SQL migration fixtures stay in the default/durable suite.
step "run-ingress-neutral" "keep store-only arithmetic out of the database-less Worker graph" \
  cargo clippy -p awaken-dispatch --no-default-features --lib "${locked[@]}" -- -D warnings
step "run-ingress-neutral-unit" "keep memory and domain tests independent from optional SQL drivers" \
  cargo test -p awaken-dispatch --no-default-features --lib "${locked[@]}"

# CR7 a brokered client needs catalog/HTTP contracts but no Admin database:
# its actual transitive graph must compile warning-free without workspace SQL
# feature unification. CR8 SQL adapters/tests still own migration-history helpers;
# the workspace check below and Admin schema tests retain those consumers. Do not
# silence dead-code warnings or enable a database merely to hide this boundary.
# CR9 default authority must be disabled explicitly: both brokered tests and
# the adapter-absent rejection test formerly called an authority-only constructor.
# Cross adapter present/absent with no authority; the workspace graph retains
# authority-enabled consumers. Execute the same tests, not cfg-filtered copies.
for materializer_profile in bare brokered; do
  materializer_features=()
  if [ "$materializer_profile" = brokered ]; then
    materializer_features=(--features brokered)
  fi
  step "credential-materializer-$materializer_profile-neutral" "keep database-only helpers out of neutral materializer consumers" \
    cargo clippy -p awaken-credential-materializer --no-default-features "${materializer_features[@]}" --all-targets "${locked[@]}" -- -D warnings
  step "credential-materializer-$materializer_profile-neutral-unit" "keep neutral materializer tests independent of local Vault authority" \
    cargo test -p awaken-credential-materializer --no-default-features "${materializer_features[@]}" --lib "${locked[@]}"
done

# CR6 optional Pod transport and its production startup are not enabled by
# default -> include the owning CLI feature, which also enables Worker and
# security adapters. A leaf-only check would miss configuration/wiring drift.
# The full gate retains its existing real Hand companion in the same graph.
workspace_features="awaken-cli/container-k8s"
case "$mode" in
  --quick|--push)
    # Causal regression: the physical Attempt owns chained ScheduledActions;
    # embedding its concrete Future in Worker cancellation exceeded rustc's
    # layout-query depth even though ingress tests compiled. This all-targets
    # workspace check must include the production Worker composition. Boxing at
    # the owned boundary fixes layout without detaching execution or increasing
    # recursion limits; an ingress-only replacement would lose this coverage.
    step "check" "fix the compile errors shown above" \
      cargo check --workspace --all-targets --features "$workspace_features" "${locked[@]}"
    if [ "$mode" = "--push" ]; then
      # Push cause/effect table. P1 a commit compiles but carries dead code or
      # another `-D warnings` lint -> the quick check passes, main goes red at
      # the first full run; the pre-push gate runs the same workspace clippy
      # the full gate runs (same feature set, same locked graph) so the lint
      # is refused before it leaves the machine. P2 a Kani harness cannot be
      # planned (no @obligation anchor, a bad @kani tag, unreachable from its
      # Cargo target) -> the formal group fails hours later; the ledger check
      # derives the plan in seconds, so it runs here. Neither is
      # a test run: the workspace suite stays in --full.
      #
      # P3 an optional feature is declared but no gate compiles it -> a renamed
      # trait method, a deleted port or a builder added to one tier and not its
      # sibling rots behind the feature wall and reaches main; worse, a crate
      # that fails to compile stops its dependents from compiling, so one such
      # rot hides the next.
      #
      # P1 and P3 used to be two steps over two different graphs: clippy saw one
      # named feature, `cargo check` saw all 108. So the lint never ran over the
      # graph that compiled -- a warning-free `--features awaken-cli/container-k8s`
      # said nothing about the feature-gated code the maximal graph pulls in.
      # One step, one graph, both questions: clippy compiles everything
      # `cargo check` compiled and lints all of it. Compile only: these tiers
      # need a live daemon to run, which their own integration gates provide.
      step "lint-all-features" "review the errors above; every declared feature must compile and lint clean (auto-fixable: cargo clippy --fix --all-features --all-targets --allow-dirty)" \
        cargo clippy --workspace --all-features --all-targets "${locked[@]}" -- -D warnings
      step "formal-coverage" "fix the harness anchors/@kani tags or formal/obligations/ as shown above" \
        python3 scripts/ci/check_formal_coverage.py
      # The formal-evidence pre-commit hook now triggers on every production
      # Rust file and answers the same question incrementally (--staged), so an
      # unclassified module is refused at the commit that introduces it. This
      # whole-tree run stays here because the coverage *ratio* it reports is
      # global: only a full walk can show a surface that lost its last owner.
      step "formal-surface" "classify the new source in its own opening lines (// @formal surface ... with a // @formal reason:)" \
        python3 scripts/ci/check_formal_surface.py --require-complete
    fi
    ;;
  --full)
    # C1 Cargo's user build-dir differs from target-dir; C2 Scenario tests
    # resolve the real Hand beside their test executable. E1 one full-test
    # artifact tree preserves Cargo's sibling layout for either C1 value.
    export CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR"
    workspace_features="$(python3 scripts/ci/rust_test_plan.py --features)" || exit 1
    # The maximal graph, compiled and linted in one step -- the same command
    # --push runs, so the two lanes cannot diverge in what they see. The test
    # feature set below stays narrower on purpose: `--all-features` enables
    # tiers whose tests need a live daemon, and their own integration gates run
    # those. Compiling and linting them is free; running them is not.
    if step "lint-all-features" "review the errors above; every declared feature must compile and lint clean (auto-fixable: cargo clippy --fix --all-features --all-targets --allow-dirty)" \
      cargo clippy --workspace --all-features --all-targets "${locked[@]}" -- -D warnings; then
      # Scenario/Coordinator integration uses the real release Hand companion.
      # C1 full workspace tests => E1 build its authored feature in the same
      # Cargo graph; a thin default binary must not replace the tested companion.
      # nextest bounds every test with the deadline in .config/nextest.toml;
      # it does not run doctests, so the doctest target keeps its own step
      # rather than silently going unrun.
      # Build with Nextest's underlying Cargo command. Discovery happens once,
      # in the execution step, while this step retains stable HTML build timings
      # without nightly JSON instrumentation.
      if step "test-build" "fix the test compilation/link errors shown above" \
        cargo test --workspace --features "$workspace_features" "${locked[@]}" --no-run --timings; then
        if [ "${AWAKEN_RUST_SHARDS:-1}" = 1 ]; then
          step "test" "re-run a single failure with: cargo nextest run --workspace -E 'test(<test_name>)' --no-capture" \
            cargo nextest run --workspace --features "$workspace_features" "${locked[@]}"
        else
          shard_threads=()
          if [ -n "${NEXTEST_TEST_THREADS:-}" ] && [ "$NEXTEST_TEST_THREADS" != num-cpus ]; then
            shard_threads=(--threads "$NEXTEST_TEST_THREADS")
          fi
          step "test" "inspect the partition receipts; all listed cases must execute exactly once" \
            python3 scripts/ci/rust_test_shards.py --shards "$AWAKEN_RUST_SHARDS" "${shard_threads[@]}" \
              --timeout "${AWAKEN_RUST_TEST_TIMEOUT_SECONDS:-1800}" -- \
              --workspace --features "$workspace_features" "${locked[@]}"
        fi
      else
        echo "   (skipping workspace execution until test binaries compile)" >&2
      fi
      step "doctest" "re-run with: cargo test --workspace --doc <test_name> -- --nocapture" \
        cargo test --workspace --doc --features "$workspace_features" "${locked[@]}"
    else
      echo "   (skipping tests until the maximal graph lints clean)" >&2
    fi
    ;;
  *)
    echo "ERROR: unknown mode: $mode (expected --quick, --push or --full)" >&2
    exit 1
    ;;
esac

[ "$fail" -eq 0 ] || exit 1
