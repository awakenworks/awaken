#!/usr/bin/env python3
"""Validate the OKF wiki shape.

Usage: check_wiki_okf.py [FILE ...]

The repo keeps ``docs/wiki`` conformant with Open Knowledge Format conventions
while using local ``FACT-*`` records inside ordinary concept pages. This check
verifies the mechanical parts agents are likely to drift:

- every ordinary Markdown concept file in ``docs/wiki`` has frontmatter with a
  non-empty ``type``;
- reserved ``index.md`` files have no frontmatter and act as navigation;
- reserved ``log.md`` files have no frontmatter and use date-grouped update
  history;
- owned fact records use ``FACT-*`` headings, not legacy ``OKF-*`` headings;
- every fact record carries Status, Owner, Fact, Links, and Verification fields;
- each fact's ``Owner`` field links to a real owner file and, if anchored, a real
  heading -- so a renamed owner or heading cannot silently orphan a fact.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _doc_anchors import mask_code_fences, resolve_link  # noqa: E402

# Input set: the flat wiki page set this checker globs (`WIKI_ROOT.glob("*.md")`),
# plus the shared anchor resolver used to prove each fact's Owner link resolves.
INPUT_GLOBS: tuple[str, ...] = ("docs/wiki/*.md", "scripts/ci/_doc_anchors.py")

WIKI_ROOT = Path("docs/wiki")
REQUIRED_FACT_FIELDS = ("Status", "Owner", "Fact", "Links", "Verification")
FACT_HEADING = re.compile(r"^##\s+FACT-[A-Z]+-\d{3}:", re.MULTILINE)
LEGACY_OKF_HEADING = re.compile(r"^##\s+OKF-[A-Z]+-\d{3}:", re.MULTILINE)
MARKDOWN_LINK = re.compile(r"\[[^\]]*\]\(([^)]+)\)")
DATE_HEADING = re.compile(r"^##\s+\d{4}-\d{2}-\d{2}\s*$", re.MULTILINE)


def is_wiki_markdown(path: Path) -> bool:
    return path.suffix == ".md" and path.parts[:2] == ("docs", "wiki")


def frontmatter(text: str) -> tuple[int, list[str]] | None:
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return None
    for idx, line in enumerate(lines[1:], 1):
        if line.strip() == "---":
            return idx, lines[1:idx]
    return None


def has_non_empty_type(lines: list[str]) -> bool:
    for line in lines:
        if line.startswith("type:") and line.split(":", 1)[1].strip():
            return True
    return False


def fact_sections(text: str) -> list[tuple[str, str]]:
    headings = list(FACT_HEADING.finditer(text))
    sections: list[tuple[str, str]] = []
    for idx, match in enumerate(headings):
        end = headings[idx + 1].start() if idx + 1 < len(headings) else len(text)
        heading = text[match.start() : text.find("\n", match.start(), end)]
        sections.append((heading, text[match.end() : end]))
    return sections


def check_file(path: Path) -> list[str]:
    errors: list[str] = []
    try:
        text = path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError) as exc:
        return [f"{path}: cannot read ({exc})"]

    if path.name == "index.md":
        errors.extend(check_index_file(path, text))
    elif path.name == "log.md":
        errors.extend(check_log_file(path, text))
    else:
        errors.extend(check_concept_file(path, text))

    if "OKF means **Owned" in text:
        errors.append(f"{path}: reserve OKF for Open Knowledge Format")

    # Fact records shown as fenced examples (the template) are not real records.
    scan = mask_code_fences(text)

    for match in LEGACY_OKF_HEADING.finditer(scan):
        errors.append(f"{path}: legacy fact heading `{match.group(0)}`; use FACT-*")

    for match in MARKDOWN_LINK.finditer(text):
        target = match.group(1).split(" ", 1)[0].strip()
        if target.startswith("/"):
            errors.append(f"{path}: use a relative link instead of `{target}`")

    for heading, section in fact_sections(scan):
        for field in REQUIRED_FACT_FIELDS:
            pattern = re.compile(rf"^-\s+{field}:\s*\S", re.MULTILINE)
            if not pattern.search(section):
                errors.append(f"{path}: {heading} missing `- {field}:`")
        errors.extend(check_owner_link(path, heading, section))

    return errors


def check_concept_file(path: Path, text: str) -> list[str]:
    fm = frontmatter(text)
    if fm is None:
        return [f"{path}: ordinary OKF concept file is missing YAML frontmatter"]
    _, lines = fm
    if not has_non_empty_type(lines):
        return [f"{path}: frontmatter missing non-empty `type`"]
    return []


def check_index_file(path: Path, text: str) -> list[str]:
    errors: list[str] = []
    if frontmatter(text) is not None:
        errors.append(f"{path}: OKF reserved index.md must not have frontmatter")
    if not re.search(r"^#\s+\S", text, re.MULTILINE):
        errors.append(f"{path}: OKF index.md should start with a title heading")
    if not re.search(r"^-\s+\[[^\]]+\]\([^)]+\)\s+-\s+\S", text, re.MULTILINE):
        errors.append(
            f"{path}: OKF index.md should enumerate links as `- [Title](target) - description`"
        )
    return errors


def check_log_file(path: Path, text: str) -> list[str]:
    errors: list[str] = []
    if frontmatter(text) is not None:
        errors.append(f"{path}: OKF reserved log.md must not have frontmatter")
    if not re.search(r"^#\s+\S", text, re.MULTILINE):
        errors.append(f"{path}: OKF log.md should start with a title heading")
    dates = list(DATE_HEADING.finditer(text))
    if not dates:
        errors.append(f"{path}: OKF log.md must contain ISO date headings like `## YYYY-MM-DD`")
        return errors
    values = [match.group(0).removeprefix("##").strip() for match in dates]
    if values != sorted(values, reverse=True):
        errors.append(f"{path}: OKF log.md date headings must be newest first")
    return errors


def check_owner_link(path: Path, heading: str, section: str) -> list[str]:
    owner = re.search(r"^-\s+Owner:\s*(.+)$", section, re.MULTILINE)
    if not owner:
        return []
    link = MARKDOWN_LINK.search(owner.group(1))
    if not link:
        return [f"{path}: {heading} `Owner` must link to its owning document"]
    target = link.group(1).split(" ", 1)[0].strip()
    problem = resolve_link(path, target)
    return [f"{path}: {heading} Owner {problem}"] if problem else []


def main(argv: list[str]) -> int:
    # The trigger also covers this checker and the shared anchor resolver, so an
    # argument list may legitimately carry no wiki page. Re-scan the whole wiki
    # then: a trigger that fires and checks nothing is the bug, not a feature.
    paths = [path for path in map(Path, argv) if is_wiki_markdown(path)]
    paths = paths or sorted(WIKI_ROOT.glob("*.md"))
    errors: list[str] = []
    for path in paths:
        if path.is_file():
            errors.extend(check_file(path))

    if errors:
        print("check-wiki-okf:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
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
        prog="check_wiki_okf.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("paths", nargs="*", help="wiki Markdown files; empty rescans the wiki")
    return parser.parse_args(argv)


if __name__ == "__main__":
    raise SystemExit(main(parse(sys.argv[1:]).paths))
