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

groups=(static docs rust api formal postgres kubernetes k3d frontend e2e sandbox)
requested_group=all
case "${1:-}" in
  "") ;;
  --group)
    requested_group="${2:-}"
    if [ -z "$requested_group" ]; then
      echo "usage: scripts/ci/check-all.sh [--group <name>|--list-groups]" >&2
      exit 2
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
    echo "usage: scripts/ci/check-all.sh [--group <name>|--list-groups]" >&2
    exit 2
    ;;
esac

failed=()   # labels of checks that failed
repro=()    # standalone reproduce command, parallel to `failed`
timing_file="${AWAKEN_TEST_TIMINGS_FILE:-target/test-timings/check-all-${requested_group}.tsv}"
mkdir -p "$(dirname "$timing_file")"
printf 'group\tcheck\tstatus\tduration_seconds\n' > "$timing_file"

# run "<group>" "<label>" <command...> — runs the check; on failure records its label and
# a reproduce command. Never aborts early, so one run surfaces every failure.
run() {
  local group="$1" label="$2" started duration status; shift 2
  if [ "$requested_group" != all ] && [ "$requested_group" != "$group" ]; then
    return 0
  fi
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
  printf '%s\t%s\t%s\t%s\n' "$group" "$label" "$status" "$duration" >> "$timing_file"
}

run static "repository-hygiene self-test" python3 scripts/ci/check_repository_hygiene.py --self-test
run static "repository-hygiene" python3 scripts/ci/check_repository_hygiene.py
run static "test-orchestration self-test" python3 scripts/ci/check_test_orchestration.py --self-test
run static "test-orchestration" python3 scripts/ci/check_test_orchestration.py
run static "e2e runner unit" npm --prefix e2e run test:runner
run static "postgres harness self-test" scripts/ci/pg_tests.sh --self-test
run static "k3d-product-image-contract" python3 deploy/k3d/test_image_contract.py
run static "secrets self-test" python3 scripts/ci/check_secrets.py --self-test
run static "secrets" python3 scripts/ci/check_secrets.py
run static "file-limits self-test" python3 scripts/ci/check_file_limits.py --self-test
run static "file-limits" python3 scripts/ci/check_file_limits.py
run static "commit-message self-test" python3 scripts/ci/check_commit_message.py --self-test
run static "Cargo target isolation self-test" scripts/ci/_cargo_target.sh --self-test
run static "sandbox image build deadline self-test" deploy/images/sandbox/build.sh --self-test
run docs "documentation" scripts/ci/check-docs.sh
run rust "rust" scripts/ci/check-rust.sh --full
run rust "dependency-policy" cargo deny --log-level error check bans
run api "public-api" scripts/ci/check_public_api.sh --require-tools
run formal "formal" scripts/ci/check_formal.sh --require-tools
# Release completeness is strict: unavailable infrastructure is a failed gate,
# never a successful skip. Developer-specific partial suites remain runnable by
# invoking their scripts without the required flags.
run postgres "postgres" scripts/ci/pg_tests.sh --require-docker
run kubernetes "kubernetes-container" scripts/e2e/k8s_container_e2e.sh
run k3d "distributed-k3d" env AWAKEN_K3D_REQUIRED=1 e2e/k3d/distributed_control_e2e.sh
run k3d "nats-wake-k3d" env AWAKEN_K3D_REQUIRED=1 e2e/k3d/nats_wake_e2e.sh 12
run frontend "frontend" scripts/ci/check-frontend.sh --full
run e2e "deterministic-e2e" npm --prefix e2e run test:deterministic
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
