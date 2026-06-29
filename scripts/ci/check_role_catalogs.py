#!/usr/bin/env python3
"""Validate source-document role catalog classification.

Usage: check_role_catalogs.py [FILE ...]

``docs/STATUS.md`` owns the catalog classification for source documents. This
check keeps that table executable:

- every source document is classified exactly once;
- documents marked ``Required`` contain a role/component catalog heading;
- documents marked ``Delegated`` link to a real catalog owner;
- documents marked ``Not required`` do not quietly grow a catalog.

The check is whole-tree by design. Arguments are ignored so it is safe to wire to
any doc glob in pre-commit hooks. Source documents are discovered from
``docs/*.md``, ``docs/design/*.md``, and future ``docs/adr/0*.md`` files. Wiki
documents are excluded because they are checked by the OKF/wiki guardrails.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _doc_anchors import mask_code_fences, resolve_link  # noqa: E402

STATUS = Path("docs/STATUS.md")
DESIGN_DIR = Path("docs/design")
ADR_DIR = Path("docs/adr")

POLICIES = {"Required", "Delegated", "Not required"}
LINK = re.compile(r"\[[^\]]+\]\(([^)]+)\)")
CATALOG_HEADING = re.compile(
    r"^##\s+.*(?:Role Catalog|Component Catalog)\s*$",
    re.MULTILINE,
)


def required_source_docs() -> list[Path]:
    docs = sorted(Path("docs").glob("*.md"))
    docs.extend(sorted(DESIGN_DIR.glob("*.md")))
    if ADR_DIR.is_dir():
        docs.extend(sorted(ADR_DIR.glob("0*.md")))
    return sorted(docs)


def normalize_doc(cell: str) -> Path:
    raw = cell.strip().strip("`")
    path = Path(raw)
    if path.parts and path.parts[0] == "docs":
        return path
    return Path("docs") / path


def table_lines(text: str) -> list[str]:
    marker = "## Role Catalog Coverage"
    start = text.find(marker)
    if start < 0:
        return []
    rest = text[start:].splitlines()[1:]
    lines: list[str] = []
    in_table = False
    for line in rest:
        if line.startswith("## ") and in_table:
            break
        if line.startswith("|"):
            in_table = True
            lines.append(line)
        elif in_table and line.strip():
            break
    return lines


def parse_rows(text: str) -> tuple[list[tuple[Path, str, str, str]], list[str]]:
    rows: list[tuple[Path, str, str, str]] = []
    errors: list[str] = []
    lines = table_lines(text)
    if not lines:
        return rows, [f"{STATUS}: missing `## Role Catalog Coverage` table"]

    for line in lines:
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 4:
            continue
        if cells[0] == "Document" or set(cells[0]) == {"-"}:
            continue
        doc, doc_class, policy, owner = cells[:4]
        if policy not in POLICIES:
            errors.append(
                f"{STATUS}: {doc} has invalid catalog policy `{policy}` "
                f"(expected one of {', '.join(sorted(POLICIES))})"
            )
        if not doc_class:
            errors.append(f"{STATUS}: {doc} has empty class")
        rows.append((normalize_doc(doc), doc_class, policy, owner))
    return rows, errors


def has_catalog(path: Path) -> bool:
    try:
        text = path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        return False
    scan = mask_code_fences(text)
    for match in CATALOG_HEADING.finditer(scan):
        heading = match.group(0).lower()
        if "rule" in heading or "coverage" in heading:
            continue
        return True
    return False


def check_owner_link(owner: str) -> str | None:
    link = LINK.search(owner)
    if not link:
        return "Delegated policy requires a Markdown link in Catalog owner"
    target = link.group(1).split(" ", 1)[0].strip()
    return resolve_link(STATUS, target)


def main(argv: list[str]) -> int:
    del argv
    if not STATUS.is_file():
        print(f"check-role-catalogs:\n  missing {STATUS}", file=sys.stderr)
        return 1

    rows, errors = parse_rows(STATUS.read_text(encoding="utf-8"))
    by_doc: dict[Path, tuple[str, str, str]] = {}
    for doc, doc_class, policy, owner in rows:
        if doc in by_doc:
            errors.append(f"{STATUS}: duplicate role catalog row for {doc}")
        by_doc[doc] = (doc_class, policy, owner)

    expected = set(required_source_docs())
    listed = set(by_doc)
    for missing in sorted(expected - listed):
        errors.append(f"{STATUS}: missing role catalog classification for {missing}")
    for extra in sorted(listed - expected):
        errors.append(f"{STATUS}: role catalog classification references unknown source {extra}")

    for doc, (_doc_class, policy, owner) in sorted(by_doc.items()):
        if doc not in expected:
            continue
        if not doc.is_file():
            errors.append(f"{STATUS}: classified source does not exist: {doc}")
            continue

        catalog = has_catalog(doc)
        if policy == "Required":
            if owner != "self":
                errors.append(f"{STATUS}: {doc} is Required but Catalog owner is not `self`")
            if not catalog:
                errors.append(f"{doc}: catalog policy is Required but no role/component catalog heading was found")
        elif policy == "Delegated":
            problem = check_owner_link(owner)
            if problem:
                errors.append(f"{STATUS}: {doc} delegated catalog owner {problem}")
            if catalog:
                errors.append(f"{doc}: catalog policy is Delegated; move the catalog to the owner or mark this doc Required")
        elif policy == "Not required":
            if owner != "n/a":
                errors.append(f"{STATUS}: {doc} is Not required but Catalog owner is not `n/a`")
            if catalog:
                errors.append(f"{doc}: catalog policy is Not required but a role/component catalog heading was found")

    if errors:
        print("check-role-catalogs:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
