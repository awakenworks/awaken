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
4. ADR numbers are unique across ``docs/adr/``.

Deliberately NOT checked: prose quality, supersede-graph completeness, or link
resolution (links are covered by check_doc_links). Classification of each ADR as
a source document is covered by check_role_catalogs.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ADR_DIR = Path("docs/adr")
TITLE = re.compile(r"^#\s+ADR-(\d{4}):\s+\S")
STATUS = re.compile(r"^-\s+Status:\s*(.+?)\s*$", re.MULTILINE)
FILE_NUM = re.compile(r"^(\d{4})-")
ALLOWED_STATUS = {"Proposed", "Accepted", "Superseded", "Deprecated"}
REQUIRED_SECTIONS = ("## Context", "## Decision", "## Consequences")


def main(argv: list[str]) -> int:
    if not ADR_DIR.is_dir():
        return 0  # no ADRs yet is fine

    errors: list[str] = []
    seen: dict[str, str] = {}

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

    if errors:
        print("check-adr:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
