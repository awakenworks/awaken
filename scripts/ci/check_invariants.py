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

import json
import re
import sys
from pathlib import Path

INVARIANTS = Path("docs/INVARIANTS.md")
DESIGN_DIR = Path("docs/design")
WIKI_ROOT = Path("docs/wiki")
FEATURES = Path("formal/features.json")

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


def promotion_profile_errors(target_ids: set[str], data: object) -> list[str]:
    """Require one diagnostic profile for every table-owned target guardrail."""
    if not isinstance(data, dict) or not isinstance(data.get("guardrail_promotions"), list):
        return ["formal/features.json has no guardrail_promotions list"]
    profile_ids: list[str] = []
    errors: list[str] = []
    for profile in data["guardrail_promotions"]:
        guardrail_id = profile.get("id") if isinstance(profile, dict) else None
        if not isinstance(guardrail_id, str) or ROW_ID.fullmatch(guardrail_id) is None:
            errors.append("guardrail promotion profile has an invalid id")
            continue
        profile_ids.append(guardrail_id)
    duplicates = sorted({value for value in profile_ids if profile_ids.count(value) > 1})
    if duplicates:
        errors.append("duplicate guardrail promotion profiles: " + ", ".join(duplicates))
    profile_set = set(profile_ids)
    missing = sorted(target_ids - profile_set)
    stale = sorted(profile_set - target_ids)
    if missing:
        errors.append("target guardrails lack promotion profiles: " + ", ".join(missing))
    if stale:
        errors.append("promotion profiles do not belong to target guardrails: " + ", ".join(stale))
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

    # Promotion-profile cause/effect table: C1 target/profile sets are equal;
    # C2 ids are canonical and unique. E1 accept iff C1+C2; R2 missing, R3
    # stale, R4 duplicate, and R5 malformed each reject without changing which
    # table owns status.
    #
    # | Rule | Equal sets | Canonical unique ids | Effect |
    # |---|---|---|---|
    # | P1 | T | T | accept |
    # | P2 | F (missing) | T | reject |
    # | P3 | F (stale) | T | reject |
    # | P4 | T | F (duplicate) | reject |
    # | P5 | - | F (malformed) | reject |
    profiles = {"guardrail_promotions": [{"id": "G2"}]}
    assert not promotion_profile_errors({"G2"}, profiles), "P1"
    assert any("lack" in error for error in promotion_profile_errors({"G2", "G3"}, profiles)), "P2"
    assert any("do not belong" in error for error in promotion_profile_errors(set(), profiles)), "P3"
    assert any("duplicate" in error for error in promotion_profile_errors(
        {"G2"}, {"guardrail_promotions": [{"id": "G2"}, {"id": "G2"}]}
    )), "P4"
    assert promotion_profile_errors({"G2"}, {"guardrail_promotions": [{"id": "bad"}]}), "P5"


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
    target_ids = rows_in_section(
        registry_text, "## Target Guardrails", "## DDD Review Checklist"
    )
    try:
        feature_data = json.loads(FEATURES.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        errors.append(f"{FEATURES}: cannot read promotion profiles: {error}")
    else:
        errors.extend(
            f"{FEATURES}: {error}"
            for error in promotion_profile_errors(target_ids, feature_data)
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
