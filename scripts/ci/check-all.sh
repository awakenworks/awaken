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
run "secrets self-test" python3 scripts/ci/check_secrets.py --self-test
run "secrets" python3 scripts/ci/check_secrets.py
run "file-limits self-test" python3 scripts/ci/check_file_limits.py --self-test
run "file-limits" python3 scripts/ci/check_file_limits.py
run "commit-message self-test" python3 scripts/ci/check_commit_message.py --self-test
run "documentation" scripts/ci/check-docs.sh
run "rust" scripts/ci/check-rust.sh --full
if [ "${AWAKEN_SKIP_FORMAL:-0}" = "1" ]; then
  echo "-> formal (explicitly skipped with AWAKEN_SKIP_FORMAL=1)"
else
  run "formal" scripts/ci/check_formal.sh --require-tools
fi
# Postgres-backed suites against a throwaway database. Docker-gated: SKIPS (passes)
# where docker is unavailable — so `cargo test --workspace` above still covers the
# no-DB path, and a docker-equipped CI additionally runs the ~half of
# distributed-correctness tests that only exercise real behaviour on Postgres (and
# otherwise self-skip into a false green). See scripts/ci/pg_tests.sh.
run "postgres" scripts/ci/pg_tests.sh
run "frontend" scripts/ci/check-frontend.sh --full

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
