#!/usr/bin/env python3
"""Validate relative links in staged Markdown files."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import unquote

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
LINK_RE = re.compile(r"!?\[[^\]]*\]\((?P<link>[^)\s]+)(?:\s+\"[^\"]*\")?\)")
SKIP_SCHEMES = ("http://", "https://", "mailto:", "data:", "javascript:")
DOC_FILE_RE = re.compile(r"(^AGENTS\.md$|^CLAUDE\.md$|^docs/.*\.md$)")


@dataclass(frozen=True)
class DocFile:
    path: str
    text: str


def _staged_doc_files() -> list[DocFile]:
    out = subprocess.check_output(
        ["git", "diff", "--cached", "--name-only", "--diff-filter=ACMR"],
        cwd=REPO_ROOT,
        text=True,
    )
    docs: list[DocFile] = []
    for path in out.splitlines():
        if not DOC_FILE_RE.search(path):
            continue
        text = subprocess.check_output(["git", "show", f":{path}"], cwd=REPO_ROOT, text=True)
        docs.append(DocFile(path=path, text=text))
    return docs


def _target_path(source_path: str, link: str) -> Path | None:
    if not link or link.startswith(SKIP_SCHEMES) or link.startswith("#"):
        return None
    link = link.split("#", 1)[0].split("?", 1)[0]
    if not link:
        return None
    link = unquote(link)
    raw = Path(link.lstrip("/")) if link.startswith("/") else Path(source_path).parent / link
    normalized = (REPO_ROOT / raw).resolve()
    try:
        normalized.relative_to(REPO_ROOT)
    except ValueError:
        return normalized
    return normalized


def validate_docs(docs: list[DocFile]) -> list[str]:
    errors: list[str] = []
    for doc in docs:
        for match in LINK_RE.finditer(doc.text):
            link = match.group("link").strip("<>")
            target = _target_path(doc.path, link)
            if target is None:
                continue
            line_no = doc.text.count("\n", 0, match.start()) + 1
            if not target.exists():
                errors.append(f"{doc.path}:{line_no}: broken relative link: {link}")
    return errors


def self_test() -> int:
    ok = DocFile("AGENTS.md", "[ADR](docs/adr/0038-runtime-commit-boundary.md)\n")
    broken = DocFile("AGENTS.md", "[Missing](docs/adr/no-such-adr.md)\n")
    if validate_docs([ok]):
        sys.stderr.write("self-test rejected a valid relative link\n")
        return 1
    if not validate_docs([broken]):
        sys.stderr.write("self-test did not reject a missing relative link\n")
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--staged", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.staged:
        sys.stderr.write("use --staged so legacy documentation links do not block unrelated changes\n")
        return 2

    errors = validate_docs(_staged_doc_files())
    if errors:
        sys.stderr.write("ERROR: broken staged Markdown links found:\n")
        for error in errors:
            sys.stderr.write(f"  - {error}\n")
        sys.stderr.write("<system-reminder>→ Next: link to the canonical doc path or remove duplicated truth</system-reminder>\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
