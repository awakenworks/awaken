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

New tests need no bookkeeping. Default/`--check` reads without writing; an
absent inventory keeps the original empty floor. `--tighten` adds or raises
minimum occurrences while retaining every held requirement. Explicit
`--write-baseline` records the current multiset only after the original ledger
check. Neither mode nor `--reason` can account for a missing test instead of
the ledger. The TXT format and the ledger's name-based decisions stay intact.

* `test-occurrence`: every held occurrence remains or its ledger accounts for it.
* `test-ledger`: every removal reason or rename target has a nonempty value.

A test is a function under `#[test]`, `#[tokio::test]` (any arguments),
`#[rstest]`, `#[test_case(..)]`, `#[sqlx::test]` or `#[test_log::test]`, or a
`fn` inside a `proptest! { .. }` block. Comments are ignored. Inside
`macro_rules!` bodies the template name (for example `$name`) is recorded as
written, so a generated family stays one stable entry.
"""

from __future__ import annotations

import json
import re
import sys
import tempfile
from collections import Counter
from pathlib import Path

import _crate_boundary_workspace
import _architecture_ratchet

ROOT = Path(__file__).resolve().parents[2]
INVENTORY = ROOT / "scripts/ci/test_inventory.txt"
LEDGER = ROOT / "scripts/ci/test_inventory_ledger.json"

# Structural reachability reads Cargo targets and the module helper; the
# existing local helpers and the canonical CLI/test boundary are inputs too.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "scripts/ci/*.py",
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


def parse_inventory(text: str) -> Counter[str]:
    """The original TXT decoder, with physical IO owned by baseline_cli."""
    return Counter(line for line in text.splitlines() if line)


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
        errors.append(_architecture_ratchet.diagnostic(
            "test-occurrence", repr(name), count, have,
            f"move the test rather than deleting it, or record its rename/removal in {LEDGER.name}",
            details=f"{lost} recorded occurrence(s) no longer exist"))
    for kind in ("renamed", "removed"):
        for name, value in sorted(ledger[kind].items()):
            if not str(value).strip():
                errors.append(_architecture_ratchet.diagnostic(
                    "test-ledger", f"{kind}[{name!r}]", "nonempty value", "missing",
                    f"restore the owning value in {LEDGER.name}; a CLI reason cannot replace it"))
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
    # IC4 the original ledger accounts by old name, never by a one-to-one
    # occurrence transfer. Multiple names/counts may point to one live target.
    assert not missing(Counter({"a": 2, "b": 3}), Counter({"new": 1}),
                       {"renamed": {"a": "new", "b": "new"}, "removed": {}}), "IC4 original many-to-one rename"
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
    _selftest_cli_snapshot()
    print("check_test_inventory selftest: the decision table holds")


def _selftest_cli_snapshot() -> None:
    """I1-I6/IC1-IC9 graph: parse -> original floor -> current/ledger -> missing.

    Admission failure -> no candidate/write; admitted snapshots + mode ->
    read-only / pointwise-max tighter floor / current explicit record. Reason
    has no edge to ledger permission. A hypothetical later record never owns
    the result. This is a floor: higher occurrences mean more protection.

    Rule | causes                                     | effects
    I1   | equal held/current; later read would differ | one read/decode; validated held report
    I2   | extra current name/occurrence               | check no write; tighten protects; write records
    I3   | missing held occurrence, no ledger/reason   | all modes refuse; original bytes intact
    I4   | removed reason / rename target exists      | original name scope, including two old ->one new
    I5   | absent TXT, check/write/tighten             | empty floor / init / require existing before scan
    I6   | old/unknown/abbreviated/conflicting argv     | usage before input IO; selftest standalone
    IC4  | blank ledger value even without loss       | hard refusal, no CLI reason repairs it
    IC5  | authorized loss plus new names             | tighten retains old floor; write uses current
    IC8  | original real collector + codec            | one invocation; original T1-T14/byte representation
    IC9  | decode/collector/ledger/encoder failure     | original bytes intact; existing exception policy
    """
    import contextlib
    import io
    from unittest.mock import patch

    collector, decoder, ledger_load, checker, encoder = current_inventory, parse_inventory, read_ledger, missing, render
    original_read = Path.read_text

    def invoke(argv, held="retained\n", source="#[test] fn retained() {}\n", ledger=None, failure=None):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "crates/fixture"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text('[package]\nname="fixture"\n')
            (crate / "src/lib.rs").write_text(source)
            record, decision = root / "inventory.txt", root / "ledger.json"
            if held is not None:
                record.write_text(held)
            decision.write_text(json.dumps(ledger if ledger is not None else {"removed": {}, "renamed": {}}))
            before = record.read_bytes() if record.exists() else None
            reads, calls, checks, decision_reads = [], [], [], []
            def fail(phase):
                if phase == failure:
                    raise OSError(f"fixture {phase} failure")
            def read(path, *args, **kwargs):
                if path == record:
                    reads.append(path)
                    if len(reads) > 1:
                        return "unvalidated\n" * 100
                elif path == decision:
                    decision_reads.append(path)
                return original_read(path, *args, **kwargs)
            def current():
                calls.append("current"); fail("collector")
                return collector(root)
            def decode(text):
                calls.append("recorded"); fail("decode")
                return decoder(text)
            def decisions():
                calls.append("ledger"); fail("ledger")
                return ledger_load(decision)
            def checked(previous, current, ledger):
                checks.append(previous)
                return checker(previous, current, ledger)
            def encode(counts):
                fail("encode")
                return encoder(counts)
            output, errors = io.StringIO(), io.StringIO()
            with patch.dict(main.__globals__, {"INVENTORY": record, "ROOT": root,
                    "current_inventory": current, "parse_inventory": decode, "read_ledger": decisions,
                    "missing": checked, "render": encode, "selftest": lambda: calls.append("selftest")}), \
                    patch.object(Path, "read_text", read), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                try:
                    status = main(argv)
                except SystemExit as error:
                    status = error.code
                except OSError:
                    status = "OSError"
            after = record.read_bytes() if record.exists() else None
            assert len(reads) <= 1 and len(decision_reads) <= 1 and len(checks) <= 1, "I1/IC8 one record/ledger/admission"
            assert calls.count("current") <= 1 and calls.count("recorded") <= 1, "IC8 one real collector/decode"
            if checks:
                assert calls == (["recorded"] if held is not None else []) + ["current", "ledger"], "I1 shared IO precedes measurement"
            return status, before, after, output.getvalue(), calls, errors.getvalue()

    writes = (["--write-baseline"], ["--write-baseline", "--reason", "reviewed"])
    modes = ([], ["--check"], ["--tighten"], *writes)
    for argv in modes:
        status, before, after, output, _, _ = invoke(argv)
        assert status == 0 and before == after, "I1 equal exact bytes"
        if argv in ([], ["--check"]):
            assert "all 1 recorded" in output, "I1 validated held report"
    added = "#[test] fn retained() {}\n#[test] fn fresh() {}\n"
    for argv in ([], ["--check"]):
        status, before, after, output, _, _ = invoke(argv, source=added)
        assert status == 0 and before == after and "all 1 recorded" in output, "I2 new tests need no write"
    for argv in (["--tighten"], *writes):
        status, _, after, _, _, _ = invoke(argv, source=added)
        assert status == 0 and after == b"fresh\nretained\n", "I2 existing TXT renderer / floor tightening"
    repeated = "mod a { #[test] fn retained() {} } mod b { #[test] fn retained() {} }"
    status, _, after, _, _, _ = invoke(["--tighten"], source=repeated)
    assert status == 0 and after == b"retained\nretained\n", "IC2 higher multiplicity tightens protection"
    for argv in modes:
        for held in ("lost\n", "retained\nretained\n"):
            status, before, after, _, _, errors = invoke(argv, held=held)
            assert status == 1 and before == after and "test-occurrence:" in errors, "I3 reason cannot authorize a lost occurrence"
        for ledger in ({"removed": {"lost": "covered by retained"}, "renamed": {}},
                       {"removed": {}, "renamed": {"lost": "retained"}}):
            status, before, after, _, _, _ = invoke(argv, held="lost\nlost\n", ledger=ledger)
            expected = b"retained\n" if argv in writes else b"lost\nlost\nretained\n" if argv == ["--tighten"] else before
            assert status == 0 and after == expected, "I4/IC5 original two-old/one-new name scope"
        for ledger in ({"removed": {"other": " "}, "renamed": {}}, {"removed": {}, "renamed": {"other": ""}}):
            status, before, after, _, _, errors = invoke(argv, ledger=ledger)
            assert status == 1 and before == after and "test-ledger:" in errors, "IC4 blank always hard"
        status, before, after, _, _, _ = invoke(argv, held="lost\n", ledger={"removed": {}, "renamed": {"lost": "absent"}})
        assert status == 1 and before == after, "I4 absent rename target refuses"
    for argv in ([], ["--check"], *writes):
        status, before, after, output, _, _ = invoke(argv, held=None)
        assert status == 0 and before is None and after == (b"retained\n" if argv in writes else None), "I5 original missing floor"
        if argv not in writes:
            assert "all 0 recorded" in output, "I5 empty Counter success report"
    status, before, after, _, calls, _ = invoke(["--tighten"], held=None)
    assert status == 1 and before is None and after is None and not calls, "IC6 shared tighten needs an existing floor"
    for argv in (["--write"], ["--unknown"], ["--w"], ["--write-baselin"], ["--self-tes"],
                 ["--check", "--tighten"], ["--self-test", "--write-baseline"], ["--reason", "why"],
                 ["--tighten", "--reason", "why"], ["--write-baseline", "--reason", " "]):
        status, before, after, _, calls, errors = invoke(argv)
        assert status == 2 and before == after and not calls and "usage:" in errors, "I6 parser before every input / no alias"
    status, before, after, _, calls, _ = invoke(["--self-test"])
    assert status == 0 and before == after and calls == ["selftest"], "IC7 standalone selftest"
    for phase in ("decode", "collector", "ledger", "encode"):
        status, before, after, _, _, _ = invoke(["--write-baseline"], source=added, failure=phase)
        assert status == "OSError" and before == after, "IC9 failure before writer leaves exact original bytes"
    # IC8 decoding keeps the original raw nonempty lines, including whitespace
    # and $name. A codec port is not permission to reinterpret TXT metadata.
    assert decoder(" z \n\n#name\n$name\n$name\n") == Counter({" z ": 1, "#name": 1, "$name": 2})
    assert encoder(Counter({"z": 1, "$name": 2})) == "$name\n$name\nz\n", "IC8 exact sorted multiset encoding"


def main(argv: list[str] | None = None) -> int:
    current: Counter[str] = Counter()
    ledger: dict[str, dict[str, str]] = {}

    def measure():
        nonlocal current, ledger
        current, ledger = current_inventory(), read_ledger()
        return [], current

    return _architecture_ratchet.baseline_cli(
        argv, prog="check_test_inventory.py", description=__doc__, baseline_path=INVENTORY,
        self_test=selftest, measure=measure,
        hard_rules=lambda previous: missing(previous, current, ledger),
        # Admission is checked once against the original floor. Both candidates
        # are built from those same snapshots: max only adds protection, and
        # current can drop a held requirement only when the ledger allowed it.
        check=lambda _current, _candidate: [],
        tighten=lambda current, previous: Counter(previous) | current,
        write=lambda current, _previous, _reason: (current, []),
        decode=parse_inventory, encode=render, empty_baseline=Counter, allow_missing_check=True,
        check_message=lambda _current, previous: f"OK - all {sum(previous.values())} recorded Rust tests still exist.",
    )


if __name__ == "__main__":
    raise SystemExit(main())
