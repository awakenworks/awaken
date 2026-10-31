#!/usr/bin/env python3
"""Print how far production code is from each global principle; never a gate.

Plan v5 S3: a number in the plan or in STATUS comes from a command, not from a
hand count. This prints the plan's principle table from the tree:

* P7 a lock read that panics on poison (`.lock()/.read()/.write()` followed by
  `.expect(..)`/`.unwrap()`; a tokio `.lock().await` cannot poison);
* P5 a `Weak<..>` back-reference;
* P5 an optional role, `Option<&dyn ..>` with or without a lifetime;
* P6 a typed failure flattened into text, `Internal(x.to_string())`;
* D6 the number of crates whose metadata declares `layer = "contract"`.

Code rows count production code only: the modules reachable from each crate's
library and binary roots (`production_module_files`) with `cfg(test)` items
and comments removed (`production_rust`). Each row shows its total, the
bounded contexts it falls in (Session always listed) and its top five crates.
`--guards` adds the governance measures of plan v5 V1: the Python scripts
under scripts/ci, their lines, and the entries of the ratchet baselines.

The exit status reports only whether the tool ran; a count never fails it.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tempfile
from collections import Counter
from pathlib import Path

import _crate_boundary_workspace

# Every file this command reads: crate manifests and sources, the scripts it
# counts and the baselines whose entries it counts.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/Cargo.toml",
    "crates/**/*.rs",
    "scripts/ci/*.py",
    "scripts/ci/*_baseline.json",
)

SESSION_CONTEXT = "session"
TOP = 5

# (principle, rule, pattern) over production source with comments removed.
# String literals stay, so `store.read("path").unwrap()` is not a lock read.
CODE_RULES: tuple[tuple[str, str, re.Pattern[str]], ...] = (
    ("P7", "lock poison panics",
     re.compile(r"\.\s*(?:lock|read|write)\s*\(\s*\)\s*\.\s*(?:expect|unwrap)\s*\(")),
    ("P5", "`Weak<` back-references", re.compile(r"\bWeak\s*<")),
    ("P5", "optional roles `Option<&dyn ..>`",
     re.compile(r"\bOption\s*<\s*&\s*(?:'\w+\s+)?(?:mut\s+)?dyn\b")),
    ("P6", "`Internal(x.to_string())`",
     re.compile(r"\bInternal\s*\(\s*[\w.]+\.to_string\s*\(\s*\)\s*\)")),
)
CONTRACT_RULE = ("D6", "contract crates")

# Keys of a baseline file that annotate an allowance rather than hold one.
BASELINE_ANNOTATIONS = frozenset({
    "$comment", "comment", "growth_reasons", "reason", "owner", "expires",
    "approved_by", "date", "remove_when", "history",
})


def count_source(source: str) -> Counter[str]:
    """Rule hits in one Rust source file, production code only."""
    production = _crate_boundary_workspace.production_rust(source)
    return Counter({rule: len(pattern.findall(production)) for _, rule, pattern in CODE_RULES})


def crate_rows(crates: Path) -> list[tuple[str, str, str, Counter[str]]]:
    """(crate, bounded context, layer, rule hits) for every workspace crate."""
    rows = []
    for manifest_path in _crate_boundary_workspace.crate_manifests(crates):
        manifest = _crate_boundary_workspace.load_manifest(manifest_path)
        metadata = manifest.get("package", {}).get("metadata", {}).get("awaken", {})
        hits: Counter[str] = Counter()
        for path in sorted(_crate_boundary_workspace.production_module_files([manifest_path])):
            hits.update(count_source(path.read_text(encoding="utf-8")))
        rows.append((
            _crate_boundary_workspace.package_name(manifest),
            metadata.get("bounded_context", "unplaced"),
            metadata.get("layer", "-"),
            hits,
        ))
    return rows


def _ranked(counts: Counter[str], limit: int) -> list[tuple[str, int]]:
    """Largest first, ties by name; zero counts are not shown."""
    return sorted(((k, v) for k, v in counts.items() if v), key=lambda kv: (-kv[1], kv[0]))[:limit]


def _contexts(counts: Counter[str]) -> str:
    shown = dict(_ranked(counts, 3))
    shown.setdefault(SESSION_CONTEXT, counts.get(SESSION_CONTEXT, 0))
    return ", ".join(f"{k} {v}" for k, v in shown.items())


def principle_table(rows: list[tuple[str, str, str, Counter[str]]]) -> list[tuple[str, ...]]:
    """(principle, rule, total, by context, top crates) per table row."""
    table = []
    for principle, rule, _ in CODE_RULES:
        by_crate = Counter({crate: hits[rule] for crate, _, _, hits in rows})
        by_context: Counter[str] = Counter()
        for crate, context, _, hits in rows:
            by_context[context] += hits[rule]
        top = ", ".join(f"{k} {v}" for k, v in _ranked(by_crate, TOP))
        table.append((principle, rule, str(sum(by_crate.values())), _contexts(by_context), top))
    contracts = [(crate, context) for crate, context, layer, _ in rows if layer == "contract"]
    by_context = Counter(context for _, context in contracts)
    session = ", ".join(sorted(crate for crate, context in contracts if context == SESSION_CONTEXT))
    table.append((*CONTRACT_RULE, str(len(contracts)), _contexts(by_context), f"session: {session}"))
    return table


def baseline_entries(document: object) -> int:
    """Allowances a baseline holds: every leaf value, annotations excluded."""
    if isinstance(document, dict):
        return sum(baseline_entries(v) for k, v in document.items() if k not in BASELINE_ANNOTATIONS)
    if isinstance(document, list):
        return len(document)
    return 1


def guard_table(scripts: Path) -> list[tuple[str, ...]]:
    python = sorted(scripts.glob("*.py"))
    lines = sum(len(path.read_text(encoding="utf-8").splitlines()) for path in python)
    baselines = sorted(scripts.glob("*_baseline.json"))
    entries = sum(baseline_entries(json.loads(path.read_text(encoding="utf-8"))) for path in baselines)
    kinds = Counter(
        "test" if p.name.startswith("test_") else "module" if p.name.startswith("_") else "entry"
        for p in python
    )
    return [
        ("V1", "Python scripts (entry / module / test)",
         f"{len(python)} ({kinds['entry']} / {kinds['module']} / {kinds['test']})"),
        ("V1", "Python lines", str(lines)),
        ("V1", "baseline files / entries", f"{len(baselines)} / {entries}"),
    ]


def render(header: tuple[str, ...], rows: list[tuple[str, ...]]) -> str:
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    out += ["| " + " | ".join(row) + " |" for row in rows]
    return "\n".join(out)


def selftest() -> None:
    """Decision table: each source shape and the rule hits it must produce.

    | Row | Source shape | Effect |
    |---|---|---|
    | S1 | `.lock().unwrap()`, `.write().expect("x")` | 2 lock hits |
    | S2 | tokio `.lock().await`, `.lock().map_err(..)?` | 0 (cannot poison / typed) |
    | S3 | `store.read("path").unwrap()` | 0 (an argument is not a lock read) |
    | S4 | a forbidden shape in a comment or a `cfg(test)` module | 0 |
    | S5 | `Weak<T>` field; `use std::sync::Weak;` | 1 Weak hit |
    | S6 | `Option<&dyn R>`, `Option<&'a dyn R>`; `Option<Arc<dyn R>>` | 2 role hits |
    | S7 | `Internal(e.to_string())`; `Internal(code)`, `Internal(format!(..))` | 1 P6 hit |
    | S8 | a module file no root reaches, a `tests/` file | not counted |
    | S9 | contract layer crates | counted by metadata, Session column listed |
    | S10 | annotations in a baseline | not entries |
    """
    hits = count_source(
        "fn f(m: &Mutex<u8>, r: &RwLock<u8>) {\n"
        "    m.lock().unwrap();\n    r.write().expect(\"x\");\n"  # S1
        "    t.lock().await;\n    m.lock().map_err(E::from)?;\n"  # S2
        "    store.read(\"path\").unwrap();\n"  # S3
        "    // m.lock().unwrap(); Weak<X> Internal(e.to_string())\n"  # S4
        "}\n"
        "#[cfg(test)]\nmod tests { fn t() { m.lock().unwrap(); let _: Weak<u8>; } }\n"  # S4
        "use std::sync::Weak;\nstruct S { parent: Weak<Inner> }\n"  # S5
        "trait T { fn a(&self) -> Option<&dyn R>; fn b<'a>(&'a self) -> Option<&'a dyn R>;\n"
        "          fn c(&self) -> Option<Arc<dyn R>>; }\n"  # S6
        "fn g() { E::Internal(e.to_string()); E::Internal(code); E::Internal(format!(\"{e}\")); }\n"  # S7
    )
    expected = {
        "lock poison panics": 2,
        "`Weak<` back-references": 1,
        "optional roles `Option<&dyn ..>`": 2,
        "`Internal(x.to_string())`": 1,
    }
    assert dict(hits) == expected, f"S1-S7: {dict(hits)}"

    with tempfile.TemporaryDirectory() as tmp:
        crates = Path(tmp) / "crates"
        for name, context, layer in (
            ("demo-contract", "session", "contract"),
            ("demo-app", "dispatch", "application"),
        ):
            root = crates / name
            (root / "src").mkdir(parents=True)
            (root / "tests").mkdir()
            (root / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "0.1.0"\n'
                f'[package.metadata.awaken]\nbounded_context = "{context}"\nlayer = "{layer}"\n',
                encoding="utf-8",
            )
            (root / "src/lib.rs").write_text("mod used;\nfn f() { m.lock().unwrap(); }\n", encoding="utf-8")
            (root / "src/used.rs").write_text("struct S(Weak<u8>);\n", encoding="utf-8")
            (root / "src/orphan.rs").write_text("fn f() { m.lock().unwrap(); }\n", encoding="utf-8")  # S8
            (root / "tests/it.rs").write_text("fn f() { m.lock().unwrap(); }\n", encoding="utf-8")  # S8
        table = {row[1]: row for row in principle_table(crate_rows(crates))}
        assert table["lock poison panics"][2] == "2", f"S8: {table['lock poison panics']}"
        assert table["lock poison panics"][4] == "demo-app 1, demo-contract 1", "ranking ties by name"
        assert table["`Weak<` back-references"][2] == "2", "S8: reachable module counted"
        contract = table["contract crates"]
        assert contract[2:] == ("1", "session 1", "session: demo-contract"), f"S9: {contract}"
        assert table["`Internal(x.to_string())`"][3] == "session 0", "S9: session always shown"

    annotated = {"$comment": "x", "rule": {"a": 1, "b": 2}, "growth_reasons": {"rule": {"a": "r"}},
                 "list": ["c"], "record": {"files": 3, "reason": "r"}}
    assert baseline_entries(annotated) == 4, "S10"
    print("principle status: self-test passed")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="run the decision table and exit")
    parser.add_argument("--guards", action="store_true", help="also print the V1 governance measures")
    options = parser.parse_args(argv)
    if options.self_test:
        selftest()
        return 0
    rows = crate_rows(_crate_boundary_workspace.CRATES)
    print(render(("Principle", "Rule", "Now", "By context", "Top crates"), principle_table(rows)))
    if options.guards:
        print()
        print(render(("Item", "Measure", "Now"), guard_table(Path(__file__).resolve().parent)))
    print("\nGenerated by python3 scripts/ci/principle_status.py; production code only.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
