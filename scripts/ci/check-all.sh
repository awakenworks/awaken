#!/usr/bin/env bash
# Run every repository guardrail.
#
# On failure, ends with a summary that names each failed check and the exact
# command to reproduce it standalone, so the fix loop is: read summary → run
# one command → fix → retry.
set -uo pipefail
cd "$(dirname "$0")/../.."

failed=()   # labels of checks that failed
repro=()    # standalone reproduce command, parallel to `failed`

# run "<label>" <command...> — runs the check; on failure records its label and
# a reproduce command. Never aborts early, so one run surfaces every failure.
run() {
  local label="$1"; shift
  echo "-> $label"
  if ! "$@"; then
    failed+=("$label")
    repro+=("$*")
  fi
}

run "repository-hygiene self-test" python3 scripts/ci/check_repository_hygiene.py --self-test
run "repository-hygiene" python3 scripts/ci/check_repository_hygiene.py
run "test-orchestration self-test" python3 scripts/ci/check_test_orchestration.py --self-test
run "test-orchestration" python3 scripts/ci/check_test_orchestration.py
run "postgres harness self-test" scripts/ci/pg_tests.sh --self-test
run "k3d-product-image-contract" python3 deploy/k3d/test_image_contract.py
run "secrets self-test" python3 scripts/ci/check_secrets.py --self-test
run "secrets" python3 scripts/ci/check_secrets.py
run "file-limits self-test" python3 scripts/ci/check_file_limits.py --self-test
run "file-limits" python3 scripts/ci/check_file_limits.py
run "commit-message self-test" python3 scripts/ci/check_commit_message.py --self-test
run "Cargo target isolation self-test" scripts/ci/_cargo_target.sh --self-test
run "sandbox image build deadline self-test" deploy/images/sandbox/build.sh --self-test
run "documentation" scripts/ci/check-docs.sh
run "rust" scripts/ci/check-rust.sh --full
run "dependency-policy" cargo deny --log-level error check bans
run "public-api" scripts/ci/check_public_api.sh --require-tools
run "formal" scripts/ci/check_formal.sh --require-tools
# Release completeness is strict: unavailable infrastructure is a failed gate,
# never a successful skip. Developer-specific partial suites remain runnable by
# invoking their scripts without the required flags.
run "postgres" scripts/ci/pg_tests.sh --require-docker
run "kubernetes-container" scripts/e2e/k8s_container_e2e.sh
run "distributed-k3d" env AWAKEN_K3D_REQUIRED=1 e2e/k3d/distributed_control_e2e.sh
run "nats-wake-k3d" env AWAKEN_K3D_REQUIRED=1 e2e/k3d/nats_wake_e2e.sh 12
run "frontend" scripts/ci/check-frontend.sh --full
run "deterministic-e2e" npm --prefix e2e run test:deterministic
run "sandbox-capabilities" scripts/e2e/sandbox_capability_suite.sh --require-substrates

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
echo "✅ all repository checks passed"
