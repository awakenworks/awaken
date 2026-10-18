"""The realization classifies each failure once.

Plan v4 D3 and principles P6/P7. `awaken-session-realization` reports every
failure as a `RealizationError`, whose `disposition()` is the one decision of
what a caller does next. Two regressions would silently undo that, and the
compiler cannot see either (a poisoned lock read through `.expect` is refused
by `_poisoned_lock_fitness.py`, which covers the realization and the executor):

* `HostError` reappearing in the realization. The executor's error is not the
  realization's vocabulary; mapping it back in would flatten every class into
  the executor's kinds again.
* A typed failure flattened into the text of a stated variant
  (`invariant(error.to_string())`, `invalid_request(error.to_string())`, a
  `format!` interpolating the error, or `HostError::internal(..)`). The typed
  source keeps its class; only closed codes classify.

Only production code is scanned: comments, `cfg(test)` code, test files and the
test-support fixtures are excluded.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

import _crate_boundary_workspace

REALIZATION_SRC = "crates/server/awaken-session-realization/src"
# The realization crate's sources are this check's whole input.
INPUT_GLOBS: tuple[str, ...] = ("crates/server/awaken-session-realization/src/**",)
TEST_FILES = re.compile(r"(^|/)(tests?|test_support)(\.rs|/)")

HOST_ERROR = re.compile(r"\bHostError\b")
STATED = r"(?:\bRealizationError\s*::\s*)?\b(?:invariant|invalid_request|internal)\s*\("
FLATTENED_TEXT = re.compile(STATED + r"\s*[\w.]*\.to_string\s*\(\s*\)")
FLATTENED_FORMAT = re.compile(STATED + r"\s*format!\s*\(\s*\"[^\"]*\{(?:error|err|e|source)\}")


def realization_error_violations(sources: dict[str, str]) -> list[str]:
    """Rule violations in the realization's production sources.

    Cause/effect graph: C1 the source is production realization code; C2 it
    names `HostError`; C3 it flattens a typed failure into a stated message.
    E1 accept when C1 is false or neither C2 nor C3 holds; E2 reject and name
    the file and rule.
    """
    errors: list[str] = []
    for relative, source in sorted(sources.items()):
        if not relative.startswith(REALIZATION_SRC + "/") or not relative.endswith(".rs"):
            continue
        if TEST_FILES.search(relative[len(REALIZATION_SRC) + 1 :]):
            continue
        production = _crate_boundary_workspace.production_rust(source)
        text = _crate_boundary_workspace.rust_without_comments(production)
        syntax = _crate_boundary_workspace.rust_syntax(production)
        if HOST_ERROR.search(syntax):
            errors.append(
                f"{relative}: the realization returns RealizationError; HostError belongs to the executor"
            )
        if FLATTENED_TEXT.search(syntax) or FLATTENED_FORMAT.search(text):
            errors.append(
                f"{relative}: keep the typed failure in its RealizationError variant; "
                "do not flatten it into a stated message"
            )
    return errors


def selftest() -> None:
    """Decision table for the realization error rules.

    | Rule | Source                                                  | Effect |
    |---|---|---|
    | E1 | typed variants, `?` into RealizationError               | accept |
    | E2 | the same text in a cfg(test) module or a test file      | accept |
    | E3 | the same text outside the realization crate             | accept |
    | E4 | `HostError` named in production realization code        | reject |
    | E5 | `invariant(error.to_string())`                          | reject |
    | E6 | `invalid_request(format!("bad: {error}"))`              | reject |
    | E7 | the forbidden text only inside a comment or a string    | accept |
    """
    path = f"{REALIZATION_SRC}/environment_lifecycle.rs"
    clean = "fn f() -> Result<(), RealizationError> { g().map_err(RealizationError::from)?; Ok(()) }\n"
    assert realization_error_violations({path: clean}) == [], "E1"
    forbidden = "fn f() { let _ = m.lock().expect(\"x\"); HostError::internal(e.to_string()); }\n"
    assert realization_error_violations(
        {path: clean + "#[cfg(test)]\nmod tests {\n" + forbidden + "}\n"}
    ) == [], "E2 cfg(test)"
    assert realization_error_violations({f"{REALIZATION_SRC}/session_realizations/tests.rs": forbidden}) == [], "E2 file"
    assert realization_error_violations({f"{REALIZATION_SRC}/test_support.rs": forbidden}) == [], "E2 support"
    assert realization_error_violations(
        {"crates/server/awaken-session-executor/src/host_error.rs": forbidden}
    ) == [], "E3"
    rejected = {
        "E4": "fn f() -> Result<(), HostError> { Ok(()) }\n",
        "E5": "fn f() { g().map_err(|error| RealizationError::invariant(error.to_string())); }\n",
        "E6": "fn f() { g().map_err(|error| RealizationError::invalid_request(format!(\"bad: {error}\"))); }\n",
    }
    for rule, source in rejected.items():
        assert len(realization_error_violations({path: source})) == 1, rule
    commented = (
        "// HostError::internal(error.to_string()) and m.lock().expect(\"x\")\n"
        "fn f() { let _ = \"HostError .lock().expect(\"; }\n"
    )
    assert realization_error_violations({path: commented}) == [], "E7"


def check_all(repo_root: Path) -> list[str]:
    root = repo_root / REALIZATION_SRC
    sources = {
        path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
        for path in root.rglob("*.rs")
    }
    return realization_error_violations(sources)


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        print("usage: python3 scripts/ci/_realization_error_fitness.py --self-test", file=sys.stderr)
        raise SystemExit(2)
    selftest()
    print("_realization_error_fitness selftest: the decision table holds")
