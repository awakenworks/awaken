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

mode="${1:---full}"

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
  local out rc
  if out=$("$@" 2>&1); then
    echo "✓ $label"
    return 0
  else
    # Capture the command status while the failed `if` condition is still the
    # immediately preceding command.  Reading `$?` after the `if` statement
    # would observe the statement's own zero status and falsely continue.
    rc=$?
  fi
  {
    echo ""
    echo "✗ $label"
    printf '%s\n' "$out" | grep -Ev "$NOISE"
    echo ""
    echo "→ fix: $fix"
  } >&2
  fail=1
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
  echo "check-rust self-test: success and failure branches are exact"
  exit 0
fi

step "dependency-sources" "use an allowlisted HTTPS Git repository and pin a full 40-character rev" \
  python3 scripts/ci/check_dependency_sources.py
step "authority-arithmetic" "replace lossy or saturating authority arithmetic with checked conversions" \
  python3 scripts/ci/check_authority_arithmetic.py
step "crate-boundaries" "remove the illegal dependency shown above (a lower layer must not import a higher one)" \
  python3 scripts/ci/check_crate_boundaries.py

# The sole Rust gate runs toolchain-independent checks even without Cargo.
# A missing toolchain still fails; it cannot suppress the static evidence or
# become a successful skip after removal of the redundant standalone hooks.
if ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: Cargo is required for Rust checks." >&2
  exit 1
fi

step "fmt" "cargo fmt --all" \
  cargo fmt --all -- --check

# Cause/effect rule CR1: when Runtime Host is resolved without default
# features (cause), its neutral credential-refresh port must still compile
# without acquiring Vault authority (effect). Checking it separately keeps
# workspace feature unification from masking this product-consumer graph.
step "runtime-host-neutral" "keep neutral Runtime Host ports available without enabling authority dependencies" \
  cargo check -p awaken-session-executor --no-default-features "${locked[@]}"

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
step "brokered-inference-neutral" "keep database-only migration helpers out of the brokered client graph" \
  cargo clippy -p awaken-credential-materializer --features brokered --all-targets "${locked[@]}" -- -D warnings

# CR6 optional Pod transport and its production startup are not enabled by
# default -> include the owning CLI feature, which also enables Worker and
# security adapters. A leaf-only check would miss configuration/wiring drift.
# The full gate retains its existing real Hand companion in the same graph.
workspace_features="awaken-cli/container-k8s"
case "$mode" in
  --quick)
    # Causal regression: the physical Attempt owns chained ScheduledActions;
    # embedding its concrete Future in Worker cancellation exceeded rustc's
    # layout-query depth even though ingress tests compiled. This all-targets
    # workspace check must include the production Worker composition. Boxing at
    # the owned boundary fixes layout without detaching execution or increasing
    # recursion limits; an ingress-only replacement would lose this coverage.
    step "check" "fix the compile errors shown above" \
      cargo check --workspace --all-targets --features "$workspace_features" "${locked[@]}"
    ;;
  --full)
    # C1 Cargo's user build-dir differs from target-dir; C2 Scenario tests
    # resolve the real Hand beside their test executable. E1 one full-test
    # artifact tree preserves Cargo's sibling layout for either C1 value.
    export CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR"
    workspace_features+=",awaken-sandbox/hand"
    if step "clippy" "review the errors above; auto-fixable lints: cargo clippy --fix --all-targets --allow-dirty" \
      cargo clippy --workspace --all-targets --features "$workspace_features" "${locked[@]}" -- -D warnings; then
      # Scenario/Coordinator integration uses the real release Hand companion.
      # C1 full workspace tests => E1 build its authored feature in the same
      # Cargo graph; a thin default binary must not replace the tested companion.
      step "test" "re-run a single failure with: cargo test --workspace <test_name> -- --nocapture" \
        cargo test --workspace --features "$workspace_features" "${locked[@]}"
    else
      echo "   (skipping tests until clippy compiles)" >&2
    fi
    ;;
  *)
    echo "ERROR: unknown mode: $mode (expected --quick or --full)" >&2
    exit 1
    ;;
esac

[ "$fail" -eq 0 ] || exit 1
