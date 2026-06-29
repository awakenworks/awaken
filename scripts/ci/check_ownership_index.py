#!/usr/bin/env python3
"""Keep the wiki ownership index complete.

Usage: check_ownership_index.py [FILE ...]

The ownership index lives in the wiki (``docs/wiki/document-ownership.md``). It
is only useful if it lists *every* design document (and any ADR, if this corpus
grows a ``docs/adr`` tree). A human reminder rots; this check fails the commit
when a design doc is added, renamed, or removed without updating the index.
Arguments are ignored -- the whole tree is scanned -- so the check is safe to
wire to any glob. This corpus owns no ``docs/adr`` tree.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

INDEX = Path("docs/wiki/document-ownership.md")
DESIGN_DIR = Path("docs/design")
ADR_DIR = Path("docs/adr")
LINK = re.compile(r"\[[^\]]*\]\(([^)]+)\)")

# Files that are navigation, not owned content, so they need no ownership row.
EXEMPT = {"README.md", "0000-template.md"}


def required_docs() -> list[Path]:
    docs = [p for p in DESIGN_DIR.glob("*.md") if p.name not in EXEMPT]
    docs += [p for p in ADR_DIR.glob("0*.md") if p.name not in EXEMPT]
    return sorted(docs)


def linked_names(index_text: str) -> set[str]:
    names: set[str] = set()
    for match in LINK.finditer(index_text):
        target = match.group(1).split(" ", 1)[0].split("#", 1)[0].strip()
        if target.endswith(".md"):
            names.add(Path(target).name)
    return names


def main(argv: list[str]) -> int:
    if not INDEX.is_file():
        print(f"check-ownership-index:\n  missing index {INDEX}", file=sys.stderr)
        return 1

    listed = linked_names(INDEX.read_text(encoding="utf-8"))
    missing = [str(p) for p in required_docs() if p.name not in listed]

    if missing:
        print("check-ownership-index:", file=sys.stderr)
        print(f"  these docs are not listed in {INDEX}:", file=sys.stderr)
        for path in missing:
            print(f"    {path}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
