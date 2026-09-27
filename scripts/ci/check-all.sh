#!/usr/bin/env bash
# Run every repository guardrail.
#
# On failure, ends with a summary that names each failed check and the exact
# command to reproduce it standalone, so the fix loop is: read summary → run
# one command → fix → retry.
#
# External CI can run independent gates concurrently without duplicating this
# release manifest: `scripts/ci/check-all.sh --group <name>`. The default remains
# the complete, sequential local gate.
set -uo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_provider_environment.sh
awaken_unset_ambient_api_keys
awaken_append_repo_tools

groups=(static docs rust api contracts formal reliability postgres kubernetes k3d frontend e2e sandbox)
requested_group=all
# `--full-gate` is the scheduled entry point: every group, a log, and a summary.
# `--record` additionally blesses scripts/ci/gate_record.json — but only after
# the run below finds nothing failed, because a record written by a partial run
# is exactly the claim nobody earned. docs/adr/0080 owns the policy.
full_gate=0
record_full_gate=0
original_arguments=("$@")
usage() {
  echo "usage: scripts/ci/check-all.sh [--group <name>|--list-groups|--full-gate [--record]]" >&2
  exit 2
}
case "${1:-}" in
  "") ;;
  --full-gate)
    full_gate=1
    shift
    if [ "${1:-}" = "--record" ]; then
      record_full_gate=1
      shift
    fi
    [ "$#" -eq 0 ] || usage
    ;;
  --group)
    requested_group="${2:-}"
    if [ -z "$requested_group" ]; then
      usage
    fi
    valid=0
    for group in "${groups[@]}"; do
      [ "$requested_group" = "$group" ] && valid=1
    done
    if [ "$valid" -ne 1 ]; then
      echo "unknown check group: $requested_group" >&2
      exit 2
    fi
    ;;
  --list-groups)
    printf '%s\n' "${groups[@]}"
    exit 0
    ;;
  *)
    usage
    ;;
esac

# A scheduled run needs a file to read afterwards, and a run that takes hours
# needs one even when a terminal goes away. Re-enter once with output teed; the
# marker variable is what stops that from recursing.
if [ "$full_gate" -eq 1 ] && [ -z "${AWAKEN_FULL_GATE_LOG:-}" ]; then
  full_gate_log_directory="${AWAKEN_FULL_GATE_LOG_DIR:-target/test-timings}"
  mkdir -p "$full_gate_log_directory"
  full_gate_log="$full_gate_log_directory/full-gate-$(date -u +%Y%m%dT%H%M%SZ).log"
  echo "full gate log: $full_gate_log"
  AWAKEN_FULL_GATE_LOG="$full_gate_log" "$0" "${original_arguments[@]}" 2>&1 | tee "$full_gate_log"
  exit "${PIPESTATUS[0]}"
fi

failed=()   # labels of checks that failed
gate_candidate_revision="$(git rev-parse --verify HEAD)" || exit 1
repro=()    # standalone reproduce command, parallel to `failed`
timing_file="${AWAKEN_TEST_TIMINGS_FILE:-target/test-timings/check-all-${requested_group}.tsv}"
initialize_timing_file() {
  mkdir -p "$(dirname "$timing_file")"
  if [ ! -e "$timing_file" ]; then
    printf 'group\tcheck\tstatus\tduration_seconds\n' > "$timing_file"
  fi
}
rm -f "$timing_file"
initialize_timing_file

# Groups whose external tools have already been decided this run.
declare -A prechecked=()

# A group's first check decides that group's environment first. Every external
# tool a group needs is declared in scripts/ci/gate_tools.json, and an absent one
# is reported as an absent tool here — never later, inside the check, wearing the
# name of whatever its absence happened to break.
precheck_group() {
  local group="$1"
  [ -z "${prechecked[$group]:-}" ] || return 0
  prechecked[$group]=1
  echo "-> $group tool precheck"
  if ! python3 scripts/ci/gate_tools.py require --gate "$group"; then
    failed+=("$group tool precheck")
    repro+=("python3 scripts/ci/gate_tools.py require --gate $group")
  fi
}

# run "<group>" "<label>" <command...> — runs the check; on failure records its label and
# a reproduce command. Never aborts early, so one run surfaces every failure.
run() {
  local group="$1" label="$2" started duration status; shift 2
  if [ "$requested_group" != all ] && [ "$requested_group" != "$group" ]; then
    return 0
  fi
  precheck_group "$group"
  echo "-> $label"
  started=$SECONDS
  if ! "$@"; then
    status=failed
    failed+=("$label")
    repro+=("$*")
  else
    status=passed
  fi
  duration=$((SECONDS - started))
  # A substrate runner may rotate its Cargo target directory. Timing telemetry
  # must never turn a completed release gate into an orchestration failure.
  initialize_timing_file
  printf '%s\t%s\t%s\t%s\n' "$group" "$label" "$status" "$duration" >> "$timing_file"
}

run static "repository-hygiene self-test" python3 scripts/ci/check_repository_hygiene.py --self-test
run static "repository-hygiene" python3 scripts/ci/check_repository_hygiene.py
run static "dependency-sources self-test" python3 scripts/ci/check_dependency_sources.py --self-test
run static "authority-arithmetic self-test" python3 scripts/ci/check_authority_arithmetic.py --self-test
run static "hook-glob generator self-test" python3 scripts/ci/generate_hooks.py --self-test
run static "hook-glob generation" python3 scripts/ci/generate_hooks.py --check
run static "test-orchestration self-test" python3 scripts/ci/check_test_orchestration.py --self-test
run static "test-orchestration" python3 scripts/ci/check_test_orchestration.py
run static "formal-product-ledger" python3 scripts/ci/check_feature_coverage.py --require-complete
run static "managed-wire-naming self-test" python3 scripts/ci/check_managed_wire_naming.py --self-test
run static "managed-wire-naming" python3 scripts/ci/check_managed_wire_naming.py
# A checker that accepts an argument it does not implement runs something other
# than what this file asked for, and still exits zero — which is how the
# degrading-defaults self-test below was invoked as `--selftest` for years while
# that script's flag was `--self-test`, so it never once ran. The rule is over
# the whole set of gate entry points, so a checker added tomorrow inherits it.
run static "gate-argument hygiene self-test" python3 scripts/ci/check_gate_arguments.py --self-test
# An executable no lane runs reads as coverage and is cited as evidence while
# never executing. Every one is selected by a lane or registered with a reason.
run static "lane-reachability self-test" python3 scripts/ci/check_lane_reachability.py --self-test
run static "lane reachability" python3 scripts/ci/check_lane_reachability.py
run static "gate-argument hygiene" python3 scripts/ci/check_gate_arguments.py
run static "dependency-gates self-test" python3 scripts/ci/check_dependency_gates.py --self-test
run static "dependency-gates" python3 scripts/ci/check_dependency_gates.py
run static "degrading-defaults self-test" python3 scripts/ci/check_degrading_defaults.py --self-test
run static "degrading-defaults" python3 scripts/ci/check_degrading_defaults.py
run static "fabricated-facts self-test" python3 scripts/ci/check_fabricated_facts.py --self-test
run static "fabricated-facts" python3 scripts/ci/check_fabricated_facts.py
run static "proof-input-derivation self-test" python3 scripts/ci/check_proof_input_derivation.py --selftest
run static "contract-shape self-test" python3 scripts/ci/check_contract_shape.py --self-test
run static "contract-shape" python3 scripts/ci/check_contract_shape.py
run static "coverage checker tests" python3 scripts/ci/test_changed_e2e_line_coverage.py
run static "LCOV export tests" python3 scripts/ci/test_export_homogeneous_lcov.py
run static "provider-environment self-test" scripts/ci/_provider_environment.sh --self-test
# The runner below imports e2e/node_modules. Report an uninstalled root as an
# uninstalled root: its absence otherwise surfaces as ERR_MODULE_NOT_FOUND,
# which reads as a broken import in this repository.
run static "node dependencies" python3 scripts/ci/check_node_dependencies.py
run static "e2e runner unit" npm --prefix e2e run test:runner
run static "command deadline tests" python3 scripts/ci/test_deadline.py
run static "postgres harness self-test" scripts/ci/pg_tests.sh --self-test
run static "k3d harness self-test" e2e/k3d/harness.sh --self-test
run static "distributed role rollout self-test" e2e/k3d/distributed_control_e2e.sh --self-test
run static "NATS wake runner self-test" e2e/k3d/nats_wake_e2e.sh --self-test
run static "k3d-product-image-contract" python3 deploy/k3d/test_image_contract.py
# The product backend topology contract renders and asserts the shipped overlay
# without a cluster. It sat executable and unrun; a topology assertion nothing
# executes is indistinguishable from one that passes.
run static "k3d product topology contract" e2e/k3d/product_backend_contract_test.sh
run static "standard local storage deployment contract" python3 deploy/storage/local-pv/test_contract.py
run static "secrets self-test" python3 scripts/ci/check_secrets.py --self-test
run static "secrets" python3 scripts/ci/check_secrets.py
run static "file-limits self-test" python3 scripts/ci/check_file_limits.py --self-test
run static "file-limits" python3 scripts/ci/check_file_limits.py
run static "test-wait self-test" python3 scripts/ci/check_test_waits.py --self-test
run static "test waits name a durable fact" python3 scripts/ci/check_test_waits.py
run static "scan-scope self-test" python3 scripts/ci/check_scan_scope.py --self-test
run static "a guard's scope is derived, not hand-written" python3 scripts/ci/check_scan_scope.py
run static "commit-message self-test" python3 scripts/ci/check_commit_message.py --self-test
run static "release-package self-test" python3 scripts/release/package.py --self-test
run static "product release checker self-test" python3 scripts/ci/check_product_release.py --self-test
run static "product release contract" python3 scripts/ci/check_product_release.py
run static "observation-collapse self-test" python3 scripts/ci/check_observation_collapse.py --self-test
run static "observation-collapse" python3 scripts/ci/check_observation_collapse.py
run static "reliability checker self-test" python3 scripts/ci/check_reliability.py --self-test
run static "reliability evidence contract" python3 scripts/ci/check_reliability.py
run static "Cargo target isolation self-test" scripts/ci/_cargo_target.sh --self-test
run static "public API gate tests" python3 scripts/ci/test_public_api.py
run static "Rust gate control-flow self-test" scripts/ci/check-rust.sh --self-test
# The bootstrap owns the qualified Kani package. Its publication rules run
# here as well as in the formal group, because a machine that cannot run the
# formal group is exactly the one those rules exist to keep working.
run static "Kani bootstrap publication rules" python3 scripts/ci/test_bootstrap_kani.py
# The tool manifest is the one declaration of what every group needs from this
# machine. Its self-test proves each row still locates by the method it states,
# so a missing tool is reported as a missing tool and never as a proof failure,
# a broken import, or a container pull error.
run static "gate tool manifest self-test" python3 scripts/ci/gate_tools.py --self-test
# Disk is a prerequisite like any other, and it filled twice in one session. The
# reclaimer decides what is derived build output and what is a checkout; that
# decision is what must stay proved, because it runs with --apply.
run static "build-space reclamation self-test" python3 scripts/ci/reclaim_build_space.py --self-test
# The full gate takes hours, so it is rarely run end to end; main once carried
# four unrelated defects behind a gate that had never completed, each hiding the
# next. This reports how far main has walked from the last recorded green full
# gate. It warns here and fails only under --require-fresh, so an ordinary run is
# never blocked by somebody else's schedule; docs/adr/0080 owns the policy.
run static "full-gate freshness self-test" python3 scripts/ci/check_gate_freshness.py --self-test
run static "full-gate freshness" python3 scripts/ci/check_gate_freshness.py
run static "prerequisite doctor self-test" scripts/ci/doctor.sh --self-test
run static "sandbox image build deadline self-test" deploy/images/sandbox/build.sh --self-test
run static "sandbox image provenance self-test" python3 scripts/release/awaken_sandbox_image_provenance.py self-test
run static "sandbox image release checker self-test" python3 scripts/ci/check_sandbox_image_release.py --self-test
run static "sandbox image release contract" python3 scripts/ci/check_sandbox_image_release.py
run docs "documentation" scripts/ci/check-docs.sh
run rust "rust" scripts/ci/check-rust.sh --full
run rust "dependency-policy" cargo deny --log-level error check advisories bans
run api "public-api" scripts/ci/check_public_api.sh --require-tools
# Generated wire artifacts (JSON Schema, OpenAPI, TS types, the Managed SDK
# oracle) are derived from Rust DTOs and the pinned official SDK. They drift
# silently unless a gate regenerates and compares them.
run contracts "generated-contracts" scripts/contract/generate-contracts.sh --check
run contracts "managed-sdk-oracle" pnpm --filter @awaken/managed-sdk-oracle check
# A harness can be green and constrain nothing. This walks every #[kani::proof]
# in the tree and refuses a proof-only fork inside a production entrypoint, a
# harness that restates its callee's body, and a free symbol standing in for a
# fact production computes -- against the registry's justified/backlog ratchet.
run formal "proof-input-derivation" python3 scripts/ci/check_proof_input_derivation.py --require-complete
run formal "formal" scripts/ci/check_formal.sh --require-tools
run reliability "reliability" scripts/ci/check-reliability.sh --require-tools
# Release completeness is strict: unavailable infrastructure is a failed gate,
# never a successful skip. Developer-specific partial suites remain runnable by
# invoking their scripts without the required flags.
run postgres "postgres" scripts/ci/pg_tests.sh --require-docker
run kubernetes "kubernetes-container" scripts/e2e/k8s_container_e2e.sh
run k3d "distributed-k3d" env AWAKEN_K3D_REQUIRED=1 e2e/k3d/distributed_control_e2e.sh
run k3d "generic preallocated local PVCs" e2e/k3d/local_storage_e2e.sh
run k3d "nats-wake-k3d" env AWAKEN_K3D_REQUIRED=1 e2e/k3d/nats_wake_e2e.sh 12
run frontend "frontend" scripts/ci/check-frontend.sh --full
run frontend "browser-e2e" pnpm -C web test:e2e
run e2e "deterministic-e2e" scripts/ci/e2e-coverage.sh
run e2e "latest-managed-sdk" npm --prefix e2e run test:sdk-latest-canary
run sandbox "sandbox-capabilities" scripts/e2e/sandbox_capability_suite.sh --require-substrates

if [ "${#failed[@]}" -ne 0 ]; then
  {
    echo ""
    echo "❌ ${#failed[@]} check(s) failed:"
    for i in "${!failed[@]}"; do
      echo "   • ${failed[$i]}"
      echo "       reproduce: ${repro[$i]}"
    done
  } >&2
  exit 1
fi
echo "✅ repository check group '$requested_group' passed"
echo "   timings: $timing_file"
if [ "$full_gate" -eq 1 ]; then
  echo ""
  echo "full gate summary:"
  if command -v column >/dev/null 2>&1; then
    column -t -s "$(printf '\t')" "$timing_file"
  else
    cat "$timing_file"
  fi
  if [ -n "${AWAKEN_FULL_GATE_LOG:-}" ]; then
    echo "   log: $AWAKEN_FULL_GATE_LOG"
  fi
fi
# Reached only when every group above passed, so this is the one place a record
# can be written and the only run that is entitled to write one.
if [ "$record_full_gate" -eq 1 ]; then
  python3 scripts/ci/check_gate_freshness.py --record --gates "${groups[*]}" \
    --expected-commit "$gate_candidate_revision" || exit 1
fi
