#!/usr/bin/env python3
"""Validate staged ADR shape and append-mostly edits for accepted ADRs."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
ADR_PATH_RE = re.compile(r"^docs/adr/(?P<num>\d{4})-[^/]+\.md$")
TITLE_RE = re.compile(r"^# ADR-(?P<num>\d{4}): .+", re.MULTILINE)
STATUS_RE = re.compile(r"^- \*\*Status\*\*: (?P<status>.+)$", re.MULTILINE)
ACCEPTED_RE = re.compile(r"\bAccepted\b|✅\s*Accepted")
ALLOWED_REMOVAL_RE = re.compile(
    r"^\s*$|^- \*\*(Status|Date|Depends on|Updates|Supersedes|Superseded by)\*\*:|^\[.*\]:\s*\S+|^<!-- awaken-allow: ADR-edit -->$"
)


@dataclass(frozen=True)
class StagedAdr:
    path: str
    text: str
    previous_text: str | None


def _git(args: list[str]) -> str:
    return subprocess.check_output(["git", *args], cwd=REPO_ROOT, text=True)


def _staged_adrs() -> list[StagedAdr]:
    out = _git(["diff", "--cached", "--name-only", "--diff-filter=ACMR"])
    adrs: list[StagedAdr] = []
    for path in out.splitlines():
        if not path.startswith("docs/adr/") or not path.endswith(".md"):
            continue
        text = _git(["show", f":{path}"])
        try:
            previous = _git(["show", f"HEAD:{path}"])
        except subprocess.CalledProcessError:
            previous = None
        adrs.append(StagedAdr(path=path, text=text, previous_text=previous))
    return adrs


def _removed_lines(path: str) -> list[tuple[int, str]]:
    diff = _git(["diff", "--cached", "--unified=0", "--", path])
    removed: list[tuple[int, str]] = []
    old_line = 0
    for raw in diff.splitlines():
        if raw.startswith("@@ "):
            match = re.search(r"@@ -(?P<start>\d+)(?:,\d+)? \+\d+(?:,\d+)? @@", raw)
            old_line = int(match.group("start")) if match else 0
            continue
        if raw.startswith("-") and not raw.startswith("---"):
            removed.append((old_line, raw[1:]))
            old_line += 1
        elif not raw.startswith("+"):
            old_line += 1
    return removed


def validate_adrs(adrs: list[StagedAdr]) -> list[str]:
    errors: list[str] = []
    for adr in adrs:
        path_match = ADR_PATH_RE.match(adr.path)
        if not path_match:
            continue
        expected_num = path_match.group("num")
        title_match = TITLE_RE.search(adr.text)
        if not title_match:
            errors.append(f"{adr.path}: missing '# ADR-{expected_num}: ...' title")
        elif title_match.group("num") != expected_num:
            errors.append(f"{adr.path}: title number ADR-{title_match.group('num')} does not match filename")

        status_match = STATUS_RE.search(adr.text)
        if not status_match:
            errors.append(f"{adr.path}: missing '- **Status**:' metadata")

        if adr.previous_text and STATUS_RE.search(adr.previous_text):
            previous_status = STATUS_RE.search(adr.previous_text).group("status")
            if ACCEPTED_RE.search(previous_status):
                for line_no, line in _removed_lines(adr.path):
                    if not ALLOWED_REMOVAL_RE.search(line):
                        errors.append(
                            f"{adr.path}:{line_no}: accepted ADRs are append-mostly; removed content requires a superseding ADR or explicit amendment"
                        )
                        break
    return errors


def self_test() -> int:
    valid = StagedAdr(
        "docs/adr/9999-example.md",
        "# ADR-9999: Example\n\n- **Status**: Accepted\n",
        None,
    )
    bad_title = StagedAdr(
        "docs/adr/9998-example.md",
        "# ADR-9999: Example\n\n- **Status**: Accepted\n",
        None,
    )
    if validate_adrs([valid]):
        sys.stderr.write("self-test rejected a valid ADR\n")
        return 1
    if not validate_adrs([bad_title]):
        sys.stderr.write("self-test did not reject title/filename mismatch\n")
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
        sys.stderr.write("use --staged for ADR constraint checks\n")
        return 2

    errors = validate_adrs(_staged_adrs())
    if errors:
        sys.stderr.write("ERROR: ADR constraints failed:\n")
        for error in errors:
            sys.stderr.write(f"  - {error}\n")
        sys.stderr.write("<system-reminder>→ Next: keep accepted ADRs append-mostly or add a superseding ADR</system-reminder>\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
