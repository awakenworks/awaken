#!/usr/bin/env python3
"""Validate that relative Markdown links resolve to real files and anchors.

Usage: check_doc_links.py [FILE ...]

Two things are checked for local relative links:

1. The file part resolves to a real file on disk.
2. If the link carries a ``#fragment``, that fragment resolves to a heading in
   the target file -- either the GitHub auto-slug of the heading text or an
   explicit ``{#custom-id}`` attribute. Same-file ``#fragment`` links are checked
   against the file that contains them.

URLs, mailto, and template placeholders (links containing ``XXXX``) are skipped.
Fragments are only validated against local Markdown files we can read.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _doc_anchors import FENCE, resolve_link  # noqa: E402

LINK = re.compile(r"\[[^\]]*\]\(([^)]+)\)")


def is_external(target: str) -> bool:
    return target.startswith(("http://", "https://", "mailto:")) or "XXXX" in target


def main(argv: list[str]) -> int:
    errors: list[str] = []
    for arg in argv:
        path = Path(arg)
        if path.suffix != ".md" or not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue

        in_fence = False
        for line in text.splitlines():
            if FENCE.match(line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            for match in LINK.finditer(line):
                target = match.group(1).split(" ")[0].strip()
                if not target or is_external(target):
                    continue
                problem = resolve_link(path, target)
                if problem:
                    errors.append(f"{path}: {problem}")

    if errors:
        print("check-doc-links:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
