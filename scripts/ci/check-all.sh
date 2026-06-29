#!/usr/bin/env bash
# Run every repository guardrail.
set -euo pipefail
cd "$(dirname "$0")/../.."

fail=0
run() {
  echo "-> $1"
  if ! "${@:2}"; then
    fail=1
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
run "frontend" scripts/ci/check-frontend.sh --full

if [ "$fail" -ne 0 ]; then
  echo "repository checks failed" >&2
  exit 1
fi
echo "all repository checks passed"
