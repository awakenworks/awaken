#!/usr/bin/env python3
"""Validate the invariant registry and close the wiki-citation loop.

Usage: check_invariants.py [FILE ...]

``docs/INVARIANTS.md`` is the human-owned, must-hold registry. The wiki and the
design docs cite invariants by id (``G<n>`` — a single Architecture Guardrail
namespace). This check keeps that machinery consistent without touching the
*content* (which is a human + ADR decision):

1. Every registry row carries a non-empty Statement and Enforcer.
2. Registry ids are unique.
3. Every invariant id *cited in the wiki* resolves to a real definition in the
   corpus -- the central registry (``INVARIANTS.md``) or a design doc that
   declares it. A wiki fact may not cite a phantom invariant.

Deliberately NOT checked:

- Contiguous numbering. Gaps are intentional (e.g. A-G23/A-G24 were vacated by a
  consolidation renumber); enforcing "no holes" would false-positive.
- Promotion of design-local invariants. A cited id need only be *defined
  somewhere authoritative* (the registry or a design doc); the not-yet-promoted
  ids are printed as an informational note, not failed.

No legacy ``A-G`` / ``A-SP`` / ``B-G`` prefix is allowed: those were consolidated
into the single ``G`` namespace, and any survivor is reported as an error.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

INVARIANTS = Path("docs/INVARIANTS.md")
DESIGN_DIR = Path("docs/design")
WIKI_ROOT = Path("docs/wiki")

ID_TOKEN = re.compile(r"\bG\d+\b")
ROW_ID = re.compile(r"^G\d+$")
LEGACY = re.compile(r"A-G\d+|A-G\*|A-SP\d+|B-G[‐‑‒–\-]?[A-Za-z*]+")


def registry_rows(path: Path) -> list[tuple[str, list[str]]]:
    """Return (id, [statement, enforcer, validation]) for each registry row.

    The registry table is `| ID | Statement | Enforcer | Validation |` — four
    columns (ADR-0001 D2): every guardrail must name a concrete enforcer symbol
    AND a concrete validation test, so all three trailing cells are required.
    """
    rows: list[tuple[str, list[str]]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if not ROW_ID.match(cells[0]):
            continue
        if len(cells) < 4:
            rows.append((cells[0], (cells[1:] + ["", "", ""])[:3]))
            continue
        rows.append((cells[0], cells[1:4]))
    return rows


def rows_in_section(text: str, heading: str, next_heading: str) -> set[str]:
    """Return guardrail ids from exactly one status-owning table section."""
    _, marker, tail = text.partition(heading)
    if not marker:
        return set()
    section, _, _ = tail.partition(next_heading)
    return {
        cells[0]
        for line in section.splitlines()
        if line.startswith("|")
        for cells in ([cell.strip() for cell in line.strip().strip("|").split("|")],)
        if cells and ROW_ID.match(cells[0])
    }


def status_table_errors(text: str) -> list[str]:
    active_ids = rows_in_section(text, "## Guardrails", "## Target Guardrails")
    target_ids = rows_in_section(
        text, "## Target Guardrails", "## DDD Review Checklist"
    )
    errors: list[str] = []
    if not active_ids:
        errors.append("active Guardrails table is empty")
    if not target_ids:
        errors.append("Target Guardrails table is empty")
    overlap = sorted(active_ids & target_ids)
    if overlap:
        errors.append("ids appear as both active and target: " + ", ".join(overlap))
    if re.search(r"\*\*(?:Active|Target)\*\*\s*\([^\n]*\bG\d+", text):
        errors.append("status ids must be owned by tables, not a prose inventory")
    return errors


def self_test() -> None:
    # Cause/effect decision table for status ownership:
    # C1 active table exists; C2 target table exists; C3 ids are disjoint;
    # C4 no prose status inventory. E1 accept iff all causes hold; each broken
    # cause is rejected by R2-R5 so semantic status cannot drift beside the table.
    #
    # | Rule | C1 | C2 | C3 | C4 | Effect |
    # | R1   | T  | T  | T  | T  | accept |
    # | R2   | F  | *  | *  | *  | reject |
    # | R3   | T  | F  | *  | *  | reject |
    # | R4   | T  | T  | F  | *  | reject |
    # | R5   | T  | T  | T  | F  | reject |
    valid = """## Guardrails
| G1 | active | owner | test |
## Target Guardrails
| G2 | target | owner | test |
## DDD Review Checklist
"""
    assert not status_table_errors(valid), "R1"
    assert any("active" in error for error in status_table_errors(valid.replace("| G1 | active | owner | test |\n", ""))), "R2"
    assert any("Target" in error for error in status_table_errors(valid.replace("| G2 | target | owner | test |\n", ""))), "R3"
    assert any("both" in error for error in status_table_errors(valid.replace("G2", "G1"))), "R4"
    assert any("prose" in error for error in status_table_errors(valid + "**Active** (G1)\n")), "R5"


def ids_in(path: Path) -> set[str]:
    try:
        return set(ID_TOKEN.findall(path.read_text(encoding="utf-8")))
    except OSError:
        return set()


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        self_test()
        return 0
    if argv:
        print("usage: check_invariants.py [--self-test]", file=sys.stderr)
        return 2
    if not INVARIANTS.is_file():
        print(f"check-invariants:\n  missing {INVARIANTS}", file=sys.stderr)
        return 1

    errors: list[str] = []
    registry_text = INVARIANTS.read_text(encoding="utf-8")

    # Status has one source of truth: section membership. A prose id inventory
    # previously drifted for months while the structural check stayed green.
    errors.extend(
        f"{INVARIANTS}: {error}" for error in status_table_errors(registry_text)
    )

    # 0: no legacy prefix survives anywhere under docs/.
    for doc in sorted(Path("docs").rglob("*.md")):
        try:
            text = doc.read_text(encoding="utf-8")
        except OSError:
            continue
        legacy = sorted(set(LEGACY.findall(text)))
        if legacy:
            errors.append(
                f"{doc}: legacy invariant prefix(es) {', '.join(legacy)} "
                f"— use the single `G<n>` namespace"
            )

    # 1 + 2: registry completeness and unique ids.
    rows = registry_rows(INVARIANTS)
    seen: set[str] = set()
    registry_ids: set[str] = set()
    field_names = ("Statement", "Enforcer", "Validation")
    for inv_id, fields in rows:
        if inv_id in seen:
            errors.append(f"{INVARIANTS}: duplicate invariant id {inv_id}")
        seen.add(inv_id)
        registry_ids.add(inv_id)
        for name, value in zip(field_names, fields):
            if not value:
                errors.append(f"{INVARIANTS}: {inv_id} has empty {name}")

    # Authoritative universe a wiki citation may resolve to.
    design_ids: set[str] = set()
    for doc in sorted(DESIGN_DIR.glob("*.md")):
        design_ids |= ids_in(doc)
    defined = registry_ids | design_ids

    # 3: every id cited in the wiki must be defined somewhere authoritative.
    for doc in sorted(WIKI_ROOT.glob("*.md")):
        for cited in sorted(ids_in(doc)):
            if cited not in defined:
                errors.append(
                    f"{doc}: cites undefined invariant {cited} "
                    f"(not in {INVARIANTS} or any design doc)"
                )

    if errors:
        print("check-invariants:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1

    # Informational: design-local invariants not promoted to the registry.
    unpromoted = sorted(design_ids - registry_ids, key=lambda s: (s[:4], s))
    if unpromoted:
        print(
            "check-invariants: note — design-local invariants not in the central "
            f"registry: {', '.join(unpromoted)}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
