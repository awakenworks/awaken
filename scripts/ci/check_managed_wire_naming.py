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
CITATION_RE = re.compile(r"`([A-Za-z][A-Za-z0-9_]*)(?:\.[a-z_]+)?`")


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
    errors = []
    grandfathered = {(entry["path"], entry["name"]) for entry in baseline["unannotated"]}
    seen = set()
    for row in rows:
        key = (row["path"], row["name"])
        if row["status"] == "inline-uncited":
            errors.append(f"{row['path']}: `{row['name']}` says `SDK: inline` but cites no declared SDK type")
        elif row["status"] == "unannotated":
            seen.add(key)
            if key not in grandfathered:
                errors.append(
                    f"{row['path']}: `{row['name']}` cites no SDK type; add the exact "
                    "`Beta…` name in backticks, `SDK: inline` with the owning type, or `SDK: none` with a reason"
                )
    stale = grandfathered - seen
    for path, name in sorted(stale):
        errors.append(
            f"baseline lists `{name}` in {path} but it is now annotated or gone; remove it from the baseline (the ratchet only shrinks)"
        )
    return errors


def self_test() -> None:
    declared = {"BetaManagedAgentsAgentToolUseEvent", "BetaManagedAgentsAgentToolEvaluation"}
    cases = {
        "/// `BetaManagedAgentsAgentToolEvaluation`.\n": ("cited", "BetaManagedAgentsAgentToolEvaluation"),
        "/// SDK: inline — `BetaManagedAgentsAgentToolUseEvent.evaluated_permission`.\n": (
            "inline",
            "BetaManagedAgentsAgentToolUseEvent",
        ),
        "/// SDK: inline — mirrors something.\n": ("inline-uncited", None),
        "/// SDK: none — adapter-private cursor.\n": ("private", None),
        "/// A doc that cites `Nothing`.\n": ("unannotated", None),
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
    print("check_managed_wire_naming self-test: ok")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument(
        "--write-baseline",
        action="store_true",
        help="rewrite the baseline from the current tree (only when deliberately shrinking it)",
    )
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    oracle = json.loads(ORACLE.read_text(encoding="utf-8"))
    declared = declared_types(oracle)
    rows = scan(TYPES_ROOT, declared)
    if args.write_baseline:
        baseline = {
            "$comment": "Managed wire types that cite no SDK counterpart yet. A ratchet: remove entries as docs are annotated; never add.",
            "unannotated": [
                {"path": row["path"], "name": row["name"]}
                for row in rows
                if row["status"] == "unannotated"
            ],
        }
        BASELINE.write_text(json.dumps(baseline, indent=2) + "\n", encoding="utf-8")
        print(f"wrote baseline with {len(baseline['unannotated'])} unannotated types")
        return 0
    baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
    errors = check(rows, baseline)
    counts = {}
    for row in rows:
        counts[row["status"]] = counts.get(row["status"], 0) + 1
    print(
        "managed wire naming: "
        + ", ".join(f"{status}={count}" for status, count in sorted(counts.items()))
        + f"; baseline={len(baseline['unannotated'])}"
    )
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
