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

import argparse
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _doc_anchors import FENCE, resolve_link  # noqa: E402

# Input set: the Markdown corpus under docs/, plus the shared anchor resolver
# that decides whether a `#fragment` exists. Link *targets* may live anywhere,
# but a target is only read after a docs Markdown file cites it.
INPUT_GLOBS: tuple[str, ...] = ("docs/**/*.md", "scripts/ci/_doc_anchors.py")

LINK = re.compile(r"\[[^\]]*\]\(([^)]+)\)")


def is_external(target: str) -> bool:
    return target.startswith(("http://", "https://", "mailto:")) or "XXXX" in target


def main(argv: list[str]) -> int:
    # The trigger also covers this checker and the shared anchor resolver, so an
    # argument list may legitimately carry no Markdown. Re-scan the docs corpus
    # then: a trigger that fires and checks nothing is the bug, not a feature.
    paths = [Path(arg) for arg in argv if Path(arg).suffix == ".md"]
    paths = paths or sorted(Path("docs").rglob("*.md"))
    errors: list[str] = []
    for path in paths:
        if not path.is_file():
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


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_doc_links.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("paths", nargs="*", help="Markdown files to check; empty rescans docs/")
    return parser.parse_args(argv)


if __name__ == "__main__":
    raise SystemExit(main(parse(sys.argv[1:]).paths))
