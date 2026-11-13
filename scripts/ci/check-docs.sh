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

# This gate takes no arguments. Accepting one silently is how a caller's
# intent ("only the links", "--quick") disappears into a full run that
# answers a different question; refuse before any work.
if [ "$#" -ne 0 ]; then
  echo "usage: scripts/ci/check-docs.sh" >&2
  exit 2
fi

docs=$(find docs -name '*.md' | sort)

fail=0
run() { echo "→ $1"; if ! "${@:2}"; then fail=1; fi; }

# The link checker owns the full input set, including product/SDK READMEs.
run "check-doc-links self-test"   python3 scripts/ci/check_doc_links.py --self-test
run "check-doc-links"             python3 scripts/ci/check_doc_links.py
run "example projection"          node scripts/generate-example-docs.mjs --check
run "check-wiki-okf self-test"    python3 scripts/ci/check_wiki_okf.py --self-test
run "check-wiki-okf"              python3 scripts/ci/check_wiki_okf.py $docs
run "check-wiki-no-invariant-copy" python3 scripts/ci/check_wiki_no_invariant_copy.py $docs
run "check-invariants self-test"  python3 scripts/ci/check_invariants.py --self-test
run "check-invariants"            python3 scripts/ci/check_invariants.py
run "check-adr"                   python3 scripts/ci/check_adr.py $docs
run "check-ownership-index"       python3 scripts/ci/check_ownership_index.py
run "check-role-catalogs"         python3 scripts/ci/check_role_catalogs.py
run "check-architecture-vocabulary-self-test" python3 scripts/ci/check_architecture_vocabulary.py --self-test
run "check-architecture-vocabulary" python3 scripts/ci/check_architecture_vocabulary.py
# The Session acceptance ledger may only claim what bound assertions and
# receipts prove; its gate tests execute each refusal on real inputs.
run "check-session-acceptance self-test" python3 scripts/ci/check_session_acceptance.py --self-test
run "check-session-acceptance"    python3 scripts/ci/check_session_acceptance.py
run "session-acceptance gate tests" python3 scripts/ci/test_session_acceptance.py

if [ "$fail" -ne 0 ]; then
  echo "✗ documentation checks failed" >&2
  exit 1
fi
echo "✓ all documentation checks passed"
