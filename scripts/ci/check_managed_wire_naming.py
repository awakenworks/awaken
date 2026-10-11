#!/usr/bin/env python3
"""Every Managed wire type names its official SDK counterpart.

`crates/server/awaken-protocol-managed/src/types` holds the native Managed
Agents transfer objects. Its module doc states the naming convention: Rust
names are idiomatic and namespaced by resource module, and the exact SDK type
is cited in backticks in each type's doc so the correspondence is one `rg`
away. This check makes that convention a fact about the tree instead of a
sentence in a doc:

- A wire type's doc must cite, in backticks, a name the current SDK oracle
  declares (`contracts/anthropic-managed/upstream-oracle.generated.json`,
  `current.declared_types`); or
- declare `SDK: inline` and cite the owning SDK type whose anonymous member it
  mirrors (`` `BetaManagedAgentsAgentToolUseEvent.evaluated_permission` ``); or
- declare `SDK: none` with the reason it is adapter-private.

Types that predate the check and carry none of the three are listed in the
baseline (`crates/server/awaken-protocol-managed/contracts/wire-naming-baseline.json`).
The baseline is a ratchet: it can only shrink. A new unannotated type fails.
Default/`--check` is read-only. `--tighten` and `--write-baseline` record only
decreases; even `--reason` cannot admit a new unannotated or inline-uncited type.
An explicit first write requires zero unannotated types and all hard rules to
pass. Existing root and retained-entry metadata stay with their decisions.

* `sdk-citation`: every new wire type cites its SDK owner or is adapter-private.
* `inline-citation`: an inline type must cite its owning declared SDK type.
* `stale-wire-debt`: an annotated or removed held type leaves the baseline.

Which SDK *events* the adapter projects is not a naming question and is not
checked here: the event-shape conformance test in `awaken-protocol-managed`
compares every committed event against the oracle's extracted shapes.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

import _architecture_ratchet

# The owned corpus, data and CLI/test dependencies below let `generate_hooks.py`
# render this checker's trigger instead of maintaining it by hand. This keeps
# the hook from drifting narrower than what the checker actually reads.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/server/awaken-protocol-managed/src/types/**/*.rs",
    "crates/server/awaken-protocol-managed/contracts/wire-naming-baseline.json",
    "contracts/anthropic-managed/upstream-oracle.generated.json",
    "scripts/ci/*.py",
    "lefthook.yml",
)

REPO_ROOT = Path(__file__).resolve().parents[2]
TYPES_ROOT = REPO_ROOT / "crates/server/awaken-protocol-managed/src/types"
ORACLE = REPO_ROOT / "contracts/anthropic-managed/upstream-oracle.generated.json"
BASELINE = REPO_ROOT / "crates/server/awaken-protocol-managed/contracts/wire-naming-baseline.json"

TYPE_RE = re.compile(
    r"((?:^[ \t]*///[^\n]*\n)*)"  # doc block
    r"((?:^[ \t]*#\[[^\n]*\]\n)*)"  # attributes
    r"^[ \t]*pub(?:\(crate\)|\(super\))? (?:struct|enum) (\w+)",
    re.MULTILINE,
)
# A citation names one SDK type, optionally with its generic parameters
# (`PageCursor<T>`) or one member (`Event.evaluated_permission`). The captured
# name is the outer type: `Vec<PageCursor>` cites `Vec`, not `PageCursor`.
CITATION_RE = re.compile(r"`([A-Za-z][A-Za-z0-9_]*)(?:<[A-Za-z0-9_, ]*>)?(?:\.[a-z_]+)?`")


def declared_types(oracle: dict) -> set[str]:
    names = oracle.get("current", {}).get("declared_types")
    if not isinstance(names, list) or not names:
        raise SystemExit("oracle has no current.declared_types; regenerate the Managed SDK oracle")
    return set(names)


def classify(doc: str, declared: set[str]) -> tuple[str, str | None]:
    """Return (status, citation) for one type's doc block."""
    if "SDK: none" in doc:
        return "private", None
    if "SDK: inline" in doc:
        for cited in CITATION_RE.findall(doc):
            if cited in declared:
                return "inline", cited
        return "inline-uncited", None
    for cited in CITATION_RE.findall(doc):
        if cited in declared:
            return "cited", cited
    return "unannotated", None


def scan(types_root: Path, declared: set[str]) -> list[dict]:
    rows = []
    for path in sorted(types_root.rglob("*.rs")):
        source = path.read_text(encoding="utf-8")
        for match in TYPE_RE.finditer(source):
            status, cited = classify(match.group(1), declared)
            rows.append(
                {
                    "path": path.relative_to(REPO_ROOT).as_posix(),
                    "name": match.group(3),
                    "status": status,
                    "cited": cited,
                }
            )
    return rows


def check(rows: list[dict], baseline: dict) -> list[str]:
    # The original checker required this record (indexed lookup). An existing
    # document without it is not an initialized debt record; explicit zero
    # initialization still builds the required key before candidate checking.
    if "unannotated" not in baseline:
        return ["stale-wire-debt: baseline record missing -> required; "
                "fix: restore the owning wire-naming-baseline.json record"]
    errors = []
    grandfathered = {(entry["path"], entry["name"]) for entry in baseline["unannotated"]}
    seen = set()
    for row in rows:
        key = (row["path"], row["name"])
        if row["status"] == "inline-uncited":
            errors.append(f"inline-citation: {row['path']}::{row['name']} uncited -> required; "
                          "`SDK: inline` cites no declared SDK type; fix: cite the exact owning SDK type")
        elif row["status"] == "unannotated":
            seen.add(key)
            if key not in grandfathered:
                errors.append(
                    f"sdk-citation: {row['path']}::{row['name']} unannotated -> cited; `{row['name']}` cites no SDK type; fix: add the exact "
                    "`Beta…` name in backticks, `SDK: inline` with the owning type, or `SDK: none` with a reason"
                )
    stale = grandfathered - seen
    for path, name in sorted(stale):
        errors.append(
            f"stale-wire-debt: {path}::{name} held -> absent; baseline lists `{name}` but it is now annotated or gone; "
            "the ratchet only shrinks; fix: python3 scripts/ci/check_managed_wire_naming.py --tighten"
        )
    return errors


def self_test() -> None:
    declared = {
        "BetaManagedAgentsAgentToolUseEvent",
        "BetaManagedAgentsAgentToolEvaluation",
        "PageCursor",
    }
    cases = {
        "/// `BetaManagedAgentsAgentToolEvaluation`.\n": ("cited", "BetaManagedAgentsAgentToolEvaluation"),
        "/// SDK: inline — `BetaManagedAgentsAgentToolUseEvent.evaluated_permission`.\n": (
            "inline",
            "BetaManagedAgentsAgentToolUseEvent",
        ),
        "/// SDK: inline — mirrors something.\n": ("inline-uncited", None),
        "/// SDK: none — adapter-private cursor.\n": ("private", None),
        # C3 preserves the original classifier; this CLI migration does not
        # add a new private-reason hard rule that the scanner never owned.
        "/// SDK: none\n": ("private", None),
        "/// A doc that cites `Nothing`.\n": ("unannotated", None),
        # A generic SDK type is cited with its parameters and still names it.
        "/// Anthropic `PageCursor<T>` used by collections.\n": ("cited", "PageCursor"),
        "/// `PageCursor<Item, Extra>`.\n": ("cited", "PageCursor"),
        # Only the outer type is cited: a wrapper does not borrow its argument.
        "/// A `Vec<PageCursor>` of pages.\n": ("unannotated", None),
    }
    for doc, expected in cases.items():
        assert classify(doc, declared) == expected, (doc, classify(doc, declared))
    rows = [
        {"path": "a.rs", "name": "Old", "status": "unannotated", "cited": None},
        {"path": "a.rs", "name": "New", "status": "unannotated", "cited": None},
    ]
    errors = check(rows, {"unannotated": [{"path": "a.rs", "name": "Old"}]})
    assert len(errors) == 1 and "`New`" in errors[0], errors
    errors = check(
        [{"path": "a.rs", "name": "Old", "status": "cited", "cited": "X"}],
        {"unannotated": [{"path": "a.rs", "name": "Old"}]},
    )
    assert len(errors) == 1 and "ratchet" in errors[0], errors
    assert check(rows[:1], {"unannotated": [{"path": "a.rs", "name": "Old"}]}) == []
    _selftest_cli()
    print("check_managed_wire_naming self-test: ok")


def main(argv: list[str] | None = None) -> int:
    def measure():
        oracle = json.loads(ORACLE.read_text(encoding="utf-8"))
        return [], scan(TYPES_ROOT, declared_types(oracle))

    def lower(rows, previous):
        # Only an absent file has first-write authorization. An existing
        # document missing its required record stays malformed, so check must
        # refuse every write mode rather than silently repairing it.
        if BASELINE.exists() and "unannotated" not in previous:
            return previous
        live = {(row["path"], row["name"]) for row in rows if row["status"] == "unannotated"}
        # The original decisions alone may survive. New keys cannot enter the
        # candidate, and check(candidate) therefore still refuses new debt or
        # inline hard errors. Copy whole entries so metadata is never erased.
        return {**previous, "unannotated": [entry for entry in previous.get("unannotated", [])
                if (entry["path"], entry["name"]) in live]}

    return _architecture_ratchet.baseline_cli(
        argv, prog="check_managed_wire_naming.py", description=__doc__, baseline_path=BASELINE,
        self_test=self_test, measure=measure, check=check, tighten=lower,
        write=lambda rows, previous, _reason: (lower(rows, previous), []),
    )


def _selftest_cli() -> None:
    """C1-C6 cause/effect graph, owned by these actual CLI fixture cases.

    Valid argv -> read baseline/oracle -> one real scan -> classifier -> held
    key comparison. Default checks never write; tighten/write keep held live
    entries only and the same check validates the candidate. New unannotated or
    inline-uncited types always refuse, regardless of reason or net decrease.
    Existing metadata survives; a missing baseline can initialize only when
    there is no debt or hard error. Invalid argv refuses before reading/scanning.

    Rule | Causes / constraint                         | Effects
    C1   | held=live / stale, read-only or write        | exact bytes / refuse or shrink
    C2   | new key, including a different held-key fall| hard refusal, no write (reason cannot waive)
    C3   | inline/oracle hard error or missing record  | refusal in all modes, no repair/write
    C4   | retained entries + foreign root/entry data  | whole metadata + original order retained
    C5   | absent file + explicit write + zero valid  | initialize; other modes/debt/hard refuse
    C6   | invalid argv / self-test / valid invocation | no reads / self-test / one input snapshot
    """
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch

    held = {"$comment": "keep exact comment", "owner": "wire-owner", "expires": "later",
            "foreign": {"keep": [1, 2]}, "unannotated": [
                {"path": "types/old.rs", "name": "Old", "owner": "type-owner",
                 "expires": "review", "metadata": {"keep": True}},
            ]}
    original_scan = scan
    original_read = Path.read_text

    def invoke(argv, sources=None, previous=held, oracle=None):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            types = root / "types"
            types.mkdir()
            baseline = root / "baseline.json"
            if previous is not None:
                baseline.write_text(json.dumps(previous) + "\n")
            upstream = root / "oracle.json"
            upstream.write_text(json.dumps(oracle if oracle is not None else {
                "current": {"declared_types": ["SdkOwner"]}}))
            for name, text in (sources if sources is not None else {"old.rs": "pub struct Old;\n"}).items():
                (types / name).write_text(text)
            before = baseline.read_bytes() if baseline.exists() else None
            observed, tests, read_paths = [], [], []
            def read(path, *args, **kwargs):
                if path == baseline or path == upstream or types in path.parents:
                    read_paths.append(path)
                return original_read(path, *args, **kwargs)
            def counted(types_root, declared):
                observed.append(types_root)
                return original_scan(types_root, declared)
            stdout, stderr = io.StringIO(), io.StringIO()
            with patch.dict(main.__globals__, {"REPO_ROOT": root, "TYPES_ROOT": types,
                    "ORACLE": upstream, "BASELINE": baseline, "scan": counted,
                    "self_test": lambda: tests.append(True)}), \
                    patch.object(Path, "read_text", read), \
                    contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                try:
                    status = main(argv)
                except SystemExit as error:
                    status = error.code if isinstance(error.code, int) else 1
            after = baseline.read_bytes() if baseline.exists() else None
            assert len(observed) <= 1, "C6 one real scan, no writer rescan"
            if status == 2 or tests:
                assert not read_paths, "C6 parser/self-test precedes baseline, oracle and source reads"
            elif observed:
                assert read_paths.count(upstream) == 1, "C6 one oracle snapshot"
                assert read_paths.count(baseline) == (previous is not None), "C6 one existing baseline snapshot"
                source_reads = [path for path in read_paths if types in path.parents]
                assert len(source_reads) == len(set(source_reads)), "C6 each actual source read once"
            return status, before, after, observed, tests, stdout.getvalue(), stderr.getvalue()

    writes = (["--tighten"], ["--write-baseline"], ["--write-baseline", "--reason", "reviewed"])
    modes = ([], ["--check"], *writes)
    for argv in modes:
        status, before, after, reads, _, _, _ = invoke(argv)
        assert status == 0 and before == after and len(reads) == 1, "C1 equal: exact bytes, one scan"
    for argv in ([], ["--check"]):
        status, before, after, _, _, _, errors = invoke(argv, {})
        assert status == 1 and before == after and "--tighten" in errors, "C1 readonly stale"
    for argv in writes:
        status, _, after, _, _, _, _ = invoke(argv, {})
        expected = {**held, "unannotated": []}
        assert status == 0 and json.loads(after) == expected, "C1/C4 shrink retains root metadata"
        extended = {**held, "unannotated": held["unannotated"] + [{"path": "types/gone.rs", "name": "Gone"}]}
        status, _, after, _, _, _, _ = invoke(argv, previous=extended)
        assert status == 0 and json.loads(after) == held, "C4 retain whole entry metadata and order"
        # C4 retains multiple entries in their original, deliberately
        # non-lexicographic order, even though the scanner reads sorted files.
        ordered = {**held, "unannotated": [
            {"path": "types/z.rs", "name": "Z", "owner": "z-owner", "unknown": {"exact": 1}},
            {"path": "types/gone.rs", "name": "Gone", "expiry": "after-docs"},
            {"path": "types/a.rs", "name": "A", "expires": "review", "unknown": [2, 3]},
        ]}
        source_pair = {"a.rs": "pub struct A;\n", "z.rs": "pub struct Z;\n"}
        status, _, after, _, _, _, _ = invoke(argv, source_pair, ordered)
        expected = {**ordered, "unannotated": [ordered["unannotated"][0], ordered["unannotated"][2]]}
        assert status == 0 and json.loads(after) == expected, "C4 whole-entry order after middle removal"
        status, before, after, _, _, _, _ = invoke(argv, source_pair, expected)
        assert status == 0 and before == after, "C4 equal non-lexicographic metadata bytes never rewritten"
    # C2: independent keys do not transfer allowance when an old key vanishes;
    # zero-held and mixed falls both refuse the actual new key in every mode.
    for sources, previous in (({"new.rs": "pub struct New;\n"}, held),
                              ({"new.rs": "pub struct New;\n"}, {**held, "unannotated": []}),
                              ({"old.rs": "pub struct Old;\n", "new.rs": "pub struct New;\n"}, held)):
        for argv in modes:
            status, before, after, _, _, _, errors = invoke(argv, sources, previous)
            assert status == 1 and before == after and "sdk-citation:" in errors, "C2 no new debt, even reasoned write"
    for argv in modes:
        status, before, after, _, _, _, errors = invoke(argv, {"old.rs": "/// SDK: inline without a citation\npub struct Old;\n"})
        assert status == 1 and before == after and "inline-citation:" in errors, "C3 hard failure never writes"
        for oracle in ({}, {"current": {"declared_types": []}}):
            status, before, after, scans, _, _, _ = invoke(argv, oracle=oracle)
            assert status == 1 and before == after and not scans, "C3 missing oracle refuses before scan"
    # C3 an existing malformed record retains the original indexed-checker
    # refusal; absence of the unannotated key is not a zero-debt default pass.
    for previous in ({}, {"owner": "record-owner"}):
        for argv in modes:
            status, before, after, _, _, _, errors = invoke(argv, {}, previous)
            assert status == 1 and before == after and "baseline record missing" in errors, "C3 existing record required in every mode"
    # C5 first initialization permits exactly the already-valid zero-debt tree,
    # never a new allowance. Missing default/tighten still require a baseline.
    for argv in (["--write-baseline"], ["--write-baseline", "--reason", "first record"]):
        status, before, after, _, _, _, _ = invoke(argv, {}, None)
        assert status == 0 and before is None and json.loads(after) == {"unannotated": []}, "C5 zero initialization"
        for sources in ({"new.rs": "pub struct New;\n"}, {"inline.rs": "/// SDK: inline\npub struct Inline;\n"}):
            status, before, after, _, _, _, _ = invoke(argv, sources, None)
            assert status == 1 and before is None and after is None, "C5 new/hard cannot initialize"
    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, scans, _, _, _ = invoke(argv, {}, None)
        assert status == 1 and before is None and after is None and not scans, "C5 require existing baseline"
    for argv in (["--unknown"], ["--self-tes"], ["--write-baselin"], ["--check", "--tighten"],
                 ["--self-test", "--write-baseline"], ["--reason", "why"],
                 ["--tighten", "--reason", "why"], ["--write-baseline", "--reason", "   "]):
        status, before, after, scans, _, _, errors = invoke(argv)
        assert status == 2 and before == after and not scans and "usage:" in errors, "C6 argument admission first"
    status, before, after, scans, tests, _, _ = invoke(["--self-test"])
    assert status == 0 and before == after and not scans and tests == [True], "C6 original self-test standalone"


if __name__ == "__main__":
    sys.exit(main())
