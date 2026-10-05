#!/usr/bin/env python3
"""No Rust test disappears silently.

The crate-boundary work moves tests between crates and rewrites how they build
their fixtures. A move or a fixture rewrite must never lose a test, and losing
one is invisible to a green test run: a deleted test simply stops running.

`test_inventory.txt` records the name of every Rust test function in the
workspace, one per line, repeated once per occurrence (names are a multiset,
since the same name may exist in several crates). The check requires every
recorded occurrence to still exist, by name, in a file some test build compiles
(module-reachable from a crate's lib, bin, integration-test or bench root; a
file no `mod` declaration reaches does not count, because its tests never run).
Identity
is the function name, not its crate, file or body: moving a test to another
crate, splitting a file, or changing how the test constructs its host keeps it
matched. Renaming or deleting a test is recorded in `test_inventory_ledger.json`
with the new name or the reason, in the same commit.

New tests need no bookkeeping. `--write` refreshes the inventory (recording new
tests) and refuses to drop a name the ledger does not account for.

A test is a function under `#[test]`, `#[tokio::test]` (any arguments),
`#[rstest]`, `#[test_case(..)]`, `#[sqlx::test]` or `#[test_log::test]`, or a
`fn` inside a `proptest! { .. }` block. Comments are ignored. Inside
`macro_rules!` bodies the template name (for example `$name`) is recorded as
written, so a generated family stays one stable entry.
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

ROOT = Path(__file__).resolve().parents[2]
INVENTORY = ROOT / "scripts/ci/test_inventory.txt"
LEDGER = ROOT / "scripts/ci/test_inventory_ledger.json"

# The scan reads every Rust file under crates/ and the two records.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    "scripts/ci/test_inventory.txt",
    "scripts/ci/test_inventory_ledger.json",
)

TEST_ATTRIBUTE = re.compile(
    r"#\[\s*(?:tokio::test|test|rstest|test_case|sqlx::test|test_log::test)\b[^\]]*\]"
)
FUNCTION = re.compile(r"\bfn\s+(\$?\w+)")
PROPTEST_BLOCK = re.compile(r"\bproptest!\s*\{")


def test_names(source: str) -> list[str]:
    """Every test function name in one Rust source, in order of appearance."""
    code = _crate_boundary_workspace.rust_without_comments(source)
    names: list[str] = []
    for attribute in TEST_ATTRIBUTE.finditer(code):
        found = FUNCTION.search(code, attribute.end())
        if found:
            names.append(found.group(1))
    for block in PROPTEST_BLOCK.finditer(code):
        depth, cursor = 1, block.end()
        while cursor < len(code) and depth:
            depth += {"{": 1, "}": -1}.get(code[cursor], 0)
            cursor += 1
        names.extend(FUNCTION.findall(code[block.end() : cursor]))
    return names


def compiled_test_sources(root: Path = ROOT) -> list[Path]:
    """Rust files a test build compiles: module-reachable from a crate's lib,
    bin, integration-test or bench roots. A file no `mod` declaration reaches
    is not compiled, so a test inside it does not run and does not count."""
    manifests = _crate_boundary_workspace.crate_manifests(root / "crates")
    reachable = _crate_boundary_workspace.production_module_files(
        manifests,
        project=_crate_boundary_workspace.rust_without_comments,
        include_test_targets=True,
    )
    return sorted(reachable)


def current_inventory(root: Path = ROOT) -> Counter[str]:
    counts: Counter[str] = Counter()
    for path in compiled_test_sources(root):
        counts.update(test_names(path.read_text(encoding="utf-8", errors="ignore")))
    return counts


def read_inventory(path: Path = INVENTORY) -> Counter[str]:
    if not path.exists():
        return Counter()
    return Counter(line for line in path.read_text(encoding="utf-8").splitlines() if line)


def read_ledger(path: Path = LEDGER) -> dict[str, dict[str, str]]:
    if not path.exists():
        return {"renamed": {}, "removed": {}}
    data = json.loads(path.read_text(encoding="utf-8"))
    return {"renamed": dict(data.get("renamed", {})), "removed": dict(data.get("removed", {}))}


def missing(
    recorded: Counter[str], current: Counter[str], ledger: dict[str, dict[str, str]]
) -> list[str]:
    """Recorded occurrences no longer present and not accounted for by the ledger."""
    errors: list[str] = []
    for name, count in sorted(recorded.items()):
        have = current.get(name, 0)
        if have >= count:
            continue
        if name in ledger["removed"]:
            continue
        renamed = ledger["renamed"].get(name)
        if renamed is not None and current.get(renamed, 0) > 0:
            continue
        lost = count - have
        errors.append(
            f"test {name!r}: {lost} recorded occurrence(s) no longer exist; move the test "
            f"rather than deleting it, or record the rename/removal in {LEDGER.name}"
        )
    for kind in ("renamed", "removed"):
        for name, value in sorted(ledger[kind].items()):
            if not str(value).strip():
                errors.append(f"{LEDGER.name}: {kind} entry {name!r} needs a value")
    return errors


def render(counts: Counter[str]) -> str:
    return "".join(f"{name}\n" for name in sorted(counts.elements()))


def selftest() -> None:
    """Decision table.

    | Rule | cause                                              | effect |
    | T1   | attribute forms: test, tokio::test(args), rstest,  | found  |
    |      | test_case(..), sqlx::test, test_log::test          |        |
    | T2   | attribute in a comment                             | ignored|
    | T3   | proptest! block functions                          | found  |
    | T4   | macro template `fn $name`                          | found as `$name` |
    | T5   | recorded test still present (any crate)            | accept |
    | T6   | recorded test gone, not in ledger                  | error  |
    | T7   | recorded test gone, ledger renamed and new exists  | accept |
    | T8   | recorded test gone, ledger renamed but new missing | error  |
    | T9   | recorded test gone, ledger removed with reason     | accept |
    | T10  | one of two same-name occurrences gone              | error  |
    | T11  | ledger entry with an empty value                   | error  |
    """
    source = """
        #[test] fn plain() {}
        #[tokio::test(flavor = "multi_thread")]
        async fn async_one() {}
        #[rstest]
        #[case(1)]
        fn table(#[case] x: u8) {}
        #[test_case(1 ; "one")] fn cased(x: u8) {}
        #[sqlx::test] async fn db() {}
        #[test_log::test] fn logged() {}
        // #[test] fn commented() {}
        /* #[test] fn block_commented() {} */
        proptest! { #[test] fn prop_a(x in 0..1u8) {} fn prop_b(x in 0..1u8) {} }
        macro_rules! family { ($name:ident) => { #[test] fn $name() {} }; }
    """
    names = test_names(source)
    for expected in ("plain", "async_one", "table", "cased", "db", "logged"):
        assert expected in names, f"T1 {expected}"
    assert "commented" not in names and "block_commented" not in names, "T2"
    assert "prop_a" in names and "prop_b" in names, "T3"
    assert "$name" in names, "T4"
    empty = {"renamed": {}, "removed": {}}
    assert not missing(Counter({"a": 1}), Counter({"a": 1}), empty), "T5"
    assert missing(Counter({"a": 1}), Counter(), empty), "T6"
    assert not missing(Counter({"a": 1}), Counter({"b": 1}), {"renamed": {"a": "b"}, "removed": {}}), "T7"
    assert missing(Counter({"a": 1}), Counter(), {"renamed": {"a": "b"}, "removed": {}}), "T8"
    assert not missing(Counter({"a": 1}), Counter(), {"renamed": {}, "removed": {"a": "covered by b"}}), "T9"
    assert missing(Counter({"a": 2}), Counter({"a": 1}), empty), "T10"
    assert missing(Counter(), Counter(), {"renamed": {}, "removed": {"a": " "}}), "T11"
    # T12 a test in a file no `mod` reaches does not count; T13 cfg(test)
    # modules and integration-test roots count; T14 a build script's modules
    # do not count (cargo test never runs them).
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crate = root / "crates/x/y"
        (crate / "src").mkdir(parents=True)
        (crate / "tests").mkdir()
        (crate / "Cargo.toml").write_text('[package]\nname = "y"\n')
        (crate / "src/lib.rs").write_text("#[test] fn moved() {}\n#[cfg(test)]\nmod unit;\n")
        (crate / "src/unit.rs").write_text("#[test] fn unit_case() {}\n")
        (crate / "src/orphan.rs").write_text("#[test] fn never_compiled() {}\n")
        (crate / "tests/flow.rs").write_text("#[test] fn integration_case() {}\n")
        (crate / "build.rs").write_text("mod build_support;\nfn main() {}\n")
        (crate / "build_support.rs").write_text("#[test] fn build_only() {}\n")
        (crate / "target").mkdir()
        (crate / "target/gen.rs").write_text("#[test] fn built() {}\n")
        assert current_inventory(root) == Counter(
            {"moved": 1, "unit_case": 1, "integration_case": 1}
        ), "T12/T13/T14"
    print("check_test_inventory selftest: the decision table holds")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    parser.add_argument(
        "--write",
        action="store_true",
        help="refresh test_inventory.txt; refuses while a recorded test is unaccounted for",
    )
    args = parser.parse_args(argv)
    if args.self_test:
        selftest()
        return 0
    current = current_inventory()
    errors = missing(read_inventory(), current, read_ledger())
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    if args.write:
        INVENTORY.write_text(render(current), encoding="utf-8")
        print(f"wrote {INVENTORY.relative_to(ROOT)}: {sum(current.values())} tests")
        return 0
    print(f"OK - all {sum(read_inventory().values())} recorded Rust tests still exist.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
