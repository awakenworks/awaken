#!/usr/bin/env bash
# Run every documentation guardrail over the whole corpus.
#
# Usage: scripts/ci/check-docs.sh
#
# This is the git-independent entry point: it scans all of docs/ directly, so it
# works before `git init` / `lefthook install`. lefthook.yml wires the same
# scripts to pre-commit on staged files once the repo is under git.
set -euo pipefail
cd "$(dirname "$0")/../.."

docs=$(find docs -name '*.md' | sort)

fail=0
run() { echo "→ $1"; if ! "${@:2}"; then fail=1; fi; }

run "check-doc-links"             python3 scripts/ci/check_doc_links.py $docs
run "check-wiki-okf"              python3 scripts/ci/check_wiki_okf.py $docs
run "check-wiki-no-invariant-copy" python3 scripts/ci/check_wiki_no_invariant_copy.py $docs
run "check-invariants"            python3 scripts/ci/check_invariants.py
run "check-adr"                   python3 scripts/ci/check_adr.py $docs
run "check-ownership-index"       python3 scripts/ci/check_ownership_index.py
run "check-role-catalogs"         python3 scripts/ci/check_role_catalogs.py

if [ "$fail" -ne 0 ]; then
  echo "✗ documentation checks failed" >&2
  exit 1
fi
echo "✓ all documentation checks passed"
