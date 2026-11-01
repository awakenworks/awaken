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

source scripts/ci/_build_observation.sh
awaken_begin_build_observation "rust:$mode" || exit $?

# A full run builds the whole workspace plus every test binary. When the target
# filesystem cannot hold that, the first symptom is a linker or codegen error on
# an arbitrary crate, which reads as a source defect. Check the named cause.
if [ "$mode" = "--full" ]; then
  python3 scripts/ci/gate_tools.py disk --stage rust-full || exit $?
fi

# These are diagnostics, never a full-gate qualification receipt. Every run
# retains its own source/patch, argv, original output and elapsed/status rows.
mkdir -p "$CARGO_TARGET_DIR/test-timings" || exit 1
rust_step_evidence=$(mktemp -d "$AWAKEN_BUILD_RUN_DIRECTORY/rust-gate.XXXXXX") || exit 1
git rev-parse HEAD > "$rust_step_evidence/source.txt" || exit 1
git -c core.fsmonitor=false diff --binary HEAD > "$rust_step_evidence/working-tree.patch" || exit 1
printf '%s\n' "$mode" > "$rust_step_evidence/mode.txt" || exit 1
printf 'stage\telapsed_seconds\texit_code\n' > "$rust_step_evidence/timings.tsv" || exit 1
echo "Rust stage evidence: $rust_step_evidence"

step "dependency-sources" "use an allowlisted HTTPS Git repository and pin a full 40-character rev" \
  python3 scripts/ci/check_dependency_sources.py
step "trust-roots" "keep the immutable compatibility shim connected to patched certificate data" \
  python3 scripts/ci/check_trust_roots.py
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

# Scope of the separate-graph steps below (decision table in rust_test_plan.py
# gate_scope). S1 --full -> every step runs. S2 --quick compares HEAD with the
# index and working tree; --push compares the merge-base with origin/main, i.e.
# every commit about to leave the machine. S3 "none" -> the step is skipped and
# says so; "packages" -> it runs when its root, or a package it consumes, is a
# changed owner or a transitive consumer of one. S4 "whole", an unknown base or
# a planner refusal -> every step runs, exactly as before scoping existed.
scope="whole"
scope_reason="full gate"
scoped_packages=""
scope_base=""
if [ "$mode" = "--full" ]; then
  echo "Rust gate scope: whole (full gate)"
else
  if [ "$mode" = "--push" ]; then
    # Without an origin/main merge-base, the root commit is the base: every
    # path counts as changed, so the plan stays whole.
    scope_base=$(git merge-base HEAD refs/remotes/origin/main 2>/dev/null) \
      || scope_base=$(git rev-list --max-parents=0 HEAD | tail -n 1)
  else
    scope_base=HEAD
  fi
  if scope_plan=$(python3 scripts/ci/rust_test_plan.py --gate-scope --base "$scope_base"); then
    scope=$(printf '%s\n' "$scope_plan" | sed -n 's/^scope=//p')
    scope_reason=$(printf '%s\n' "$scope_plan" | sed -n 's/^reason=//p')
    scoped_packages=$(printf '%s\n' "$scope_plan" | sed '1,2d')
  else
    scope_reason="gate scope unavailable; every step runs"
  fi
  echo "Rust gate scope: $scope ($scope_reason)"
fi

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
  if affects $packages; then
    step "$label" "$fix" "$@"
  else
    echo "↷ $label (unaffected: $scope_reason)"
  fi
}

# Path dependencies of an out-of-workspace consumer manifest: its scoped roots.
consumed_packages() {
  sed -n 's/^\(awaken-[a-z0-9-]*\) *=.*path *=.*/\1/p' "$1"
}

# Cause/effect rule CR1: when Runtime Host is resolved without default
# features (cause), its neutral credential-refresh port must still compile
# without acquiring Vault authority (effect). Checking it separately keeps
# workspace feature unification from masking this product-consumer graph.
scoped_step "runtime-host-neutral" "keep neutral Runtime Host ports available without enabling authority dependencies" \
  "awaken-session-executor" cargo check -p awaken-session-executor --no-default-features "${locked[@]}"

# The Host's test fixtures must name their own dev capabilities. The maximal
# workspace enables store test-support through other members and can hide an
# undeclared fixture constructor. Compile this package's tests independently;
# keep the production-only check above so dev capabilities cannot mask CR1.
scoped_step "runtime-host-neutral-tests" "declare every fixture capability in the consuming crate's dev dependencies" \
  "awaken-session-executor" cargo check -p awaken-session-executor --no-default-features --tests "${locked[@]}"

# Compile-surface decision table: CR10 the Managed HTTP library is a production
# root without test-support; CR11 it calls a fixture-only Session accessor;
# CR12 workspace tests enable test-support through feature unification.
# R1 CR10 + CR11 -> reject the missing production method; R2 CR10 + the narrowed
# Session roles -> compile; R3 CR12 may compile either implementation and cannot
# substitute for R1/R2. Keep this in the one Rust gate, before workspace checks,
# so quick, push and full exercise the same real production dependency graph.
scoped_step "managed-protocol-production" "use production Session roles rather than fixture-only accessors" \
  "awaken-protocol-managed" cargo check -p awaken-protocol-managed --lib --no-default-features "${locked[@]}"

# Process assembly compile-surface decision table: C1 is the ordinary Coordinator
# root; C2 is the ordinary CLI root; C3 enables cfg(test) or test-support.
# E1 C1 excludes fixture-only URLs/imports and its concrete materializer edge;
# E2 C2 retains its selected production adapters; both compile without warnings.
# E3 C3 retains the same constructors, ControlStores and fixture materializer.
# P1=C1 -> the first isolated compiler invocation; P2=C2 -> the second;
# P3=C3 -> the existing workspace test/all-feature gates. Resolving C1 and C2
# together can unify CLI features into Coordinator and cannot substitute for P1.
scoped_step "coordinator-production" "keep fixture-only Coordinator dependencies out of its ordinary product graph" \
  "awaken-coordinator" cargo clippy -p awaken-coordinator --lib "${locked[@]}" -- -D warnings
scoped_step "cli-production" "keep fixture-only CLI helpers out of its ordinary product graph" \
  "awaken-cli" cargo clippy -p awaken-cli --lib "${locked[@]}" -- -D warnings
scoped_step "process-startup-production" "keep fixture-only startup helpers out of its ordinary product graph" \
  "awaken-process-startup" cargo clippy -p awaken-process-startup --lib "${locked[@]}" -- -D warnings

# ACP role decision table: B1 the isolated Coordinator product resolves only
# catalog/blob custody; B2 the backend's no-default build cannot name physical
# execution or host preparation; B3 that build retains the SAME subject-scoped
# blob owner and its erasure/retry assertions. A workspace/all-feature build
# legitimately unifies Worker/CLI roles and cannot prove B1/B2. These are
# additional checks; the original full/default executor suite remains below.
scoped_step "coordinator-acp-role" "keep ACP physical implementations out of the isolated Coordinator product" \
  "awaken-coordinator-process" python3 scripts/ci/_dispatch_boundary_fitness.py \
  --check-acp-product-features awaken-coordinator-process
scoped_step "control-acp-role" "keep ACP physical implementations out of the isolated Control product" \
  "awaken-control-process" python3 scripts/ci/_dispatch_boundary_fitness.py \
  --check-acp-product-features awaken-control-process
scoped_step "acp-metadata-production" "compile the catalog and blob owner warning-free without execution roles" \
  "awaken-run-executor-acp" cargo clippy "${locked[@]}" -p awaken-run-executor-acp --lib --no-default-features -- -D warnings
scoped_step "acp-metadata-compile-contract" "compile catalog and blob custody while refusing physical ACP modules" \
  "awaken-run-executor-acp" cargo test "${locked[@]}" -p awaken-run-executor-acp --no-default-features --doc
# B5 execution without trusted-host preparation retains the executor type while
# the same negative host-module contract still compiles as a rejection.
scoped_step "acp-execution-without-host" "keep Sandbox execution independent of trusted-host preparation" \
  "awaken-run-executor-acp" cargo test "${locked[@]}" -p awaken-run-executor-acp --no-default-features --features execution --doc
scoped_step "acp-metadata-custody" "retain the original subject-scoped blob erasure and retry behavior without ACP execution" \
  "awaken-run-executor-acp" run_required_rust_tests "${locked[@]}" -p awaken-run-executor-acp --no-default-features \
  --lib session_home::tests::subject_scopes_are_isolated_and_independently_erasable \
  --require-case=session_home::tests::subject_scopes_are_isolated_and_independently_erasable

# Feature admission table: W1 this Worker lacks its Kubernetes integration;
# W2 a different consumer enables the transport dependency's Kubernetes parser;
# W3 a complete Pod projection is supplied. W1+W2+W3 must reject at Worker
# configuration admission, while the full Worker feature retains Pod transport.
# A workspace/all-feature graph makes W1 false and cannot prove this refusal.
# Use the shared named-execution owner: an omitted, ignored or empty selection
# cannot qualify the process capability boundary.
scoped_step "worker-bootstrap-feature-union" "reject Pod identities when the Worker integration is absent, regardless of dependency feature unification" \
  "awaken-worker" run_required_rust_tests "${locked[@]}" -p awaken-worker --no-default-features \
  --features awaken-worker-transport-security/kubernetes --lib bootstrap::tests:: \
  --require-case=bootstrap::tests::pod_bootstrap_freezes_projected_identity_and_never_falls_back_to_signing \
  --require-case=bootstrap::tests::unavailable_pod_identity_rejects_before_projection_io_and_after_dto_mutation

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

# Two lint passes over one graph (docs/design/guard-consolidation.md, M4).
# `clippy.toml` and `[workspace.lints]` hold rules that bind what ships: no std
# lock, ambient clock, environment read or private pool construction outside a
# reviewed `#[expect]`, and no unreachable `pub`. Tests may use std locks, clocks
# and the environment, so the all-targets pass allows exactly those lints and
# the production pass (`--lib --bins`, compiled without cfg(test)) denies every
# warning. A source `#[expect]` outranks the command-line `-A`, so a reviewed
# production site holds in both passes. clippy 1.96 has no `allow-*-in-tests`
# option for these lints, which is why there are two passes.
production_only_lints=(-A clippy::disallowed_methods -A clippy::disallowed_types -A unreachable_pub)

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
  scoped_step "credential-materializer-$materializer_profile-neutral" "keep database-only helpers out of neutral materializer consumers" \
    "awaken-credential-materializer" cargo clippy -p awaken-credential-materializer --no-default-features "${materializer_features[@]}" --all-targets "${locked[@]}" -- -D warnings "${production_only_lints[@]}"
  scoped_step "credential-materializer-$materializer_profile-neutral-unit" "keep neutral materializer tests independent of local Vault authority" \
    "awaken-credential-materializer" cargo test -p awaken-credential-materializer --no-default-features "${materializer_features[@]}" --lib "${locked[@]}"
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
    # Layout decision rows: L1 the original owned physical-attempt Box/erasure
    # plus this production all-target graph -> compile. L2 removing only that
    # boundary while retaining the ScheduledAction loop, output type and claim/
    # cancellation/cleanup path must be checked by BOTH original whole argv and
    # this quick path on the same working subject. A real production layout
    # overflow is the negative effect; an unused import, inference error, other
    # factory or leaf-only success is not that effect. L3 admission75/source or
    # profile uncertainty -> retain the exact refusal, no retry/flag change.
    # UNKNOWN_CONFIG still keeps whole; a focused PASS cannot qualify a subset.
    # Both lanes plan the same check against their own base (S2 above); the
    # planner keeps the whole argv whenever it cannot prove a narrower one,
    # including when nothing changed (an incremental no-op after pre-commit).
    if [ "$mode" = "--quick" ]; then
      step "check" "fix the compile errors shown above" \
        python3 scripts/ci/rust_test_plan.py --base HEAD --run --check-command \
        cargo check --workspace --all-targets --features "$workspace_features" "${locked[@]}"
    else
      step "check" "fix the compile errors shown above" \
        python3 scripts/ci/rust_test_plan.py --base "$scope_base" --run --check-command \
        cargo check --workspace --all-targets --features "$workspace_features" "${locked[@]}"
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
      if [ "$scope" = none ]; then
        echo "↷ lint-all-features (unaffected: $scope_reason)"
      else
        step "lint-all-features" "review the errors above; every declared feature must compile and lint clean (auto-fixable: cargo clippy --fix --all-features --all-targets --allow-dirty)" \
          cargo clippy --workspace --all-features --all-targets "${locked[@]}" -- -D warnings "${production_only_lints[@]}"
        step "lint-production" "route the call through its owner (clippy.toml names it), or state a reviewed #[expect(clippy::disallowed_methods, reason = ..)] on the composition-root item" \
          cargo clippy --workspace --all-features --lib --bins "${locked[@]}" -- -D warnings
      fi
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
      && step "lint-production" "route the call through its owner (clippy.toml names it), or state a reviewed #[expect(clippy::disallowed_methods, reason = ..)] on the composition-root item" \
      cargo clippy --workspace --all-features --lib --bins "${locked[@]}" -- -D warnings; then
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
        step "test" "re-run a single failure with: cargo nextest run --workspace -E 'test(<test_name>)' --no-capture" \
          cargo nextest run --workspace --features "$workspace_features" "${locked[@]}"
      else
        echo "   (skipping workspace execution until test binaries compile)" >&2
      fi
      # Stable rustdoc ignores a compile_fail block's pinned E-code, so a body
      # failing for an unrelated reason would still pass. RUSTC_BOOTSTRAP=1
      # makes rustdoc check the pinned codes (ADR-0103).
      step "doctest" "re-run with: RUSTC_BOOTSTRAP=1 cargo test --workspace --doc <test_name> -- --nocapture" \
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
