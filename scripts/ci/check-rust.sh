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
source scripts/ci/_rust_test_execution.sh
awaken_configure_cargo_target "$PWD"
export CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR"
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
  local out="" rc started=$SECONDS timing_snapshot=""
  echo "[stage] $label: started; log ${rust_step_evidence:-not-recorded}/$label.log"
  if [ -n "${AWAKEN_BUILD_RUN_DIRECTORY:-}" ] && [[ " $* " == *" --timings "* ]]; then
    timing_snapshot=$(python3 scripts/ci/ci_performance.py timings-before --path "$CARGO_TARGET_DIR/cargo-timings") || return 74
  fi
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
  if [ -n "$timing_snapshot" ]; then
    python3 scripts/ci/ci_performance.py timings-after --snapshot "$timing_snapshot" || {
      [ "$rc" -ne 0 ] || rc=74
    }
  fi
  if [ "$rc" -eq 0 ]; then
    echo "✓ $label ($((SECONDS - started))s)"
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
      # One gate-faithful repro command per failure the step's output names.
      if [[ "$label" =~ ^(test|test-build|doctest)$ ]]; then
        printf '%s\n' "$out" | python3 scripts/ci/crate_test.py --repro-hint "$label" || true
      fi
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
  # H1 a failed test step whose output names a case prints the command that
  # reproduces that case the way the step ran it (crate_test.py --repro-hint).
  hint=$(fail=0; step "test" "not applicable" sh -c \
    'printf "        FAIL [ 0.1s] demo::it tests::boom\n"; exit 1' 2>&1 >/dev/null)
  if [[ "$hint" != *"→ fix: reproduce demo::it tests::boom: scripts/ci/test.sh -p demo -E 'binary_id(=demo::it) & test(=tests::boom)'"* ]]; then
    echo "check-rust self-test: a failed test step did not print its repro command" >&2
    exit 1
  fi
  echo "check-rust self-test: success, failure and streamed evidence are exact"
  exit 0
fi

export AWAKEN_BUILD_WORKLOAD_KIND="${AWAKEN_BUILD_WORKLOAD_KIND:-feedback}"
export AWAKEN_BUILD_OBSERVATION_KIND="${AWAKEN_BUILD_OBSERVATION_KIND:-project}"
# Performance telemetry is opt-in (AWAKEN_BUILD_OBSERVE=1, or a caller that
# already owns an observation run); it never decides or delays the gate.
if [ "${AWAKEN_BUILD_OBSERVE:-0}" = 1 ] || [ -n "${AWAKEN_BUILD_RUN_DIRECTORY:-}" ]; then
  . scripts/ci/_build_observation.sh
  awaken_begin_build_observation "rust:$mode" || exit $?
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
if [ -n "${AWAKEN_RUST_GATE_EVIDENCE:-}" ]; then
  # The admitted Cargo half of this same run keeps the first half's record.
  rust_step_evidence="$AWAKEN_RUST_GATE_EVIDENCE"
else
  rust_step_evidence=$(mktemp -d "${AWAKEN_BUILD_RUN_DIRECTORY:-$CARGO_TARGET_DIR/test-timings}/rust-gate.XXXXXX") || exit 1
  git rev-parse HEAD > "$rust_step_evidence/source.txt" || exit 1
  git -c core.fsmonitor=false diff --binary HEAD > "$rust_step_evidence/working-tree.patch" || exit 1
  printf '%s\n' "$mode" > "$rust_step_evidence/mode.txt" || exit 1
  printf 'stage\telapsed_seconds\texit_code\n' > "$rust_step_evidence/timings.tsv" || exit 1
  echo "Rust stage evidence: $rust_step_evidence"
fi

# Scope of every step below (decision table in rust_test_plan.py gate_plan).
# S1 --full -> every step runs. S2 --quick compares HEAD with the index and
# working tree; --push compares the merge-base with origin/main, i.e. every
# commit about to leave the machine. S3 "none" -> nothing to do. "packages"
# -> a step runs when its root, or a package it consumes, is a changed owner or
# a transitive consumer of one. S4 "whole", an unknown base, a planner refusal
# or a change to the selector itself -> every step runs. S5 a changed gate
# script (script= lines) also runs every step whose command executes it.
scope_base=""
if [ -n "${AWAKEN_RUST_GATE_PLAN:-}" ]; then
  scope_plan="$AWAKEN_RUST_GATE_PLAN"
  scope_base="${AWAKEN_RUST_GATE_BASE:-HEAD}"
else
  scope_plan=$'scope=whole\nreason=full gate'
  if [ "$mode" = "--push" ]; then
    # Without an origin/main merge-base, the root commit is the base: every
    # path counts as changed, so the plan stays whole.
    scope_base=$(git merge-base HEAD refs/remotes/origin/main 2>/dev/null) \
      || scope_base=$(git rev-list --max-parents=0 HEAD | tail -n 1)
  else
    scope_base=HEAD
  fi
  if [ "$mode" = "--full" ]; then
    :
  else
    plan_mode="$mode"
    scope_plan=$(python3 scripts/ci/rust_test_plan.py --gate-scope --base "$scope_base" --mode "${plan_mode#--}" --explain) \
      || scope_plan=$'scope=whole\nreason=gate scope unavailable; every step runs'
  fi
  echo "Rust gate scope: $(printf '%s\n' "$scope_plan" | sed -n 's/^scope=//p') ($(printf '%s\n' "$scope_plan" | sed -n 's/^reason=//p'))"
fi
scope=$(printf '%s\n' "$scope_plan" | sed -n 's/^scope=//p')
scope_reason=$(printf '%s\n' "$scope_plan" | sed -n 's/^reason=//p')
reached_scripts=$(printf '%s\n' "$scope_plan" | sed -n 's/^script=//p')
scoped_packages=$(printf '%s\n' "$scope_plan" | grep -v '=' || true)
rust_changed=0
if [ "$scope" = whole ] || [ -n "$scoped_packages" ]; then
  rust_changed=1
fi

# runs_reached <command...>: whether a step's command (or the shell function it
# names) executes a gate script this change reached.
runs_reached() {
  [ -n "$reached_scripts" ] || return 1
  local text="$*" script
  if declare -F "$1" >/dev/null; then
    text="$text $(declare -f "$1")"
  fi
  while IFS= read -r script; do
    if [ -n "$script" ] && [[ "$text" == *"scripts/ci/$script"* ]]; then
      return 0
    fi
  done <<< "$reached_scripts"
  return 1
}

# gate_step "<label>" "<fix hint>" <command...>: a step every Rust change needs;
# a gate-script-only change runs it only when the step executes that script.
gate_step() {
  local label="$1"
  if [ "$rust_changed" -eq 1 ] || runs_reached "${@:3}"; then
    step "$@"
  else
    echo "↷ $label (unaffected: $scope_reason)"
  fi
}

if [ "${AWAKEN_RUST_GATE_PHASE:-}" != cargo ]; then
gate_step "dependency-sources" "use an allowlisted HTTPS Git repository and pin a full 40-character rev" \
  python3 scripts/ci/check_dependency_sources.py
gate_step "trust-roots" "keep the immutable compatibility shim connected to patched certificate data" \
  python3 scripts/ci/check_trust_roots.py
gate_step "authority-arithmetic" "replace lossy or saturating authority arithmetic with checked conversions" \
  python3 scripts/ci/check_authority_arithmetic.py
gate_step "feature-matrix" "add the crate to [workspace.members] or register the feature's substitute verifier" \
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

gate_step "fmt-batch-self-test" "fix the native workspace formatting planner" \
  python3 scripts/ci/check_rustfmt.py --self-test
gate_step "fmt" "cargo fmt -p <reported-package>" \
  python3 scripts/ci/check_rustfmt.py

fi

# One host build admission for the whole Cargo half, not one queue per step:
# 30 sequential steps once spent 6,224 s of a 7,056 s hook waiting in line. The
# unadmitted static half above runs first, so a cheap failure stops before any
# queue. This script re-enters itself once under the existing
# `run_with_deadline --admit` owner; every Cargo call inside inherits that job
# (AWAKEN_CARGO_ADMITTED) and does not queue again. Without the host policy
# (another OS, a CI runner) each Cargo call runs as before, and so does a
# gate-script-only change, whose few reached steps are not worth one queue.
if [ "${AWAKEN_RUST_GATE_PHASE:-}" != cargo ] && [ "${AWAKEN_CARGO_ADMITTED:-}" != 1 ] \
    && [ "$rust_changed" -eq 1 ] && [ "$(uname -s)" = Linux ] \
    && host_cargo_directory=$(awaken_host_cargo_directory 2>/dev/null); then
  if [ "$fail" -ne 0 ]; then
    echo "static checks failed; the Cargo steps were not queued (fix the errors above and rerun)" >&2
    exit 1
  fi
  admitted_marker="$rust_step_evidence/admitted"
  awaken_report_admission_wait "$admitted_marker" &
  wait_reporter=$!
  gate_budget="${AWAKEN_RUST_GATE_TIMEOUT_SECONDS:-14400}"
  echo "[admission] one host build admission for every Cargo step; now: $(awaken_admission_holder 2>/dev/null || echo 'holder unknown')"
  PATH="$host_cargo_directory:$PATH" AWAKEN_RUST_GATE_PHASE=cargo AWAKEN_RUST_GATE_EVIDENCE="$rust_step_evidence" \
    AWAKEN_RUST_GATE_PLAN="$scope_plan" AWAKEN_RUST_GATE_BASE="$scope_base" \
    AWAKEN_RUST_GATE_ADMITTED_MARKER="$admitted_marker" \
    run_with_deadline --admit "$gate_budget" scripts/ci/check-rust.sh "$mode"
  status=$?
  kill "$wait_reporter" 2>/dev/null
  wait "$wait_reporter" 2>/dev/null
  exit "$status"
fi
[ -z "${AWAKEN_RUST_GATE_ADMITTED_MARKER:-}" ] || : > "$AWAKEN_RUST_GATE_ADMITTED_MARKER"

# affects <package>... : whether a step rooted at these packages can observe the change.
affects() {
  if [ "$scope" = none ]; then
    return 1
  fi
  if [ "$scope" != packages ]; then
    return 0
  fi
  local package
  for package in "$@"; do
    if printf '%s\n' "$scoped_packages" | grep -Fxq "$package"; then
      return 0
    fi
  done
  return 1
}

# scoped_step "<label>" "<fix hint>" "<root packages>" <command...>
scoped_step() {
  local label="$1" fix="$2" packages="$3"
  shift 3
  # shellcheck disable=SC2086 # the root package list is space separated
  if affects $packages || runs_reached "$@"; then
    step "$label" "$fix" "$@"
  else
    echo "↷ $label (unaffected: $scope_reason)"
  fi
}

# Path dependencies of an out-of-workspace consumer manifest: its scoped roots.
consumed_packages() {
  sed -n 's/^\(awaken-[a-z0-9-]*\) *=.*path *=.*/\1/p' "$1"
}

# The Host's test fixtures must name their own dev capabilities. The maximal
# workspace enables store test-support through other members and can hide an
# undeclared fixture constructor. Compile this package's tests independently;
# the production lint below keeps dev capabilities out of its library graph.
scoped_step "runtime-host-neutral-tests" "declare every fixture capability in the consuming crate's dev dependencies" \
  "awaken-session-executor" cargo check -p awaken-session-executor --no-default-features --tests "${locked[@]}"

# The fuzz crate is a separate workspace, so `cargo check --workspace` never compiles it.
# It is why three crates keep reviewed public API snapshots (ADR-0081, 2026-09-28);
# compiling it is the check that its surface still builds.
# F1 changed authored contract versions + stale independent lock -> locked
# refusal. F2 Cargo-updated locked identities -> compile all original fuzz bins.
# Resolution remains separate from this check; it cannot silently rewrite a lock.
scoped_step "fuzz-consumer" "update the fuzz targets to the changed contract surface" \
  "$(consumed_packages fuzz/Cargo.toml)" cargo check --manifest-path fuzz/Cargo.toml --bins "${locked[@]}"

# SDK tasks are an independent, minimal-feature application graph. A green
# workspace build or API snapshot cannot substitute for this consumer behavior.
scoped_step "sdk-consumer" "keep the public SDK usable from an ordinary external application" \
  "$(consumed_packages examples/sdk-consumer/Cargo.toml)" \
  python3 scripts/ci/check_sdk_consumer.py

# Compile-surface decision table: CR2 a database-less Worker consumes Ingress
# without durable/test support -> compile its production library warning-free;
# CR3 a workspace peer enables durable -> that unified graph cannot substitute
# for CR2; CR4 store/unit/Kani consumers still use the same bounded arithmetic.
# This separate root detects orphaned authority helpers that cargo check on a
# consumer only warns about. Default/all-target and formal gates retain CR4.
# CR5 cfg(test) activates memory fixtures, not SQL adapters: execute the same
# pure kernels there; SQL migration fixtures stay in the default/durable suite.
scoped_step "run-ingress-neutral" "keep store-only arithmetic out of the database-less Worker graph" \
  "awaken-dispatch" cargo clippy -p awaken-dispatch --no-default-features --lib "${locked[@]}" -- -D warnings
scoped_step "run-ingress-neutral-unit" "keep memory and domain tests independent from optional SQL drivers" \
  "awaken-dispatch" cargo test -p awaken-dispatch --no-default-features --lib "${locked[@]}"

# Two lint passes (docs/design/guard-consolidation.md, M4).
# `clippy.toml` and `[workspace.lints]` hold rules that bind what ships: no std
# lock, ambient clock, environment read or private pool construction outside a
# reviewed `#[expect]`, and no unreachable `pub`. Tests may use std locks, clocks
# and the environment, so the all-targets pass allows exactly those lints and
# the production pass (`--lib --bins` over the production graph below, compiled
# without cfg(test) or test-support) denies every warning. A source `#[expect]` outranks the command-line `-A`, so a reviewed
# production site holds in both passes. clippy 1.96 has no `allow-*-in-tests`
# option for these lints, which is why there are two passes.
production_only_lints=(-A clippy::disallowed_methods -A clippy::disallowed_types -A unreachable_pub)

# The production pass is one graph for every product root: every crate except
# `layer = "tooling"`, every feature except test-support, libraries and
# binaries only (resolver 2 activates no dev dependency there). The
# crate-boundary checker refuses any normal edge, default or feature outside
# tooling that enables test-support, so a production call into a fixture-only
# item fails to compile here instead of being hidden by feature unification.
# It replaces the per-root production builds (Runtime Host, Managed protocol,
# Coordinator, CLI, process startup) and the former `--all-features` pass,
# which switched test-support on and so was not a production graph.
production_selection_text=$(python3 scripts/ci/rust_test_plan.py --production-selection) || exit 1
mapfile -t production_selection <<< "$production_selection_text"
lint_production_fix="route the call through its owner (clippy.toml names it), state a reviewed #[expect(clippy::disallowed_methods, reason = ..)] on the composition-root item, or move a fixture-only call behind cfg(test)/test-support"

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
    # Layout decision rows: L1 the original owned physical-attempt Box/erasure
    # plus this production all-target graph -> compile. L2 removing only that
    # boundary while retaining the ScheduledAction loop, output type and claim/
    # cancellation/cleanup path must be checked by BOTH original whole argv and
    # this quick path on the same working subject. A real production layout
    # overflow is the negative effect; an unused import, inference error, other
    # factory or leaf-only success is not that effect. L3 admission75/source or
    # profile uncertainty -> retain the exact refusal, no retry/flag change.
    # UNKNOWN_CONFIG still keeps whole; a focused PASS cannot qualify a subset.
    # The workspace check is incremental: unchanged crates are fresh, so the
    # whole argv costs only what changed. (A per-package narrowing used to run
    # here; its shared-unit peer closure kept 152 of 153 packages for every
    # single-crate change, so it decided nothing.) A gate-script-only change
    # compiles nothing.
    if [ "$rust_changed" -eq 0 ]; then
      echo "↷ check (unaffected: $scope_reason)"
    else
      step "check" "fix the compile errors shown above" \
        cargo check --workspace --all-targets --features "$workspace_features" "${locked[@]}" --timings
    fi
    if [ "$mode" = "--push" ]; then
      # A dev dependency may enable the upstream Host variant while the CLI's
      # own test-support stays off. The maximal graph cannot prove this row.
      scoped_step "cli-dev-feature-union" "resolve inference at its owner without consumer-side cfg assumptions" \
        "awaken-cli" cargo check -p awaken-cli --no-default-features --example credential_reference_worker "${locked[@]}"
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
      #
      # P4 this lint stays whole whenever any Rust input changed: --all-features
      # enables every member's features, so a package subset resolves a
      # different unified graph and cannot stand in for it. Only S3 "none"
      # (nothing it compiles changed since the push base) skips it.
      if [ "$rust_changed" -eq 0 ]; then
        echo "↷ lint-all-features (unaffected: $scope_reason)"
      else
        step "lint-all-features" "review the errors above; every declared feature must compile and lint clean (auto-fixable: cargo clippy --fix --all-features --all-targets --allow-dirty)" \
          cargo clippy --workspace --all-features --all-targets "${locked[@]}" -- -D warnings "${production_only_lints[@]}"
        step "lint-production" "$lint_production_fix" \
          cargo clippy --workspace "${production_selection[@]}" --lib --bins "${locked[@]}" -- -D warnings
      fi
      gate_step "formal-coverage" "fix the harness anchors/@kani tags or formal/obligations/ as shown above" \
        python3 scripts/ci/check_formal_coverage.py
      # The formal-evidence pre-commit hook now triggers on every production
      # Rust file and answers the same question incrementally (--staged), so an
      # unclassified module is refused at the commit that introduces it. This
      # whole-tree run stays here because the coverage *ratio* it reports is
      # global: only a full walk can show a surface that lost its last owner.
      gate_step "formal-surface" "classify the new source in its own opening lines (// @formal surface ... with a // @formal reason:)" \
        python3 scripts/ci/check_formal_surface.py --require-complete
    fi
    ;;
  --full)
    # A dev dependency may enable the upstream Host variant while the CLI's
    # own test-support stays off. The maximal graph cannot prove this row.
    scoped_step "cli-dev-feature-union" "resolve inference at its owner without consumer-side cfg assumptions" \
      "awaken-cli" cargo check -p awaken-cli --no-default-features --example credential_reference_worker "${locked[@]}"
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
      cargo clippy --workspace --all-features --all-targets "${locked[@]}" -- -D warnings "${production_only_lints[@]}" \
      && step "lint-production" "$lint_production_fix" \
      cargo clippy --workspace "${production_selection[@]}" --lib --bins "${locked[@]}" -- -D warnings; then
      # Scenario/Coordinator integration uses the real release Hand companion.
      # C1 full workspace tests => E1 build its authored feature in the same
      # Cargo graph; a thin default binary must not replace the tested companion.
      # nextest bounds every test with the deadline in .config/nextest.toml;
      # it does not run doctests, so the doctest target keeps its own step
      # rather than silently going unrun.
      # Build with Nextest's underlying Cargo command. Discovery happens once,
      # in the execution step, while this step retains stable HTML build timings
      # without nightly JSON instrumentation.
      if step "test-build" "fix the test compilation/link errors shown above; reproduce with: scripts/ci/test.sh -p <package> --no-run" \
        cargo test --workspace --features "$workspace_features" "${locked[@]}" --no-run --timings; then
        step "test" "reproduce one failure the way this step runs it: scripts/ci/test.sh -p <package> -E 'test(=<test_name>)'" \
          cargo nextest run --workspace --features "$workspace_features" "${locked[@]}"
      else
        echo "   (skipping workspace execution until test binaries compile)" >&2
      fi
      # Stable rustdoc ignores a compile_fail block's pinned E-code, so a body
      # failing for an unrelated reason would still pass. RUSTC_BOOTSTRAP=1
      # makes rustdoc check the pinned codes (ADR-0103).
      step "doctest" "reproduce with the pinned error codes checked: scripts/ci/test.sh --doc -p <package> ['<doctest name>']" \
        env RUSTC_BOOTSTRAP=1 cargo test --workspace --doc --features "$workspace_features" "${locked[@]}"
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
