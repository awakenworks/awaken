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

import argparse
import re
import sys
from pathlib import Path

# Input set: the index itself and every document it must list. Adding, renaming
# or deleting a design doc or an ADR is what makes the index incomplete, so all
# three directories are triggers, not just the index.
INPUT_GLOBS: tuple[str, ...] = (
    "docs/adr/*.md",
    "docs/design/*.md",
    "docs/wiki/document-ownership.md",
)

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


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_ownership_index.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("paths", nargs="*", help="ignored; the ownership index is scanned whole")
    return parser.parse_args(argv)


if __name__ == "__main__":
    raise SystemExit(main(parse(sys.argv[1:]).paths))
