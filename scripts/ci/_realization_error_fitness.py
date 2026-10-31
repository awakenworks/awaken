"""Each execution boundary projects its failures once, at its error owner.

Plan v4 D3 and principles P6/P7. `awaken-session-realization` reports every
failure as a `RealizationError`, whose `disposition()` is the one decision of
what a caller does next. Two regressions would silently undo that, and the
compiler cannot see either (a poisoned lock read through `.expect` is refused
by clippy.toml and `_poisoned_lock_fitness.py` across all production code).
The executor's `SessionExecutionError` cannot appear in the realization at all:
the executor depends on the realization, so Cargo refuses the reverse edge.

* A typed failure flattened into the text of a stated variant
  (`invariant(error.to_string())`, `invalid_request(error.to_string())`, a
  `format!` interpolating the error, or `HostError::internal(..)`). The typed
  source keeps its class; only closed codes classify.
* A Session executor caller inspecting `SessionExecutionErrorKind` instead of
  using the error owner's projection. The removed RunApplication mapper lost
  classified Internal codes even though its result was already RunError.

Only production code is scanned: comments, `cfg(test)` code, test files and the
test-support fixtures are excluded.
"""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace

AUTHORITIES = frozenset({"session-executor", "session-realization"})


def scanned_manifests(repo_root: Path) -> list[Path]:
    """Discover the two error owners from their declared Cargo authority.

    A crate move must not remove its code from the rule. Source paths and the
    production module closure are derived from the same manifests Cargo owns.
    """
    return [
        path
        for path in _crate_boundary_workspace.crate_manifests(repo_root / "crates")
        if _crate_boundary_workspace.load_manifest(path)
        .get("package", {})
        .get("metadata", {})
        .get("awaken", {})
        .get("authority")
        in AUTHORITIES
    ]

REALIZATION_SRC = "crates/server/awaken-session-realization/src"
# check_crate_boundaries.py owns this helper's execution and hook inputs.
# Compiled source discovery follows Cargo authority and module reachability.
TEST_FILES = re.compile(r"(^|/)(tests?|test_support)(\.rs|/)")

STATED = r"(?:\bRealizationError\s*::\s*)?\b(?:invariant|invalid_request|internal)\s*\("
FLATTENED_TEXT = re.compile(STATED + r"\s*[\w.]*\.to_string\s*\(\s*\)")
FLATTENED_FORMAT = re.compile(STATED + r"\s*format!\s*\(\s*\"[^\"]*\{(?:error|err|e|source)\}")
EXECUTOR_OWNER = re.compile(r"\bpub\s+struct\s+SessionExecutionError\b")
EXECUTOR_CLASSIFICATION = re.compile(r"\bSessionExecutionErrorKind\s*::")


def realization_error_violations(sources: dict[str, str]) -> list[str]:
    """Rule violations in the realization's production sources.

    Cause/effect graph: C1 the source is production realization code; C3 it
    flattens a typed failure into a stated message. E1 accept when C1 is false
    or C3 does not hold; E2 reject and name the file and rule.
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
        if FLATTENED_TEXT.search(syntax) or FLATTENED_FORMAT.search(text):
            errors.append(
                f"{relative}: keep the typed failure in its RealizationError variant; "
                "do not flatten it into a stated message"
            )
    return errors


def executor_error_violations(sources: dict[str, str]) -> list[str]:
    """A caller forwards the executor error; only its owner projects its kind.

    Cause/effect graph: C1 exactly one production source defines the error;
    C2 another production source inspects a kind variant. Accept C1 and !C2;
    otherwise refuse a missing/ambiguous owner or the parallel classifier.
    """
    syntax = {
        relative: _crate_boundary_workspace.rust_syntax(
            _crate_boundary_workspace.production_rust(source)
        )
        for relative, source in sources.items()
        if not TEST_FILES.search(relative)
    }
    owners = [relative for relative, source in syntax.items() if EXECUTOR_OWNER.search(source)]
    if len(owners) != 1:
        return ["execution-error: exactly one production source must own SessionExecutionError"]
    return [
        f"{relative}: project SessionExecutionError through its owner {owners[0]}; "
        "a caller must not reclassify its kind or discard its code"
        for relative, source in syntax.items()
        if relative != owners[0] and EXECUTOR_CLASSIFICATION.search(source)
    ]


def selftest() -> None:
    """Decision table for the realization error rules.

    | Rule | Source                                                  | Effect |
    |---|---|---|
    | E1 | typed variants, `?` into RealizationError               | accept |
    | E2 | the same text in a cfg(test) module or a test file      | accept |
    | E3 | the same text outside the realization crate             | accept |
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

    # Executor table: X1 one owner + constructor-only caller -> accept;
    # X2 parallel kind projection -> reject; X3 absent/ambiguous owner ->
    # reject; X4 variant inspection in tests/comments/strings -> accept.
    # The owner is discovered from its declaration, so its path may move.
    owner = "moved/error.rs"
    declaration = (
        "pub struct SessionExecutionError {}\n"
        "fn project() { SessionExecutionErrorKind::Internal; }\n"
    )
    caller = "fn call() { SessionExecutionError::internal(message); }\n"
    assert executor_error_violations({owner: declaration, "caller.rs": caller}) == [], "X1"
    classifier = "fn flatten() { SessionExecutionErrorKind::Internal; }\n"
    assert len(executor_error_violations({owner: declaration, "caller.rs": classifier})) == 1, "X2"
    assert len(executor_error_violations({"caller.rs": caller})) == 1, "X3 missing"
    assert len(executor_error_violations({owner: declaration, "second.rs": declaration})) == 1, "X3 ambiguous"
    assert executor_error_violations({
        owner: declaration,
        "tests.rs": classifier,
        "caller.rs": caller + "#[cfg(test)]\nmod tests { " + classifier + "}\n"
        + '// SessionExecutionErrorKind::Internal\nfn prose() { "SessionExecutionErrorKind::Internal"; }\n',
    }) == [], "X4"


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    # The Cargo-authority/module discovery; there is no second hand-maintained
    # list of compiled files.
    for manifest in scanned_manifests(repo_root):
        sources = {
            path.relative_to(repo_root.resolve()).as_posix(): path.read_text(encoding="utf-8")
            for path in _crate_boundary_workspace.production_module_files([manifest])
        }
        authority = _crate_boundary_workspace.load_manifest(manifest)["package"]["metadata"]["awaken"]["authority"]
        errors.extend(
            executor_error_violations(sources)
            if authority == "session-executor"
            else realization_error_violations(sources)
        )
    return errors
