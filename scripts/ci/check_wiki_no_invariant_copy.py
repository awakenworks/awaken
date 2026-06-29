#!/usr/bin/env python3
"""Keep wiki facts thin: forbid copying an invariant statement verbatim.

Usage: check_wiki_no_invariant_copy.py [FILE ...]

The wiki is a retrieval layer, not a second source of truth. A `FACT-*` record
should *summarize* and reference an invariant by id (``G<n>``), not paste the
invariant's statement text. When a fact copies a long
contiguous run of words straight from ``docs/INVARIANTS.md``, the statement now
lives in two places and will drift.

This check parses the invariant statements out of the INVARIANTS table, then
compares them against the ``- Fact:`` field of every wiki fact record. If the
longest shared contiguous word run is at least ``THRESHOLD`` words, it fails and
tells the author to summarize and cite the id instead.

Only the ``Fact:`` field is scanned. The ``Verification`` field deliberately
reuses the invariant's enforcer wording, and ``Links`` carries the id -- those
are the intended references, not copies. Arguments are ignored when empty: the
whole ``docs/wiki`` tree is scanned.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _doc_anchors import mask_code_fences  # noqa: E402

INVARIANTS = Path("docs/INVARIANTS.md")
WIKI_ROOT = Path("docs/wiki")
THRESHOLD = 12  # words; a shared run this long is a copy, not a coincidence.

ID = re.compile(r"^G\d+$")
FACT_HEADING = re.compile(r"^##\s+(FACT-[A-Z]+-\d{3}):", re.MULTILINE)
FACT_FIELD = re.compile(r"^-\s+Fact:\s*(.*?)(?=^-\s+\w+:)", re.MULTILINE | re.DOTALL)


def words(text: str) -> list[str]:
    """Normalize to a lowercase word list, dropping markdown/punctuation."""
    text = text.replace("`", " ").replace("*", " ")
    return re.findall(r"[a-z0-9]+", text.lower())


def invariant_statements(path: Path) -> dict[str, list[str]]:
    """Map each invariant id to the word list of its statement column."""
    out: dict[str, list[str]] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 4 or not ID.match(cells[0]):
            continue
        out[cells[0]] = words(cells[1])
    return out


def longest_run(a: list[str], b: list[str]) -> int:
    """Length of the longest contiguous shared subsequence of words."""
    if not a or not b:
        return 0
    prev = [0] * (len(b) + 1)
    best = 0
    for i in range(1, len(a) + 1):
        cur = [0] * (len(b) + 1)
        ai = a[i - 1]
        for j in range(1, len(b) + 1):
            if ai == b[j - 1]:
                cur[j] = prev[j - 1] + 1
                if cur[j] > best:
                    best = cur[j]
        prev = cur
    return best


def fact_records(text: str) -> list[tuple[str, str]]:
    """Return (fact_id, fact_field_text) for every real FACT record."""
    scan = mask_code_fences(text)  # ignore the fenced template in README
    heads = list(FACT_HEADING.finditer(scan))
    records: list[tuple[str, str]] = []
    for idx, head in enumerate(heads):
        end = heads[idx + 1].start() if idx + 1 < len(heads) else len(scan)
        section = text[head.end() : end]
        field = FACT_FIELD.search(section)
        if field:
            records.append((head.group(1), field.group(1)))
    return records


def main(argv: list[str]) -> int:
    if not INVARIANTS.is_file():
        print(f"check-wiki-no-invariant-copy:\n  missing {INVARIANTS}", file=sys.stderr)
        return 1
    statements = invariant_statements(INVARIANTS)

    paths = [Path(a) for a in argv if Path(a).suffix == ".md"]
    paths = [p for p in paths if p.parts[:2] == ("docs", "wiki")] or sorted(
        WIKI_ROOT.glob("*.md")
    )

    errors: list[str] = []
    for path in paths:
        if not path.is_file():
            continue
        for fact_id, field in fact_records(path.read_text(encoding="utf-8")):
            fw = words(field)
            for inv_id, sw in statements.items():
                run = longest_run(fw, sw)
                if run >= THRESHOLD:
                    errors.append(
                        f"{path}: {fact_id} copies {run} consecutive words from "
                        f"{inv_id}; summarize and reference the id instead"
                    )

    if errors:
        print("check-wiki-no-invariant-copy:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
