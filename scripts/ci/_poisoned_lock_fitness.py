"""A poisoned lock denies; the Session executor and the realization never panic on one.

Plan v4 principle P7 (concurrency you can reason about: poison denies, reads
never create). A lock whose holder panicked guards state that may be
half-written. The executor (`awaken-session-executor`) and the realization
(`awaken-session-realization`) answer it with a typed refusal --
`RealizationLock` for the installed-Session registry, `ExecutorLock` for the
executor's own process-local locks -- or, where a decision table in the code
shows the answer is the fail-closed one, with that answer. Two spellings would
silently undo that, and the compiler sees neither:

* `L1` a lock read through `.expect(..)`/`.unwrap()`
  (`.lock()`, `.read()`, `.write()`).
* `L2` a registry read whose poison error is unwrapped with an `.expect(..)`
  that names the poison (`.expect("... poisoned")`): the read already returns
  the typed refusal, and the caller must propagate it.

The count is held at zero: there is no baseline, every occurrence is refused.
Only production code is scanned: comments, `cfg(test)` code, test files and the
test-support fixtures are excluded.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

import _crate_boundary_workspace

TEST_FILES = re.compile(r"(^|/)(tests?|test_support|fixtures)(\.rs|/)|_tests?\.rs$")

LOCK_PANIC = re.compile(r"\.\s*(?:lock|read|write)\s*\(\s*\)\s*\.\s*(?:expect|unwrap)\s*\(")
POISON_EXPECT = re.compile(r"\.\s*expect\s*\(\s*\"[^\"]*poison[^\"]*\"", re.IGNORECASE)


def scanned_manifests(repo_root: Path) -> list[Path]:
    """Discover the two state owners from their declared Cargo authority.

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
        in {"session-executor", "session-realization"}
    ]


def _source_roots(repo_root: Path) -> tuple[str, ...]:
    return tuple(
        (path.parent / "src").relative_to(repo_root).as_posix()
        for path in scanned_manifests(repo_root)
    )


def _scanned_root(relative: str, roots: tuple[str, ...]) -> str | None:
    for root in roots:
        if relative.startswith(root + "/") and relative.endswith(".rs"):
            return root
    return None


def poisoned_lock_violations(
    sources: dict[str, str], roots: tuple[str, ...] | None = None
) -> list[str]:
    """Rule violations in the scanned production sources.

    Cause/effect graph: C1 the source is production executor or realization
    code; C2 it reads a lock through `.expect`/`.unwrap` (L1); C3 it unwraps a
    poison refusal with a poison-naming `.expect` (L2). E1 accept when C1 is
    false or neither C2 nor C3 holds; E2 reject and name the file, line and rule.
    """
    if roots is None:
        roots = _source_roots(_crate_boundary_workspace.REPO_ROOT)
    errors: list[str] = []
    for relative, source in sorted(sources.items()):
        root = _scanned_root(relative, roots)
        if root is None or TEST_FILES.search(relative[len(root) + 1 :]):
            continue
        production = _crate_boundary_workspace.production_rust(source)
        syntax = _crate_boundary_workspace.rust_syntax(production)
        for match in LOCK_PANIC.finditer(syntax):
            # The blanked projection turns `read("path")` into `read(      )`;
            # a call with an argument is not a lock acquisition.
            if '"' in production[match.start():match.end()]:
                continue
            line = syntax.count("\n", 0, match.start()) + 1
            errors.append(
                f"{relative}:{line}: a poisoned lock denies with a typed refusal "
                "(RealizationLock / ExecutorLock) instead of panicking (L1)"
            )
        for match in POISON_EXPECT.finditer(production):
            line = production.count("\n", 0, match.start()) + 1
            errors.append(
                f"{relative}:{line}: propagate the registry's typed poison refusal "
                "instead of expecting it away (L2)"
            )
    return errors


def selftest() -> None:
    """Decision table for the poisoned-lock rules.

    | Rule | Source                                                         | Effect |
    |---|---|---|
    | P1 | `.lock().map_err(..)?`, `.lock().ok()`, `if let Ok(g) = m.lock()` | accept |
    | P2 | a forbidden spelling in a cfg(test) module or a test file      | accept |
    | P3 | a forbidden spelling in a crate outside the scanned two        | accept |
    | P4 | `.lock().expect(..)` / `.lock().unwrap()` / `.write().unwrap()` | reject L1 |
    | P5 | `.frozen(t).expect("slots mutex poisoned")`                    | reject L2 |
    | P6 | the forbidden text only inside a comment                       | accept |
    | P7 | `.expect("..")` whose message does not name poison             | accept (not a lock read) |
    | P8 | tokio `.lock().await`                                          | accept (cannot poison) |
    | P9 | authority moved to a nested crate / unrelated authority        | still scanned / excluded |
    | P10 | `store.read("path").unwrap()`, a call with a string argument   | accept (not a lock read) |
    """
    roots = _source_roots(_crate_boundary_workspace.REPO_ROOT)
    executor_root = next(root for root in roots if "session-executor/" in root)
    realization_root = next(root for root in roots if "session-realization/" in root)
    executor = f"{executor_root}/thread_reads.rs"
    realization = f"{realization_root}/realization_state.rs"
    clean = (
        "fn f() -> Result<(), E> { let g = m.lock().map_err(|_| E::Poisoned)?; "
        "let h = n.lock().ok(); if let Ok(i) = o.lock() {} p.lock().await; Ok(()) }\n"
    )
    assert poisoned_lock_violations({executor: clean, realization: clean}) == [], "P1/P8"
    forbidden = "fn f() { let _ = m.lock().expect(\"x\"); r.frozen(t).expect(\"slots poisoned\"); }\n"
    assert poisoned_lock_violations(
        {executor: clean + "#[cfg(test)]\nmod tests {\n" + forbidden + "}\n"}
    ) == [], "P2 cfg(test)"
    for test_file in ("installed_session/tests.rs", "test_support.rs", "host/tests/fixtures.rs", "memory_tests.rs"):
        assert poisoned_lock_violations({f"{executor_root}/{test_file}": forbidden}) == [], test_file
    assert poisoned_lock_violations({"crates/server/awaken-session-mcp/src/mcp.rs": forbidden}) == [], "P3"
    rejected = {
        "P4a": "fn f() { let _ = self.slots.lock().expect(\"slots\"); }\n",
        "P4b": "fn f() { let _ = self.slots.lock().unwrap(); }\n",
        "P4c": "fn f() { let _ = self.sink.write().unwrap(); }\n",
        "P5": "fn f() { let _ = r.frozen(t).expect(\"session runtime slots mutex poisoned\"); }\n",
    }
    for rule, source in rejected.items():
        for path in (executor, realization):
            assert len(poisoned_lock_violations({path: source})) == 1, rule
    commented = "// m.lock().expect(\"x\") and .expect(\"poisoned\")\nfn f() {}\n"
    assert poisoned_lock_violations({executor: commented}) == [], "P6"
    assert poisoned_lock_violations(
        {executor: "fn f() { let _ = v.first().expect(\"non-empty by construction\"); }\n"}
    ) == [], "P7"
    assert poisoned_lock_violations(
        {executor: "fn f() { let _ = store.read(\"path\").unwrap(); w.write(r\"x\").expect(\"io\"); }\n"}
    ) == [], "P10"

    # P9: moved/nested crates with the same authority retain the rule; an
    # unrelated authority does not enter it. Cargo owns the manifest and the
    # module graph, so neither directory depth nor an unreferenced .rs file
    # can become a second scan-scope list.
    import tempfile

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        moved = root / "crates" / "nested" / "moved-executor"
        (moved / "src").mkdir(parents=True)
        (moved / "Cargo.toml").write_text(
            '[package]\nname = "moved-executor"\n'
            '[package.metadata.awaken]\nauthority = "session-executor"\n',
            encoding="utf-8",
        )
        (moved / "src" / "lib.rs").write_text(forbidden, encoding="utf-8")
        assert len(check_all(root)) == 2, "P9 moved authority remains scanned"
        (moved / "Cargo.toml").write_text(
            '[package]\nname = "moved-executor"\n'
            '[package.metadata.awaken]\nauthority = "other"\n',
            encoding="utf-8",
        )
        assert check_all(root) == [], "P9 unrelated authority remains outside"


def check_all(repo_root: Path) -> list[str]:
    """Scan the files a production build of either crate compiles: a module
    declared under `cfg(test)` (or reached only through one) is test code."""
    manifests = scanned_manifests(repo_root)
    sources = {
        path.relative_to(repo_root.resolve()).as_posix(): path.read_text(encoding="utf-8")
        for path in _crate_boundary_workspace.production_module_files(manifests)
    }
    return poisoned_lock_violations(sources, _source_roots(repo_root))


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        print("usage: python3 scripts/ci/_poisoned_lock_fitness.py --self-test", file=sys.stderr)
        raise SystemExit(2)
    selftest()
    print("_poisoned_lock_fitness selftest: the decision table holds")
