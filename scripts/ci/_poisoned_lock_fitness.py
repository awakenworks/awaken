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
except `awaken-sync`, the one crate that reads a `std` lock. A site inside an
item carrying the reviewed `#[expect(clippy::disallowed_methods, reason = ..)]`
for a lock still on `std` is that lock's one recorded exemption and is not
counted twice. The count is held at zero: there is no baseline.

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
REVIEWED_STD_LOCK = re.compile(r"#\[\s*(?:cfg_attr\s*\([^\]]*?,\s*)?expect\s*\(\s*clippy\s*::\s*disallowed_methods\b")
FN_ITEM = re.compile(r"\bfn\s+[A-Za-z_]\w*")


def scanned_manifests(repo_root: Path) -> list[Path]:
    """Every workspace crate except the sync crate, discovered from Cargo."""
    return [
        path
        for path in _crate_boundary_workspace.crate_manifests(repo_root / "crates")
        if _crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(path))
        != SYNC_CRATE
    ]


def _reviewed_ranges(syntax: str) -> list[tuple[int, int]]:
    """Body spans of the fn items annotated with the reviewed std-lock expect."""
    ranges: list[tuple[int, int]] = []
    for attribute in REVIEWED_STD_LOCK.finditer(syntax):
        cursor = _crate_boundary_workspace._outer_attribute_end(syntax, attribute.start())
        if cursor is None:
            continue
        # Attributes belong to the immediately following item. Keep stacked
        # attributes, but never search past a const or bodyless declaration.
        while cursor < len(syntax):
            if syntax[cursor].isspace():
                cursor += 1
            elif syntax[cursor] == "#":
                following = _crate_boundary_workspace._outer_attribute_end(syntax, cursor)
                if following is None:
                    break
                cursor = following
            else:
                break
        end = _crate_boundary_workspace._rust_item_end(syntax, cursor)
        if end is None or syntax[end - 1] != "}":
            continue
        item = FN_ITEM.search(syntax, cursor, end)
        if item is not None and _crate_boundary_workspace._rust_item_end(syntax, item.start()) == end:
            ranges.append((attribute.start(), end))
    return ranges


def poisoned_lock_violations(sources: dict[str, str]) -> list[str]:
    """Rule violations in production sources, keyed by repository path.

    Cause/effect graph: C1 the source is production code outside the sync
    crate; C2 it unwraps a lock acquisition (L1); C3 it unwraps with a
    poison-naming `.expect` (L2); C4 the site is inside an item carrying the
    reviewed std-lock `#[expect(clippy::disallowed_methods)]`. E1 accept when
    C1 is false, C4 holds, or neither C2 nor C3 holds; E2 reject and name the
    file, line and rule.
    """
    errors: list[str] = []
    for relative, source in sorted(sources.items()):
        production = _crate_boundary_workspace.production_rust(source)
        syntax = _crate_boundary_workspace.rust_syntax(production)
        reviewed = _reviewed_ranges(syntax)

        def exempt(offset: int) -> bool:
            return any(start <= offset < end for start, end in reviewed)

        for match in LOCK_PANIC.finditer(syntax):
            # The blanked projection turns `read("path")` into `read(      )`;
            # a call with an argument is not a lock acquisition.
            if '"' in production[match.start():match.end()] or exempt(match.start()):
                continue
            line = syntax.count("\n", 0, match.start()) + 1
            errors.append(
                f"{relative}:{line}: an InvariantLock refuses poison with LockPoisoned; "
                "propagate it through the crate's typed error instead of unwrapping it (L1)"
            )
        for match in POISON_EXPECT.finditer(production):
            if exempt(match.start()):
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
    | P11 | L1/L2 inside a fn carrying the reviewed std-lock `#[expect]`    | accept (one recorded exemption) |
    | P12 | the same spelling in the next fn, outside that item             | reject |
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
        for crate in ("crates/server/awaken-session-executor/src/a.rs", "crates/devtools/x-testkit/src/b.rs"):
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
    assert poisoned_lock_violations({path: reviewed}) == [], "P11"
    assert len(poisoned_lock_violations({path: reviewed + forbidden})) == 2, "P12"

    # P13-P17 item-ownership causes/effects: a reviewed bodyless trait fn (P13)
    # or an attribute on a const (P14) cannot lend its exemption to the next fn.
    # P15 real function header const-expression braces are not its body end;
    # P16 stacked attributes/modifiers keep the SAME reviewed function, while
    # P17 unclosed/bodyless items never authorize a following lock unwrap.
    # Reuse the original L1/L2 oracle and reviewed attribute, not a second lexer
    # or a spelling-only assertion. Each unreviewed body below has one L1 site.
    attribute = reviewed[:reviewed.index("fn state")]
    adjacent = "fn next(lock: &awaken_sync::InvariantLock<()>) { let _guard = lock.lock().unwrap(); }"
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
        assert poisoned_lock_violations({path: source}) == [], rule
        assert len(poisoned_lock_violations({path: source + adjacent})) == 1, rule + " exact item only"

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
