#!/usr/bin/env python3
"""Refuse a Kani proof that may hold vacuously.

Kani counts an assertion that no input reaches as a pass, and a
``kani::cover!`` that no input satisfies does not change the verdict either.
A harness whose assumptions exclude a branch therefore proves nothing about it
while still printing ``VERIFICATION:- SUCCESSFUL``. This check reads Kani's
per-harness results and refuses two shapes:

* a cover property whose status is not ``SATISFIED`` (``UNSATISFIABLE``,
  ``UNREACHABLE`` or ``UNDETERMINED``): a branch a harness says it exercises
  was not exercised;
* an ``assertion`` (Rust ``assert!``/``assert_eq!``/``panic!``) in proof code
  whose status is ``UNREACHABLE``: the harness states something about a path
  no input takes. Proof code is a ``#[kani::proof]`` function or any item under
  ``#[cfg(kani)]``/``#[cfg(any(.., kani, ..))]`` in a workspace crate. An arm
  written ``unreachable!()`` is the harness saying so on purpose and is
  accepted; production code that one harness of a deliberate domain split does
  not reach is not this harness's vacuity and is accepted too.

Input. ``check`` reads the per-harness files Kani writes with
``-Z unstable-options --output-into-files`` (regular format, one file per
harness under ``result_output_dir/``; the formal gate passes the flag) or any
regular-format transcript. Empty or incomplete regular Check records are
unjudgeable and refused; a complete fragment does not establish proof completion.
A terse transcript carries only each harness's
cover summary (``** k of m cover properties satisfied``); ``cover_shortfall``
reads that, and ``formal_inventory`` uses it to class a run with an
unsatisfied cover as not passed::

    formal_vacuity.py check PATH... [--keep DIR]   # exit 1 on a finding
    formal_vacuity.py --self-test

``--keep`` copies every result file with a finding into DIR, so a gate that
removes the result directory keeps the evidence.

Consumers: scripts/ci/check_formal.sh (after the Kani batches) and
formal_inventory.py.
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

from _crate_boundary_workspace import _balanced_end, rust_syntax

ROOT = Path(__file__).resolve().parents[2]

ANSI = re.compile(r"\x1b\[[0-9;]*m")
CHECK = re.compile(
    r"^Check \d+: (?P<name>[^\n]+)\n"
    r"\t - Status: (?P<status>[A-Z]+)\n"
    r"\t - Description: \"(?P<description>(?:(?!^Check ).)*?)\"\n"
    r"(?:\t - Location: (?P<location>[^\n]*)\n)?(?=\n|^Check |\Z)",
    re.M | re.S,
)
CHECK_HEADER = re.compile(r"^Check [^\n]*(?:\n|$)", re.M)
LOCATION = re.compile(r"^(?P<path>\S+?):(?P<line>\d+)(?::\d+)?(?: in function .*)?$")
COVER_SUMMARY = re.compile(r"\*\* (\d+) of (\d+) cover properties satisfied")
# `unreachable!()` and `unreachable!("...")`: the arm is unreachable on purpose.
DELIBERATE = "internal error: entered unreachable code"
# `#[cfg(kani)]` and `#[cfg(any(.., kani, ..))]`; never `#[cfg(not(kani))]`.
KANI_CFG = re.compile(r"#\[\s*cfg\s*\(\s*(?:kani|any\s*\([^()]*\bkani\b[^()]*\))\s*\)\s*\]")
PROOF_ATTRIBUTE = re.compile(r"#\[\s*kani::proof\s*\]")


@dataclass(frozen=True)
class Check:
    name: str
    status: str
    description: str
    location: str

    @property
    def property_class(self) -> str:
        """``<function>.<class>.<id>`` or ``<class>.<id>``."""
        parts = self.name.rsplit(".", 2)
        return parts[-2] if len(parts) >= 2 else ""

    def source(self) -> tuple[str, int] | None:
        match = LOCATION.match(self.location)
        return (match["path"], int(match["line"])) if match else None


def checks(text: str) -> list[Check]:
    text = ANSI.sub("", text)
    matches = list(CHECK.finditer(text))
    parsed = {match.start() for match in matches}
    for header in CHECK_HEADER.finditer(text):
        if header.start() not in parsed:
            line = text.count("\n", 0, header.start()) + 1
            raise ValueError(f"incomplete or unparseable Kani Check at line {line}")
    if not matches:
        raise ValueError("no complete Kani Check record to judge")
    return [
        Check(match["name"], match["status"], match["description"], match["location"] or "")
        for match in matches
    ]


def cover_shortfall(text: str) -> str | None:
    """The first cover summary of ``text`` with an unsatisfied cover, if any."""

    for match in COVER_SUMMARY.finditer(ANSI.sub("", text)):
        satisfied, total = int(match[1]), int(match[2])
        if satisfied < total:
            return f"{satisfied} of {total} cover properties satisfied"
    return None


class ProofCode:
    """Which lines of a workspace source are proof code, read once per file."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.spans: dict[str, list[tuple[int, int]]] = {}

    def contains(self, path: str, line: int) -> bool:
        if path not in self.spans:
            self.spans[path] = self._spans(path)
        return any(first <= line <= last for first, last in self.spans[path])

    def _spans(self, path: str) -> list[tuple[int, int]]:
        if not path.startswith("crates/") or not path.endswith(".rs"):
            return []
        source = self.root / path
        if not source.is_file():
            return []
        syntax = rust_syntax(source.read_text(errors="replace"))
        spans = []
        for attribute in (*KANI_CFG.finditer(syntax), *PROOF_ATTRIBUTE.finditer(syntax)):
            # The attributed item's body: the first `{` before any `;` (an
            # attributed `use` or `mod x;` has no body here).
            brace = syntax.find("{", attribute.end())
            semicolon = syntax.find(";", attribute.end())
            if brace < 0 or 0 <= semicolon < brace:
                continue
            end = _balanced_end(syntax, brace, "{", "}") or len(syntax)
            spans.append((syntax.count("\n", 0, attribute.start()) + 1, syntax.count("\n", 0, end) + 1))
        return spans


def findings(text: str, proof_code: ProofCode) -> list[str]:
    """Every vacuity finding in one regular-format result or transcript."""

    found = []
    for check in checks(text):
        where = check.location or "unknown location"
        if check.property_class == "cover":
            if check.status != "SATISFIED":
                found.append(f"cover {check.description!r} is {check.status} ({where})")
            continue
        if check.property_class != "assertion" or check.status != "UNREACHABLE":
            continue
        if check.description.startswith(DELIBERATE):
            continue
        source = check.source()
        if source is not None and proof_code.contains(*source):
            found.append(f"assertion {check.description!r} is UNREACHABLE ({where})")
    return found


def result_files(paths: list[Path]) -> list[Path]:
    files: list[Path] = []
    for path in paths:
        if path.is_dir():
            # Kani writes one file per harness, directly in the directory.
            files += sorted(entry for entry in path.iterdir() if entry.is_file())
        elif path.is_file():
            files.append(path)
        else:
            raise FileNotFoundError(f"no Kani results at {path}")
    return files


def check(paths: list[Path], keep: Path | None, root: Path = ROOT) -> int:
    proof_code = ProofCode(root)
    files = result_files(paths)
    if not files:
        print("formal vacuity: no per-harness Kani results to judge", file=sys.stderr)
        return 1
    refused = 0
    for path in files:
        try:
            problems = findings(path.read_text(errors="replace"), proof_code)
        except ValueError as error:
            problems = [str(error)]
        if not problems:
            continue
        refused += 1
        for problem in problems:
            print(f"formal vacuity: {path.name}: {problem}", file=sys.stderr)
        if keep is not None:
            keep.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, keep / path.name)
    print(f"formal vacuity: {len(files)} harness result(s), {refused} with a vacuous or unjudgeable property")
    return 1 if refused else 0


def self_test() -> None:
    """Decision table of ``findings`` and ``cover_shortfall``.

    Causes: V1 a cover SATISFIED; V2 a cover UNSATISFIABLE, UNREACHABLE or
    UNDETERMINED; V3 an assertion UNREACHABLE inside a `#[kani::proof]` fn;
    V4 the same inside a `#[cfg(kani)]` helper; V5 an UNREACHABLE
    `unreachable!()` arm in proof code; V6 an UNREACHABLE assertion in
    production code of the same file; V7 an UNREACHABLE overflow check in proof
    code; V8 a SUCCESS assertion in proof code; V9 a library location; V10 a
    terse summary with every cover satisfied / one short; V11 colour codes.
    Effects: V2, V3, V4 => one finding each; V1, V5, V6, V7, V8, V9 => none;
    V10 => None / the shortfall; V11 parsed as without them.
    V12 Kani prints a multiline Description or a Rust qualified name with
    spaces: parse the same full property and preserve its exact description,
    status, location and original V1-V9 classification. V13 adjacent Checks
    cannot lend their status/description/location to an incomplete predecessor.
    V14 empty, terse-only, malformed or unfinished Check input (including a
    partial Location) => refuse and keep its raw
    file; a complete Check in a partial transcript may be inspected but is not
    evidence that the interrupted proof or full formal gate completed.
    """

    import tempfile  # noqa: PLC0415 - only the self-test builds fixtures

    source = "\n".join([
        "pub fn production(x: u8) -> u8 {",          # 1
        "    assert!(x < 9);",                       # 2
        "    x",                                     # 3
        "}",                                         # 4
        "#[cfg(any(test, kani))]",                   # 5
        "fn helper(x: u8) {",                        # 6
        "    assert!(x < 3);",                       # 7
        "}",                                         # 8
        "#[cfg(kani)]",                              # 9
        "#[kani::proof]",                            # 10
        "fn harness() {",                            # 11
        "    let x: u8 = kani::any();",              # 12
        "    assert!(x < 3, \"} brace in a literal\");",  # 13
        "    kani::cover!(x == 1);",                 # 14
        "}",                                         # 15
        "",
    ])

    def entry(number: int, name: str, status: str, description: str, line: int | None,
              path: str = "crates/a/src/lib.rs") -> str:
        located = f"\t - Location: {path}:{line}:5 in function harness\n" if line else ""
        return (f"Check {number}: {name}\n\t - Status: {status}\n"
                f"\t - Description: \"{description}\"\n{located}\n")

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "crates/a/src").mkdir(parents=True)
        (root / "crates/a/src/lib.rs").write_text(source)
        code = ProofCode(root)
        assert [code.contains("crates/a/src/lib.rs", n) for n in (2, 5, 7, 11, 13, 15, 16)] == [
            False, True, True, True, True, True, False], "proof-code spans"
        cases = {
            "V1": (entry(1, "harness.cover.1", "SATISFIED", "x == 1", 14), 0),
            "V2 unsatisfiable": (entry(1, "harness.cover.1", "UNSATISFIABLE", "x == 1", 14), 1),
            "V2 unreachable": (entry(1, "harness.cover.1", "UNREACHABLE", "x == 1", 14), 1),
            "V2 undetermined": (entry(1, "harness.cover.1", "UNDETERMINED", "x == 1", 14), 1),
            "V3": (entry(1, "harness.assertion.1", "UNREACHABLE", "} brace in a literal", 13), 1),
            "V4": (entry(1, "helper.assertion.1", "UNREACHABLE", "assertion failed: x < 3", 7), 1),
            "V5": (entry(1, "harness.assertion.2", "UNREACHABLE", DELIBERATE + ": arm", 13), 0),
            "V6": (entry(1, "production.assertion.1", "UNREACHABLE", "assertion failed: x < 9", 2), 0),
            "V7": (entry(1, "harness.arithmetic_overflow.1", "UNREACHABLE", "attempt to add", 13), 0),
            "V8": (entry(1, "harness.assertion.1", "SUCCESS", "assertion failed: x < 3", 13), 0),
            "V9": (entry(1, "core.assertion.1", "UNREACHABLE", "x", 3, "../library/core/x.rs"), 0),
        }
        # V12 uses the actual wrapped Check85 Description shape. SUCCESS is
        # the observed status, not a counterexample; replacing only its status
        # with UNREACHABLE must expose the proof assertion rather than skip it.
        multiline = "assertion failed: observed ==\n(expected_new, expected_bound)"
        cases.update({
            "V12 success": (entry(85, "harness.assertion.7", "SUCCESS", multiline, 13), 0),
            "V12 unreachable": (entry(85, "harness.assertion.7", "UNREACHABLE", multiline, 13), 1),
            "V12 cover": (entry(85, "harness.cover.7", "UNSATISFIABLE", "before ==\nfalse", 14), 1),
            "V12 undetermined": (entry(85, "harness.cover.7", "UNDETERMINED", "before ==\nfalse", 14), 1),
            "V12 deliberate": (entry(85, "harness.assertion.7", "UNREACHABLE", DELIBERATE + "\narm", 13), 0),
            "V12 production": (entry(85, "production.assertion.7", "UNREACHABLE", multiline, 2), 0),
            "V12 spaced name": (entry(85, "helper::<u8, u8>.assertion.7", "UNREACHABLE", multiline, 7), 1),
        })
        for label, (text, count) in cases.items():
            assert len(findings(text, code)) == count, (label, findings(text, code))
        adjacent = cases["V12 success"][0] + cases["V3"][0]
        parsed = checks(adjacent)
        assert [(item.name, item.description, item.location) for item in parsed] == [
            ("harness.assertion.7", multiline, "crates/a/src/lib.rs:13:5 in function harness"),
            ("harness.assertion.1", "} brace in a literal", "crates/a/src/lib.rs:13:5 in function harness"),
        ], "V13 exact Check boundaries and locations"
        assert len(findings(adjacent, code)) == 1, "V13 later unreachable property remains visible"
        unlocated = entry(1, "harness.assertion.1", "SUCCESS", "first", None)
        assert [item.location for item in checks(unlocated + cases["V3"][0])] == [
            "", "crates/a/src/lib.rs:13:5 in function harness",
        ], "V13 optional location cannot borrow the following Check's location"
        incomplete = 'Check 85: harness.assertion.7\n\t - Status: UNREACHABLE\n\t - Description: "unfinished\n'
        for text in ("", "Checking harness harness...\n", incomplete,
                     incomplete + cases["V8"][0], "Check bad header\n" + cases["V8"][0],
                     cases["V3"][0].rstrip("\n"),
                     "VERIFICATION:- SUCCESSFUL\n ** 2 of 2 cover properties satisfied\n",
                     "VERIFICATION:- SUCCESSFUL\n ** 1 of 2 cover properties satisfied\n"):
            try:
                checks(text)
            except ValueError:
                pass
            else:
                raise AssertionError("V14 incomplete input cannot become a vacuity pass")
        assert checks(cases["V12 success"][0])[0].status == "SUCCESS", "V14 inspect only the completed property"
        coloured = entry(1, "harness.cover.1", "\x1b[33mUNSATISFIABLE\x1b[0m", "x == 1", 14)
        assert len(findings(coloured, code)) == 1, "V11"
        assert cover_shortfall(" ** 2 of 2 cover properties satisfied\n") is None, "V10"
        assert cover_shortfall(" ** 1 of 2 cover properties satisfied (1 unreachable)\n") == \
            "1 of 2 cover properties satisfied", "V10"
        results = root / "result_output_dir"
        results.mkdir()
        (results / "a__harness").write_text(cases["V1"][0] + cases["V6"][0])
        assert check([results], None, root) == 0, "a clean result directory passes"
        (results / "a__vacuous").write_text(cases["V3"][0])
        kept = root / "kept"
        assert check([results], kept, root) == 1, "one vacuous harness refuses the run"
        assert sorted(path.name for path in kept.iterdir()) == ["a__vacuous"], "--keep holds the evidence"
        assert (kept / "a__vacuous").read_bytes() == (results / "a__vacuous").read_bytes(), "retained evidence preserves exact native bytes"
        unfinished = root / "unfinished"
        unfinished.write_text(incomplete + cases["V8"][0])
        assert check([unfinished], kept, root) == 1, "V14 incomplete Check refuses even beside a valid Check"
        assert (kept / "unfinished").read_text() == unfinished.read_text(), "V14 keep exact unjudgeable raw"
        (root / "empty").mkdir()
        assert check([root / "empty"], None, root) == 1, "no results at all is not a pass"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    commands = parser.add_subparsers(dest="command")
    checking = commands.add_parser("check", help="refuse vacuous properties in Kani results")
    checking.add_argument("paths", nargs="+", type=Path, help="result directories or files")
    checking.add_argument("--keep", type=Path, help="copy every result with a finding into this directory")
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        self_test()
        print("formal_vacuity self-test passed")
        return 0
    if arguments.command != "check":
        parser.error("choose `check` or --self-test")
    try:
        return check(arguments.paths, arguments.keep)
    except FileNotFoundError as error:
        print(f"formal vacuity: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
