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

Both rules are hard: the baseline of types that predated the check reached
zero and was removed (plan v5 S2).

* `sdk-citation`: every wire type cites its SDK owner or is adapter-private.
* `inline-citation`: an inline type must cite its owning declared SDK type.

Which SDK *events* the adapter projects is not a naming question and is not
checked here: the event-shape conformance test in `awaken-protocol-managed`
compares every committed event against the oracle's extracted shapes.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# The owned corpus and data let `generate_hooks.py` render this checker's
# trigger instead of maintaining it by hand.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/server/awaken-protocol-managed/src/types/**/*.rs",
    "contracts/anthropic-managed/upstream-oracle.generated.json",
)

REPO_ROOT = Path(__file__).resolve().parents[2]
TYPES_ROOT = REPO_ROOT / "crates/server/awaken-protocol-managed/src/types"
ORACLE = REPO_ROOT / "contracts/anthropic-managed/upstream-oracle.generated.json"

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


def check(rows: list[dict]) -> list[str]:
    errors = []
    for row in rows:
        if row["status"] == "inline-uncited":
            errors.append(f"inline-citation: {row['path']}::{row['name']} uncited -> required; "
                          "`SDK: inline` cites no declared SDK type; fix: cite the exact owning SDK type")
        elif row["status"] == "unannotated":
            errors.append(
                f"sdk-citation: {row['path']}::{row['name']} unannotated -> cited; `{row['name']}` cites no SDK type; fix: add the exact "
                "`Beta…` name in backticks, `SDK: inline` with the owning type, or `SDK: none` with a reason"
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
        {"path": "a.rs", "name": "Cited", "status": "cited", "cited": "PageCursor"},
        {"path": "a.rs", "name": "Private", "status": "private", "cited": None},
        {"path": "a.rs", "name": "New", "status": "unannotated", "cited": None},
        {"path": "a.rs", "name": "Inline", "status": "inline-uncited", "cited": None},
    ]
    assert check(rows[:2]) == [], "cited and private types pass"
    errors = check(rows)
    assert len(errors) == 2 and errors[0].startswith("sdk-citation: a.rs::New") \
        and errors[1].startswith("inline-citation: a.rs::Inline"), errors
    print("check_managed_wire_naming self-test: ok")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="check_managed_wire_naming.py", description=__doc__, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        self_test()
        return 0
    oracle = json.loads(ORACLE.read_text(encoding="utf-8"))
    rows = scan(TYPES_ROOT, declared_types(oracle))
    errors = check(rows)
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if errors:
        return 1
    print(f"OK - {len(rows)} Managed wire types each cite their SDK counterpart or are adapter-private.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
