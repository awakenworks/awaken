"""The formal obligation ledger, assembled from its sources.

Every gate that reasons about formal obligations -- the coverage, surface,
feature, lane-reachability and proof-input checkers, the verified-profile
qualification and the release manifest -- consumes one assembled value, the
coverage ledger in its v1 shape:

    {metric, minimum_ratio, verified_profile, obligations: [...]}

Each obligation row carries ``id``, ``formalizable``, ``status``, ``evidence``,
``proof_harnesses`` (Kani harness names), ``exhaustive_tests`` (``<file>::<fn>``
of the ``#[test]``s that enumerate a finite domain; see ``formal_anchors``),
``claim_scope``, ``proof_grade``, ``bounds``,
``tcb_assumptions`` and ``production_entrypoints``, and optionally
``run_evidence``: ``"needs_rerun"`` on a ``kernel_proved`` row whose harnesses
have no recorded passing run of their current text. The status states what the
harnesses prove; the marker says that claim still waits for its run
(``formal_inventory.py --obligations`` derives both from the transcripts).

Storage. The assembled value is never committed. It is built from

* ``formal/coverage.toml`` -- the metric, the minimum ratio and the verified
  profile, and
* ``formal/obligations/<prefix>/<name>.toml`` -- one file per obligation. The
  path *is* the id (``<prefix>.<name>``); ``formalizable`` is derived from
  ``claim_scope``; every array holds one value per line and every bound is its
  own ``[[bounds]]`` table, so two branches that touch different obligations,
  or different values of one, merge without conflict.

Which bytes. In a Git work tree the ledger is read from the index (``git grep
--cached``), the same staged truth a pre-commit hook judges and a clean CI
checkout holds; an edit is seen by the gates once it is staged. Outside a
repository -- a checker's fixture tree -- the files on disk are read.

``export`` writes the assembled v1 JSON under ``target/`` for a reader that
wants one file; nothing may commit it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

import formal_anchors
from formal_sources import SourceError, detached_git_environment, source_text

# Input set: the profile, every obligation file, the Rust sources and Cargo
# manifests whose anchors name the harness of each obligation, and the Web
# sources whose opening lines anchor evidence. Each gate that loads the
# ledger spells the same globs in its own INPUT_GLOBS.
INPUT_GLOBS: tuple[str, ...] = (
    "formal/coverage.toml",
    "formal/obligations/**/*",
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "web/src/**/*",
)

# Consumers of these names: ``load``/``files``/``write`` below.
PROFILE_FILE = "formal/coverage.toml"
OBLIGATION_DIRECTORY = "formal/obligations"

OBLIGATION_PATH = re.compile(
    r"formal/obligations/(?P<prefix>[a-z0-9_]+)/(?P<name>[a-z0-9_]+)\.toml"
)
SCALAR_FIELDS = ("claim_scope", "proof_grade", "status", "run_evidence")
# The one value ``run_evidence`` may hold; consumers: ``_obligation`` and
# check_formal_coverage.py (the marker is valid only on a kernel_proved row).
NEEDS_RERUN = "needs_rerun"
# Stored in the obligation file. ``proof_harnesses`` is not: it is derived from
# the `/// @obligation` anchors, and so is the evidence entry for each harness
# file. Every array is a set; the assembled ledger lists it sorted.
ARRAY_FIELDS = ("tcb_assumptions", "evidence", "production_entrypoints")
SET_FIELDS = (*ARRAY_FIELDS, "proof_harnesses", "exhaustive_tests")
PROFILE_KEYS = ("metric", "minimum_ratio", "verified_profile")


class LedgerError(ValueError):
    """The stored ledger cannot be assembled into the v1 shape."""


# --------------------------------------------------------------------------
# Assembly.


def _string_list(value: object, where: str) -> list[str]:
    if not isinstance(value, list) or not all(isinstance(item, str) and item for item in value):
        raise LedgerError(f"{where} must be a list of non-empty strings")
    if len(value) != len(set(value)):
        raise LedgerError(f"{where} repeats a value")
    return list(value)


def _obligation(relative: str, text: str) -> dict:
    match = OBLIGATION_PATH.fullmatch(relative)
    if match is None:
        raise LedgerError(
            f"{relative}: an obligation file is formal/obligations/<prefix>/<name>.toml "
            "in lower_snake_case; its path is its id"
        )
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise LedgerError(f"{relative}: {error}") from error
    unknown = sorted(set(document) - {*SCALAR_FIELDS, *ARRAY_FIELDS, "bounds"})
    if unknown:
        raise LedgerError(f"{relative}: unknown field(s) {', '.join(unknown)}")
    row: dict = {"id": f"{match['prefix']}.{match['name']}"}
    for key in SCALAR_FIELDS:
        if key in document:
            if not isinstance(document[key], str):
                raise LedgerError(f"{relative}: {key} must be a string")
            row[key] = document[key]
    if row.get("run_evidence", NEEDS_RERUN) != NEEDS_RERUN:
        raise LedgerError(f"{relative}: run_evidence must be {NEEDS_RERUN!r} when present")
    # The ledger's own rule, stated once: only repository-safety claims are
    # formalizable. Storing both would let them disagree.
    row["formalizable"] = row.get("claim_scope") == "repository_safety"
    for key in ARRAY_FIELDS:
        if key in document:
            row[key] = _string_list(document[key], f"{relative}: {key}")
    if "bounds" in document:
        tables = document["bounds"]
        if not isinstance(tables, list) or not all(
            isinstance(table, dict) and set(table) == {"text"} and isinstance(table["text"], str)
            for table in tables
        ):
            raise LedgerError(f"{relative}: every [[bounds]] table holds exactly one text string")
        row["bounds"] = [table["text"] for table in tables]
    return row


def _profile(text: str | None) -> dict:
    if text is None:
        return {}
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise LedgerError(f"{PROFILE_FILE}: {error}") from error
    unknown = sorted(set(document) - set(PROFILE_KEYS))
    if unknown:
        raise LedgerError(f"{PROFILE_FILE}: unknown field(s) {', '.join(unknown)}")
    return document


def present(root: Path) -> bool:
    """Does ``root`` carry an obligation ledger at all?"""

    return bool(source_text(root, f"{OBLIGATION_DIRECTORY}/**/*")) or bool(
        source_text(root, PROFILE_FILE)
    )


@dataclass
class Derived:
    """What the source anchors state about each obligation id.

    Consumers: ``load`` (adds it to the stored rows) and ``files`` (leaves it
    out of the stored rows, so a fixture round-trips).
    """

    harnesses: dict[str, set[str]] = field(default_factory=dict)
    tests: dict[str, set[str]] = field(default_factory=dict)
    evidence: dict[str, set[str]] = field(default_factory=dict)
    entrypoints: dict[str, set[str]] = field(default_factory=dict)

    def add(self, table: dict[str, set[str]], identifier: str, value: str) -> None:
        table.setdefault(identifier, set()).add(value)


def _derived(root: Path, rows: dict[str, dict] | None) -> Derived:
    """Harness names, evidence and entrypoints stated by the anchors under ``root``.

    With ``rows`` (the stored obligations) every orphan is refused by source
    location: an anchor outside a harness block or a file head, an anchor
    naming no obligation file, a harness serving no obligation, a repeated or
    redundant anchor, and two harnesses with one name. Without ``rows`` the
    anchors are read as they are (``files`` storing a fixture).
    """

    scan = formal_anchors.scan(root)
    headers, header_errors = formal_anchors.headers(root)
    errors = list(scan.errors) + header_errors
    errors += [
        f"{anchor.where}: @{anchor.tag} is not in the attribute block of a #[kani::proof] fn "
        "(dangling anchor)"
        for anchor in scan.dangling
    ]
    derived = Derived()
    defined: dict[str, formal_anchors.Harness] = {}

    def known(identifier: str, where: str, anchor: str) -> bool:
        if rows is None or identifier in rows:
            return True
        prefix, _, name = identifier.partition(".")
        errors.append(
            f"{where}: {anchor} {identifier!r} names no obligation file "
            f"{OBLIGATION_DIRECTORY}/{prefix}/{name}.toml"
        )
        return False

    for harness in scan.harnesses:
        if harness.name in defined:
            errors.append(
                f"{harness.where}: Kani harness {harness.name} is also defined at "
                f"{defined[harness.name].where}; a harness name is its identity"
            )
        defined.setdefault(harness.name, harness)
        served = harness.values("obligation")
        if not served:
            errors.append(
                f"{harness.where}: Kani harness {harness.name} serves no obligation; "
                "add `/// @obligation <prefix>.<name>` beside its #[kani::proof]"
            )
        seen: set[str] = set()
        for anchor in served:
            if anchor.value in seen:
                errors.append(f"{anchor.where}: duplicate @obligation {anchor.value} on {harness.name}")
            seen.add(anchor.value)
            if known(anchor.value, anchor.where, "@obligation"):
                derived.add(derived.harnesses, anchor.value, harness.name)
                derived.add(derived.evidence, anchor.value, harness.path)
    tested: dict[str, formal_anchors.Harness] = {}
    for test in scan.tests:
        identity = f"{test.path}::{test.name}"
        if identity in tested:
            errors.append(f"{test.where}: exhaustive test {identity} is also defined at {tested[identity].where}")
        tested.setdefault(identity, test)
        seen = set()
        for anchor in test.values("obligation"):
            if anchor.value in seen:
                errors.append(f"{anchor.where}: duplicate @obligation {anchor.value} on {test.name}")
            seen.add(anchor.value)
            if known(anchor.value, anchor.where, "@obligation"):
                derived.add(derived.tests, anchor.value, identity)
                derived.add(derived.evidence, anchor.value, test.path)
    harness_files = {key: set(value) for key, value in derived.evidence.items()}
    stated: dict[tuple[str, str], formal_anchors.Header] = {}
    for header in headers:
        if header.role not in ("entrypoint", "evidence"):
            continue
        key = (header.path, header.value)
        if key in stated:
            errors.append(
                f"{header.where}: {header.value} is already anchored at {stated[key].where} "
                "(one line per obligation; an entrypoint is evidence too)"
            )
            continue
        stated[key] = header
        if header.role == "evidence" and header.path in harness_files.get(header.value, set()):
            errors.append(
                f"{header.where}: evidence {header.value} is already stated by this file's "
                "`/// @obligation` harness anchor"
            )
            continue
        if known(header.value, header.where, f"@formal {header.role}"):
            derived.add(derived.evidence, header.value, header.path)
            if header.role == "entrypoint":
                derived.add(derived.entrypoints, header.value, header.path)
    if errors:
        raise LedgerError("\n".join(sorted(set(errors))))
    return derived


def _source_anchored_in_storage(relative: str, row: dict) -> list[str]:
    """The ratchet: an obligation file may not name a Rust or Web source."""

    return [
        f"{relative}: {key} names {path}, a source that states its own role; "
        f"write `// @formal {'entrypoint' if key == 'production_entrypoints' else 'evidence'} "
        f"{row['id']}` as an opening line of {path} instead"
        for key in ("evidence", "production_entrypoints")
        for path in row.get(key, [])
        if formal_anchors.SOURCE_ANCHORED.fullmatch(path)
    ]


def load(root: Path) -> dict:
    """Assemble the coverage ledger of ``root`` in its v1 shape."""

    try:
        profile = source_text(root, PROFILE_FILE)
        stored = source_text(root, f"{OBLIGATION_DIRECTORY}/**/*")
        ledger = _profile(profile.get(PROFILE_FILE))
        rows = {
            row["id"]: row
            for row in (_obligation(relative, text) for relative, text in sorted(stored.items()))
        }
        derived = _derived(root, rows)
    except SourceError as error:
        raise LedgerError(str(error)) from error
    errors: list[str] = []
    for identifier, row in rows.items():
        relative = f"{OBLIGATION_DIRECTORY}/{identifier.replace('.', '/', 1)}.toml"
        errors += _source_anchored_in_storage(relative, row)
        if identifier in derived.harnesses:
            row["proof_harnesses"] = sorted(derived.harnesses[identifier])
        if identifier in derived.tests:
            row["exhaustive_tests"] = sorted(derived.tests[identifier])
        if not row.get("proof_harnesses") and not row.get("exhaustive_tests") and (
            row.get("proof_grade") == "production_kernel_proved"
        ):
            errors.append(
                f"{relative}: {identifier} production-kernel grade has no Kani harness or exhaustive test; "
                f"anchor one with `/// @obligation {identifier}`"
            )
        for key, table in (("evidence", derived.evidence), ("production_entrypoints", derived.entrypoints)):
            if identifier in table:
                row[key] = sorted(set(row.get(key, [])) | table[identifier])
        for key in ARRAY_FIELDS:
            if key in row:
                row[key] = sorted(row[key])
    if errors:
        raise LedgerError("\n".join(sorted(errors)))
    ledger["obligations"] = list(rows.values())
    return ledger


# --------------------------------------------------------------------------
# Writing (fixtures, the migration) and digests.


def _basic(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def _bound(text: str) -> str:
    literal_safe = (
        "'''" not in text
        and not text.startswith("\n")
        and not text.endswith("'''")
        and all(ord(character) >= 32 or character == "\t" for character in text)
    )
    return f"'''{text}'''" if literal_safe else _basic(text)


def render_obligation(row: dict) -> str:
    lines: list[str] = []
    for key in SCALAR_FIELDS:
        if key in row:
            lines.append(f"{key} = {_basic(row[key])}")
    for key in ARRAY_FIELDS:
        if key not in row:
            continue
        if not row[key]:
            lines.append(f"{key} = []")
            continue
        lines.append(f"{key} = [")
        lines.extend(f"  {_basic(value)}," for value in row[key])
        lines.append("]")
    for text in row.get("bounds", []):
        lines.extend(["", "[[bounds]]", f"text = {_bound(text)}"])
    return "\n".join(lines) + "\n"


def render_profile(ledger: dict) -> str:
    lines: list[str] = []
    if "metric" in ledger:
        lines.append(f"metric = {_basic(ledger['metric'])}")
    if "minimum_ratio" in ledger:
        lines.append(f"minimum_ratio = {json.dumps(ledger['minimum_ratio'])}")
    profile = ledger.get("verified_profile")
    if profile is not None:
        lines.extend(["", "[verified_profile]"])
        for key, value in profile.items():
            if isinstance(value, list):
                lines.append(f"{key} = [")
                lines.extend(f"  {_basic(item)}," for item in value)
                lines.append("]")
            elif isinstance(value, str):
                lines.append(f"{key} = {_basic(value)}")
            else:
                lines.append(f"{key} = {json.dumps(value)}")
    return "\n".join(lines) + "\n"


def files(ledger: dict, root: Path | None = None) -> dict[str, str]:
    """The stored form of an assembled ledger: repository path -> text.

    What the anchors under ``root`` already state -- each obligation's harness
    names, harness files, and the evidence and entrypoints its sources claim in
    their opening lines -- is left out, since ``load`` derives it. A fact the
    storage may not hold (harness names that differ from the anchors, a Rust or
    Web source the anchors do not state) is refused: its anchor is where it is
    written.
    """

    derived = _derived(root, None) if root is not None else Derived()
    stored = {PROFILE_FILE: render_profile(ledger)}
    for row in ledger["obligations"]:
        prefix, dot, name = str(row["id"]).partition(".")
        relative = f"{OBLIGATION_DIRECTORY}/{prefix}/{name}.toml"
        if not dot or OBLIGATION_PATH.fullmatch(relative) is None:
            raise LedgerError(f"obligation id {row['id']!r} is not <prefix>.<name> in lower_snake_case")
        if relative in stored:
            raise LedgerError(f"duplicate obligation id {row['id']}")
        declared = set(row.get("proof_harnesses", []))
        if declared != derived.harnesses.get(row["id"], set()):
            raise LedgerError(
                f"{row['id']}: proof_harnesses {sorted(declared)} differ from the "
                f"`/// @obligation` anchors {sorted(derived.harnesses.get(row['id'], set()))}; edit the anchors"
            )
        tests = set(row.get("exhaustive_tests", []))
        if tests != derived.tests.get(row["id"], set()):
            raise LedgerError(
                f"{row['id']}: exhaustive_tests {sorted(tests)} differ from the "
                f"`/// @obligation` anchors {sorted(derived.tests.get(row['id'], set()))}; edit the anchors"
            )
        kept = dict(row)
        kept.pop("proof_harnesses", None)
        kept.pop("exhaustive_tests", None)
        for key, table in (("evidence", derived.evidence), ("production_entrypoints", derived.entrypoints)):
            if key in kept:
                kept[key] = [path for path in kept[key] if path not in table.get(row["id"], set())]
        refused = _source_anchored_in_storage(relative, kept)
        if refused:
            raise LedgerError("\n".join(refused))
        stored[relative] = render_obligation(kept)
    return stored


def write(root: Path, ledger: dict) -> None:
    """Replace the stored ledger under ``root`` with ``ledger`` (v1 shape).

    Consumers: checker self-tests and mutation cases that build a fixture tree,
    so a fixture follows the storage instead of hard-coding it.
    """

    stored = files(ledger, root)
    directory = root / OBLIGATION_DIRECTORY
    if directory.is_dir():
        for path in sorted(directory.glob("**/*.toml")):
            path.unlink()
    for relative, text in stored.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def canonical_bytes(ledger: dict) -> bytes:
    """Order-independent bytes of an assembled ledger.

    Obligations are ordered by id and every set-valued array is sorted; the
    bounds keep their order, because the order of a bound's text is declared.
    """

    ordered = dict(ledger)
    ordered["obligations"] = sorted(
        (
            {key: sorted(value) if key in SET_FIELDS else value for key, value in row.items()}
            for row in ledger["obligations"]
        ),
        key=lambda row: row["id"],
    )
    return (json.dumps(ordered, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def digest(ledger: dict) -> str:
    return "sha256:" + hashlib.sha256(canonical_bytes(ledger)).hexdigest()


def self_test() -> None:
    """Decision table for the storage, the anchors and the reader.

    Storage causes: C1 a ledger is written and read back; C2 an obligation
    file sits at another depth or spelling; C3 it names an unknown field
    (``proof_harnesses`` included: anchors own it); C4 an array repeats a
    value; C5 two rows claim one id; C6 claim_scope decides formalizable.
    Anchor causes: A1 an anchor outside a harness block (dangling); A2 an
    anchor naming no obligation file; A3 a harness with no anchor; A4 one
    harness name defined twice, or one anchor repeated; A5 a
    production-kernel obligation no harness serves; A6 a row whose
    proof_harnesses disagree with the anchors is stored.
    Exhaustive-test causes: T1 a `#[test]` with `@exhaustive <domain>` is the
    only holder of a production-kernel obligation; T2 a test names an
    obligation without stating its finite domain; T3 `@exhaustive` names no
    obligation; T4 `@exhaustive` sits on a Kani harness; T5 `@kani` sits on a
    test.
    Header causes: H1 a `// @formal` anchor after code; H2 one naming no
    obligation file; H3 one repeated; H4 evidence already stated by the
    file's harness anchor; H5 an unknown role; H6 an obligation file naming
    a Rust or Web source (the ratchet), or a fixture storing one.
    Reader causes: C7 in a repository the staged and the on-disk bytes
    differ; C8 the same tree is not a repository; C9 a hook exported this
    repository with the index being committed; C10 a hook exported another.
    Marker cause: C11 ``run_evidence`` is ``needs_rerun`` (read and stored)
    or another value (E2).
    Effects: E1 load returns what was written, harness names and harness
    files derived from the anchors; E2 refusal naming the file (and line);
    E3 load reads the staged bytes; E4 load reads disk; E5 the hook's index.
    Rules: C1 => E1; T1 => E1 with exhaustive_tests; C2|C3|C4|C5|A1..A6|H1..H6|T2..T5 => E2; C6 => formalizable ==
    (scope == repository_safety); C7 => E3; C8 => E4; C9 => E5; C10 => E3.
    """

    import os  # noqa: PLC0415 - only the self-test builds fixtures
    import shutil  # noqa: PLC0415
    import tempfile  # noqa: PLC0415
    from unittest import mock  # noqa: PLC0415

    header = "// @formal entrypoint run.safe\n"
    harness_source = header + (
        "#[cfg(kani)]\n/// @obligation run.safe\n#[kani::proof]\n#[kani::unwind(2)]\nfn safe() {}\n"
    )
    rows = [
        {"id": "run.safe", "claim_scope": "repository_safety", "proof_grade": "production_kernel_proved",
         "status": "kernel_proved", "tcb_assumptions": [], "evidence": ["crates/a/src/lib.rs"],
         "proof_harnesses": ["safe"], "production_entrypoints": ["crates/a/src/lib.rs"],
         "bounds": ["one ' quote and a trailing quote'", "two"]},
        {"id": "run.external", "claim_scope": "external_assumption", "proof_grade": "external_tcb",
         "status": "external", "tcb_assumptions": ["external.x"], "evidence": ["docs/x.md"],
         "production_entrypoints": [], "bounds": ["text with " + "'" * 3 + "triple quotes"]},
    ]
    ledger = {"metric": "m", "minimum_ratio": 0.7, "verified_profile": {"schema_version": 1, "id": "v"},
              "obligations": rows}
    expected = {**ledger, "obligations": [
        {**row, "formalizable": row["claim_scope"] == "repository_safety"} for row in rows
    ]}

    def refused(root: Path, fragment: str) -> None:
        try:
            load(root)
        except LedgerError as error:
            assert fragment in str(error), (fragment, str(error))
        else:
            raise AssertionError(f"load accepted a ledger that must be refused: {fragment}")

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = root / "crates/a/src/lib.rs"
        source.parent.mkdir(parents=True)
        (root / "crates/a/Cargo.toml").write_text('[package]\nname = "a"\nversion = "0.0.0"\n')
        source.write_text(harness_source)
        write(root, ledger)
        safe = root / OBLIGATION_DIRECTORY / "run" / "safe.toml"
        original = safe.read_text()
        assert "proof_harnesses" not in original and "crates/a/src/lib.rs" not in original, (
            "C1 derived facts are not stored")
        assert canonical_bytes(load(root)) == canonical_bytes(expected), "C1/E1 and C6"
        assert present(root) and not present(root / "formal"), "presence"
        stray = root / OBLIGATION_DIRECTORY / "run" / "deep" / "x.toml"
        stray.parent.mkdir()
        stray.write_text('status = "external"\n')
        refused(root, "formal/obligations/run/deep/x.toml")
        stray.unlink()
        upper = root / OBLIGATION_DIRECTORY / "run" / "Upper.toml"
        upper.write_text('status = "external"\n')
        refused(root, "run/Upper.toml")
        upper.unlink()
        for field in ("formalizable = true\n", 'proof_harnesses = ["safe"]\n'):
            safe.write_text(field + original)
            refused(root, f"unknown field(s) {field.split(' ')[0]}")
        safe.write_text(original.replace("tcb_assumptions = []", 'tcb_assumptions = ["x", "x"]', 1))
        refused(root, "tcb_assumptions repeats a value")
        safe.write_text('run_evidence = "passed"\n' + original)
        refused(root, "run_evidence must be 'needs_rerun' when present")
        safe.write_text('run_evidence = "needs_rerun"\n' + original)
        assert load(root)["obligations"][1]["run_evidence"] == NEEDS_RERUN, "C11 the marker is read"
        marked = {**ledger, "obligations": [dict(rows[0], run_evidence=NEEDS_RERUN), rows[1]]}
        assert 'run_evidence = "needs_rerun"' in files(marked, root)[safe.relative_to(root).as_posix()], (
            "C11 the marker is stored")
        for stored_path in ("crates/a/src/other.rs", "web/src/x.ts"):
            safe.write_text(original.replace("evidence = []", f'evidence = ["{stored_path}"]', 1))
            refused(root, f"names {stored_path}, a source that states its own role")
        safe.write_text(original)
        for broken, fragment in (
            ({"obligations": [rows[0], dict(rows[0])]}, "duplicate obligation id run.safe"),
            ({"obligations": [dict(rows[0], proof_harnesses=["safe", "other"])]}, "differ from the"),
            ({"obligations": [dict(rows[1], evidence=["crates/a/src/other.rs"])]}, "states its own role"),
        ):
            try:
                files(broken, root)
            except LedgerError as error:
                assert fragment in str(error), ("C5/A6", str(error))
            else:
                raise AssertionError(f"C5/A6 stored an inexpressible ledger: {fragment}")

        for text, fragment in (
            (harness_source + "/// @obligation run.safe\nfn helper() {}\n",
             "crates/a/src/lib.rs:7: @obligation is not in the attribute block"),
            (harness_source.replace("/// @obligation run.safe", "/// @obligation run.missing"),
             "crates/a/src/lib.rs:3: @obligation 'run.missing' names no obligation file "
             "formal/obligations/run/missing.toml"),
            (harness_source.replace("/// @obligation run.safe\n", "/// A proof.\n"),
             "crates/a/src/lib.rs:6: Kani harness safe serves no obligation"),
            (harness_source.replace("/// @obligation run.safe\n", "/// @obligation run.safe\n" * 2),
             "crates/a/src/lib.rs:4: duplicate @obligation run.safe on safe"),
            (harness_source + header,
             "crates/a/src/lib.rs:7: a @formal anchor belongs in the file's opening lines"),
            ("// @formal evidence run.missing\n" + harness_source,
             "crates/a/src/lib.rs:1: @formal evidence 'run.missing' names no obligation file"),
            (header + harness_source, "crates/a/src/lib.rs:2: run.safe is already anchored at crates/a/src/lib.rs:1"),
            (harness_source.replace(header, "// @formal evidence run.safe\n"),
             "crates/a/src/lib.rs:1: evidence run.safe is already stated by this file's"),
            ("// @formal proof run.safe\n" + harness_source, "crates/a/src/lib.rs:1: expected `// @formal <"),
        ):
            source.write_text(text)
            refused(root, fragment)
        source.write_text(harness_source)
        twin = root / "crates/a/src/twin.rs"
        twin.write_text(harness_source)
        refused(root, "crates/a/src/twin.rs:6: Kani harness safe is also defined at crates/a/src/lib.rs:6")
        twin.unlink()
        source.write_text("")
        refused(root, "run.safe production-kernel grade has no Kani harness")
        test_source = header + "/// @exhaustive bool(2) = 2\n/// @obligation run.safe\n#[test]\nfn safe_table() {}\n"
        source.write_text(test_source)
        held = {row["id"]: row for row in load(root)["obligations"]}["run.safe"]
        assert held["exhaustive_tests"] == ["crates/a/src/lib.rs::safe_table"], ("T1", held)
        assert "proof_harnesses" not in held and "crates/a/src/lib.rs" in held["evidence"], ("T1", held)
        for text, fragment in (
            (test_source.replace("/// @exhaustive bool(2) = 2\n", ""),
             "crates/a/src/lib.rs:4: test safe_table serves an obligation only as an exhaustive proof"),
            (test_source.replace("/// @obligation run.safe\n", ""),
             "crates/a/src/lib.rs:2: exhaustive test safe_table serves no obligation"),
            (harness_source.replace("#[kani::proof]", "/// @exhaustive bool(2) = 2\n#[kani::proof]"),
             "crates/a/src/lib.rs:4: @exhaustive marks a #[test]"),
            (test_source.replace("#[test]", "/// @kani tier=nightly\n#[test]"),
             "crates/a/src/lib.rs:4: @kani is not in the attribute block"),
        ):
            source.write_text(text)
            refused(root, fragment)
        source.write_text(harness_source)

        git = ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
               "-c", "core.hooksPath=/dev/null"]
        # A hook exports its own repository; the fixture must never touch it.
        detached = detached_git_environment()
        for arguments in (["init", "--quiet"], ["add", "."], ["commit", "--quiet", "-m", "fixture"]):
            subprocess.run([*git, *arguments], cwd=root, check=True, capture_output=True, env=detached)
        safe.write_text(original.replace('status = "kernel_proved"', 'status = "modeled_only"'))
        source.write_text("")
        staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "kernel_proved", "C7/E3 unstaged bytes are not the ledger"
        assert staged["run.safe"]["proof_harnesses"] == ["safe"], "C7/E3 unstaged anchors are not the ledger"
        source.write_text(harness_source)
        subprocess.run([*git, "add", "."], cwd=root, check=True, capture_output=True, env=detached)
        staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "modeled_only", "C7/E3 staged bytes are the ledger"
        # C9 a hook exports this repository and the separate index being
        # committed (`git commit <path>`): that index is the staged truth.
        git_dir = root / ".git"
        partial = git_dir / "partial-index"
        subprocess.run([*git, "read-tree", "HEAD"], cwd=root, check=True, capture_output=True,
                       env={**detached, "GIT_INDEX_FILE": str(partial)})
        with mock.patch.dict(os.environ, {"GIT_DIR": str(git_dir), "GIT_INDEX_FILE": str(partial)}):
            staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "kernel_proved", "C9 the hook's own index"
        # C10 a hook exports another repository: this tree keeps its own index.
        with mock.patch.dict(os.environ, {"GIT_DIR": str(root / "elsewhere.git"), "GIT_INDEX_FILE": str(partial)}):
            staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "modeled_only", "C10 a foreign hook repository"
        # Git objects are read-only on Windows; the fixture owns this tree.
        assert git_dir.resolve().parent == root.resolve()
        if os.name == "nt":
            for path in git_dir.rglob("*"):
                if path.is_file():
                    path.chmod(path.stat().st_mode | 0o200)
        shutil.rmtree(git_dir)
        safe.write_text(original)
        on_disk = {row["id"]: row for row in load(root)["obligations"]}
        assert on_disk["run.safe"]["status"] == "kernel_proved", "C8/E4"
    print("formal_ledger self-test: the decision table holds")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true", required=True, help="run the decision table")
    parser.parse_args(argv)
    self_test()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
