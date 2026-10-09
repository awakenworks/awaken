#!/usr/bin/env python3
"""Validate Architecture Decision Records (``docs/adr/0*.md``).

Usage: check_adr.py [FILE ...]  (arguments ignored; whole-tree by design)

Per [ADR-0001] the corpus is decision-first: every load-bearing decision is an
ADR with a stable structure so reviewers and tools can rely on it. This check
keeps that structure executable:

1. the title is ``# ADR-NNNN: <name>`` and NNNN matches the file's number;
2. there is a ``- Status:`` line with an allowed value;
3. the required sections ``## Context``, ``## Decision``, ``## Consequences``
   all exist;
4. ADR numbers are unique across ``docs/adr/``;
5. supersede and amend links are two-way: when one ADR's header says it
   ``Supersedes:`` or ``Amends:`` another (or ``Superseded by:`` / ``Amended by:``),
   the other ADR's header must name it back. A one-way link is how a later
   decision silently changed an earlier one (ADR-0091 review, 2026-10-09).

Deliberately NOT checked: prose quality or link resolution (links are covered
by check_doc_links). Classification of each ADR as
a source document is covered by check_role_catalogs.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# Input set: the ADR corpus. Numbers must stay unique across the directory, so
# every ADR is an input even when only one file changed.
INPUT_GLOBS: tuple[str, ...] = ("docs/adr/*.md",)

ADR_DIR = Path("docs/adr")
TITLE = re.compile(r"^#\s+ADR-(\d{4}):\s+\S")
STATUS = re.compile(r"^-\s+Status:\s*(.+?)\s*$", re.MULTILINE)
FILE_NUM = re.compile(r"^(\d{4})-")
ALLOWED_STATUS = {"Proposed", "Accepted", "Superseded", "Deprecated"}
REQUIRED_SECTIONS = ("## Context", "## Decision", "## Consequences")
# Header relations that must be answered in the other ADR's header.
FORWARD_RELATIONS = ("Supersedes", "Amends")
BACKWARD_RELATIONS = ("Superseded by", "Amended by")
HEADER_ITEM = re.compile(r"^-\s+([A-Z][A-Za-z ]*?):\s*(.*)$")
ADR_REFERENCE = re.compile(r"ADR-(\d{4})|\]\((\d{4})-")


def header_items(text: str) -> list[tuple[str, str]]:
    """``(key, value)`` for each ``- Key: value`` item before the first section,
    with indented continuation lines joined to their item."""
    items: list[tuple[str, str]] = []
    for line in text.splitlines()[1:]:
        if line.startswith("## "):
            break
        match = HEADER_ITEM.match(line)
        if match:
            items.append((match.group(1), match.group(2)))
        elif line.startswith("  ") and items:
            key, value = items[-1]
            items[-1] = (key, f"{value} {line.strip()}")
    return items


def referenced(value: str) -> set[str]:
    """ADR numbers a relation names. Explanations are not relations: text after an
    em dash and parenthesised asides (\"(ADR-0001 bar)\") are dropped first; link
    targets are parentheses too, so a linked ADR is found by its ``[ADR-NNNN]`` text."""
    value = value.split(" — ")[0]
    previous = None
    while previous != value:
        previous, value = value, re.sub(r"\([^()]*\)", "", value)
    return {a or b for a, b in ADR_REFERENCE.findall(value)}


def one_way_links(headers: dict[str, list[tuple[str, str]]]) -> list[str]:
    """Relations whose target ADR does not name the source anywhere in its header."""
    errors: list[str] = []
    for num, items in sorted(headers.items()):
        for key, value in items:
            if key not in FORWARD_RELATIONS + BACKWARD_RELATIONS:
                continue
            for target in sorted(referenced(value) - {num}):
                if target not in headers:
                    continue
                if not any(num in referenced(v) for _, v in headers[target]):
                    answer = "Amended by" if key == "Amends" else "Superseded by" if key == "Supersedes" else key.replace(" by", "s").replace("Supersededs", "Supersedes").replace("Amendeds", "Amends")
                    errors.append(
                        f"ADR-{num} says `{key}: ADR-{target}` but ADR-{target}'s header does not "
                        f"name ADR-{num}; add a `- {answer}: ADR-{num}` line there"
                    )
    return errors


def main(argv: list[str]) -> int:
    if not ADR_DIR.is_dir():
        return 0  # no ADRs yet is fine

    errors: list[str] = []
    seen: dict[str, str] = {}
    headers: dict[str, list[tuple[str, str]]] = {}

    for path in sorted(ADR_DIR.glob("0*.md")):
        m = FILE_NUM.match(path.name)
        if not m:
            errors.append(f"{path}: filename must start with a 4-digit number")
            continue
        num = m.group(1)
        if num in seen:
            errors.append(f"{path}: duplicate ADR number {num} (also {seen[num]})")
        seen[num] = path.name

        text = path.read_text(encoding="utf-8")
        headers[num] = header_items(text)
        lines = text.splitlines()
        first = lines[0] if lines else ""
        tm = TITLE.match(first)
        if not tm:
            errors.append(f"{path}: first line must be `# ADR-{num}: <title>`")
        elif tm.group(1) != num:
            errors.append(
                f"{path}: title number ADR-{tm.group(1)} != filename {num}"
            )

        sm = STATUS.search(text)
        if not sm:
            errors.append(f"{path}: missing `- Status:` line")
        elif sm.group(1) not in ALLOWED_STATUS:
            errors.append(
                f"{path}: Status '{sm.group(1)}' not in {sorted(ALLOWED_STATUS)}"
            )

        for sec in REQUIRED_SECTIONS:
            if not re.search(rf"^{re.escape(sec)}\s*$", text, re.MULTILINE):
                errors.append(f"{path}: missing required section `{sec}`")

    errors.extend(one_way_links(headers))

    if errors:
        print("check-adr:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    return 0


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_adr.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("paths", nargs="*", help="ignored; the ADR corpus is scanned whole")
    return parser.parse_args(argv)


if __name__ == "__main__":
    raise SystemExit(main(parse(sys.argv[1:]).paths))
