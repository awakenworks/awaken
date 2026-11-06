"""An invariant lock's poison refusal is propagated, never unwrapped away.

Plan v5 P7 / S5 and `docs/design/lock-poisoning.md`. The workspace
`clippy.toml` denies the `std::sync` lock methods in production code, so a
lock's poisoning is decided by its `awaken_sync` type: a `LeafLock` recovers
it, an `InvariantLock` refuses it with `LockPoisoned`, which the owning crate
converts once into its own typed error. Clippy cannot tell an `InvariantLock`
refusal unwrapped away from a legitimate `Result::expect`, so two spellings
stay here:

* `L1` an `InvariantLock`/`InvariantRwLock` acquisition read through
  `.expect(..)`/`.unwrap()` (`.lock()`, `.read()`, `.write()`). A leaf lock's
  acquisition returns the guard itself, so this spelling compiles only on an
  invariant lock (or a `std` lock, which clippy already refuses).
* `L2` an `.expect(..)` whose message names the poison (`"... poisoned"`): the
  refusal is already typed, and the caller propagates it.

Scope: every production module of every workspace crate (the module closure
Cargo compiles without `cfg(test)`, test-support and testkit code included),
except `awaken-sync`, the one crate that reads a `std` lock. A Clippy
annotation may permit a foreign API's required `std` representation, never
an unwrapped poison refusal. A direct site matching both rules is reported
once as L1; independent or indirect poison expects remain L2. The count is
held at zero: there is no baseline.

Retires when an invariant refusal can be unwrapped only through a typed route
(for example a `LockPoisoned` that is not `Debug`), so the compiler refuses
`.expect(..)` on it.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

import _crate_boundary_workspace

SYNC_CRATE = "awaken-sync"
LOCK_PANIC = re.compile(r"\.\s*(?:lock|read|write)\s*\(\s*\)\s*\.\s*(?:expect|unwrap)\s*\(")
POISON_EXPECT = re.compile(r"\.\s*expect\s*\(\s*\"[^\"]*poison[^\"]*\"", re.IGNORECASE)


def scanned_manifests(repo_root: Path) -> list[Path]:
    """Every workspace crate except the sync crate, discovered from Cargo."""
    return [
        path
        for path in _crate_boundary_workspace.crate_manifests(repo_root / "crates")
        if _crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(path))
        != SYNC_CRATE
    ]


def poisoned_lock_violations(sources: dict[str, str]) -> list[str]:
    """Rule violations in production sources, keyed by repository path.

    Cause/effect graph: C1 the source is production code outside the sync
    crate; C2 it unwraps a lock acquisition (L1); C3 it unwraps with a
    poison-naming `.expect` (L2); C4 that expect starts within the same reported
    L1 match. E1 accept when C1 is false or neither C2 nor C3 holds; E2 reject
    each C2 once as L1 and each C3 outside C4 as L2. An annotation does not
    change either rule. The original projections preserve these offsets.
    """
    errors: list[str] = []
    for relative, source in sorted(sources.items()):
        production = _crate_boundary_workspace.production_rust(source)
        syntax = _crate_boundary_workspace.rust_syntax(production)
        direct_ranges: list[tuple[int, int]] = []

        for match in LOCK_PANIC.finditer(syntax):
            # The blanked projection turns `read("path")` into `read(      )`;
            # a call with an argument is not a lock acquisition.
            if '"' in production[match.start():match.end()]:
                continue
            direct_ranges.append(match.span())
            line = syntax.count("\n", 0, match.start()) + 1
            errors.append(
                f"{relative}:{line}: an InvariantLock refuses poison with LockPoisoned; "
                "propagate it through the crate's typed error instead of unwrapping it (L1)"
            )
        for match in POISON_EXPECT.finditer(production):
            if any(start <= match.start() < end for start, end in direct_ranges):
                continue
            line = production.count("\n", 0, match.start()) + 1
            errors.append(
                f"{relative}:{line}: propagate the typed poison refusal "
                "instead of expecting it away (L2)"
            )
    return errors


def selftest() -> None:
    """Decision table for the poisoned-lock rules.

    | Rule | Source                                                          | Effect |
    |---|---|---|
    | P1 | `.lock().map_err(..)?`, `.lock().ok()`, `if let Ok(g) = m.lock()` | accept |
    | P2 | a forbidden spelling in a cfg(test) module                       | accept |
    | P3 | a forbidden spelling in the sync crate                           | accept (not scanned) |
    | P4 | `.lock().expect(..)` / `.lock().unwrap()` / `.write().unwrap()`  | reject L1 |
    | P5 | `.frozen(t).expect("slots mutex poisoned")`                      | reject L2 |
    | P6 | the forbidden text only inside a comment                         | accept |
    | P7 | `.expect("..")` whose message does not name poison               | accept (not a lock read) |
    | P8 | tokio `.lock().await`, a leaf `.lock().push(..)`                 | accept (cannot poison / recovers) |
    | P9 | any production crate other than the sync crate                   | scanned |
    | P10 | `store.read("path").unwrap()`, a call with a string argument    | accept (not a lock read) |
    | P11 | L1/L2 inside a fn carrying a std-lock `#[expect]`                | reject; annotations grant no exemption |
    | P12 | annotated and adjacent forbidden bodies                         | reject each independent site |
    | P13-P17 | bodyless, const-header and stacked-attribute syntax          | reject every L1 site; no item exemption |
    | P18 | an annotated acquisition with typed `map_err` propagation       | accept |
    | P19 | a direct poison expect matching L1 and L2                       | one L1 diagnostic |
    | P20 | direct and indirect poison expects on the same line             | one L1 and one L2 diagnostic |
    | P21 | a poison expect after a string-argument read                     | one L2 diagnostic (no L1 suppression) |
    """
    path = "crates/server/awaken-any/src/lib.rs"
    clean = (
        "fn f() -> Result<(), E> { let g = m.lock().map_err(|_| E::Poisoned)?; "
        "let h = n.lock().ok(); if let Ok(i) = o.lock() {} p.lock().await; "
        "leaf.lock().push(1); Ok(()) }\n"
    )
    assert poisoned_lock_violations({path: clean}) == [], "P1/P8"
    forbidden = "fn f() { let _ = m.lock().expect(\"x\"); r.frozen(t).expect(\"slots poisoned\"); }\n"
    assert poisoned_lock_violations(
        {path: clean + "#[cfg(test)]\nmod tests {\n" + forbidden + "}\n"}
    ) == [], "P2 cfg(test)"
    rejected = {
        "P4a": "fn f() { let _ = self.slots.lock().expect(\"slots\"); }\n",
        "P4b": "fn f() { let _ = self.slots.lock().unwrap(); }\n",
        "P4c": "fn f() { let _ = self.sink.write().unwrap(); }\n",
        "P5": "fn f() { let _ = r.frozen(t).expect(\"session runtime slots mutex poisoned\"); }\n",
    }
    for rule, source in rejected.items():
        for crate in ("crates/platform/execution/awaken-session-executor/src/a.rs", "crates/devtools/x-testkit/src/b.rs"):
            assert len(poisoned_lock_violations({crate: source})) == 1, (rule, "P9")
    commented = "// m.lock().expect(\"x\") and .expect(\"poisoned\")\nfn f() {}\n"
    assert poisoned_lock_violations({path: commented}) == [], "P6"
    assert poisoned_lock_violations(
        {path: "fn f() { let _ = v.first().expect(\"non-empty by construction\"); }\n"}
    ) == [], "P7"
    assert poisoned_lock_violations(
        {path: "fn f() { let _ = store.read(\"path\").unwrap(); w.write(r\"x\").expect(\"io\"); }\n"}
    ) == [], "P10"
    reviewed = (
        "#[expect(\n    clippy::disallowed_methods,\n    reason = \"the IAM API fixes this std lock\"\n)]\n"
        "fn state(&self) -> G { self.state.lock().expect(\"state lock poisoned\") }\n"
    )
    assert len(poisoned_lock_violations({path: reviewed})) == 1, "P11 annotation does not exempt L1/L2"
    assert len(poisoned_lock_violations({path: reviewed + forbidden})) == 3, "P12"

    # P13-P17 retain the bodyless/const-header/stacked-attribute inputs. Neither
    # the annotated item nor its neighbor gains an L1/L2 exemption: every actual
    # acquisition panic is rejected. Reuse the canonical Rust projections and
    # the original violation oracle rather than parsing item ranges again.
    attribute = reviewed[:reviewed.index("fn state")]
    adjacent = "fn next(lock: &awaken_sync::InvariantLock<()>) { let _guard = lock.lock().unwrap(); }"
    for annotation in (
        attribute,
        "#[allow(clippy::disallowed_methods)]\n",
        "#[cfg_attr(not(test), expect(clippy::disallowed_methods))]\n",
    ):
        indirect_errors = poisoned_lock_violations({path: annotation + rejected["P5"]})
        assert len(indirect_errors) == 1 and indirect_errors[0].endswith("(L2)"), "P11 annotations do not exempt indirect L2"
    for rule, source in (
        ("P13 trait declaration", "trait T { " + attribute + "fn state(&self); " + adjacent + " }"),
        ("P14 const owns no following fn", attribute + "const MARKER: usize = 1; " + adjacent),
        ("P17 bodyless const header", "trait T { " + attribute + "fn state(&self, _: [u8; { 1 }]); " + adjacent + " }"),
    ):
        assert len(poisoned_lock_violations({path: source})) == 1, rule
    for rule, source in (
        ("P15 const header body", attribute + "fn state<const N: usize>(&self, _: [u8; { 1 }]) -> G { self.state.lock().expect(\"state lock poisoned\") }"),
        ("P16 stacked attributes", attribute + "#[inline] pub(crate) async unsafe fn state(&self) -> G { self.state.lock().expect(\"state lock poisoned\") }"),
    ):
        assert len(poisoned_lock_violations({path: source})) == 1, rule
        assert len(poisoned_lock_violations({path: source + adjacent})) == 2, rule + " both bodies"

    # P18: a std-representation annotation is harmless when the refusal is
    # propagated. P19-P21 deduplicate only overlapping diagnostics for the SAME
    # reported L1 match, never a whole line or a read that L1 already excluded.
    assert poisoned_lock_violations(
        {path: attribute + "fn state(&self) -> Result<G, E> { self.state.lock().map_err(E::from) }"}
    ) == [], "P18 annotated typed propagation"
    direct = 'fn f() { m.lock().expect("state poisoned"); }'
    direct_errors = poisoned_lock_violations({path: direct})
    assert len(direct_errors) == 1 and direct_errors[0].endswith("(L1)"), "P19 same site"
    independent = 'fn f() { m.lock().expect("state poisoned"); r.frozen(t).expect("slots poisoned"); }'
    independent_errors = poisoned_lock_violations({path: independent})
    assert len(independent_errors) == 2, "P20 same line, independent sites"
    assert independent_errors[0].endswith("(L1)") and independent_errors[1].endswith("(L2)"), "P20 rule identities"
    read_errors = poisoned_lock_violations(
        {path: 'fn f() { store.read("path").expect("io poisoned"); }'}
    )
    assert len(read_errors) == 1 and read_errors[0].endswith("(L2)"), "P21 excluded L1 cannot suppress L2"

    # P3/P9: discovery follows Cargo manifests; only the sync crate is outside.
    import tempfile

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        for name in ("awaken-sync", "moved-anywhere"):
            crate = root / "crates" / "nested" / name
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(f'[package]\nname = "{name}"\n', encoding="utf-8")
            (crate / "src" / "lib.rs").write_text(forbidden, encoding="utf-8")
        assert len(check_all(root)) == 2, "P3 sync crate excluded, P9 every other crate scanned"


def check_all(repo_root: Path) -> list[str]:
    """Scan the files a production build compiles: a module declared under
    `cfg(test)` (or reached only through one) is test code."""
    sources = {
        path.relative_to(repo_root.resolve()).as_posix(): path.read_text(encoding="utf-8")
        for path in _crate_boundary_workspace.production_module_files(scanned_manifests(repo_root))
    }
    return poisoned_lock_violations(sources)


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        print("usage: python3 scripts/ci/_poisoned_lock_fitness.py --self-test", file=sys.stderr)
        raise SystemExit(2)
    selftest()
    print("_poisoned_lock_fitness selftest: the decision table holds")
