#!/usr/bin/env python3
"""Check local Markdown links and anchors; URLs and code fences are excluded.

No arguments scans the README/documentation inputs declared in INPUT_GLOBS.
Explicit files use the same resolver. --self-test exercises default-scope refusals.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import os
import re
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _doc_anchors import FENCE, resolve_link  # noqa: E402

# One input declaration owns both hook admission and the default corpus scan.
INPUT_GLOBS: tuple[str, ...] = ("docs/**/*.md", "README.md", "CONTRIBUTING.md", "deploy/**/README.md",
    "examples/**/README.md", "crates/sdk/**/README.md", "scripts/ci/_doc_anchors.py")

LINK = re.compile(r"\[[^\]]*\]\(([^)]+)\)")


def is_external(target: str) -> bool:
    return target.startswith(("http://", "https://", "mailto:")) or "XXXX" in target


def main(argv: list[str]) -> int:
    paths = [Path(arg) for arg in argv if Path(arg).suffix == ".md"]
    paths = paths or sorted({p for pattern in INPUT_GLOBS for p in Path(".").glob(pattern) if p.suffix == ".md"})
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


def self_test() -> int:
    # Causes: owner is root/docs/deploy/SDK/example; link file or anchor is
    # missing, or both resolve. Effects: reject either missing target, accept
    # valid links; no original files mutate. Rules L1 missing file -> reject;
    # L2 existing file + wrong anchor -> reject; L3 matching file/anchor -> pass.
    # Cross owner x L1..L3 proves the default scan, not just explicit-file calls.
    original = Path.cwd()
    try:
        with tempfile.TemporaryDirectory() as temporary:
            os.chdir(temporary)
            for name in ("README.md", "docs/README.md", "deploy/README.md", "examples/app/README.md", "crates/sdk/runtime/app/README.md"):
                path = Path(name)
                path.parent.mkdir(parents=True, exist_ok=True)
                path.with_name("target.md").write_text("# Ready", encoding="utf-8")
                for link, expected in (("missing.md", 1), ("target.md#absent", 1), ("target.md#ready", 0)):
                    path.write_text(f"[Target]({link})", encoding="utf-8")
                    with contextlib.redirect_stderr(io.StringIO()):
                        actual = main([])
                    assert actual == expected, (name, link, actual)
        return 0
    finally:
        os.chdir(original)


def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_doc_links.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("--self-test", action="store_true", help="test default scan coverage and refusals")
    parser.add_argument("paths", nargs="*", help="Markdown files to check; empty scans all declared Markdown inputs")
    if "--self-test" in argv and len(argv) != 1:
        parser.error("--self-test takes no paths or other arguments")
    return parser.parse_args(argv)


if __name__ == "__main__":
    args = parse(sys.argv[1:])
    raise SystemExit(self_test() if args.self_test else main(args.paths))
