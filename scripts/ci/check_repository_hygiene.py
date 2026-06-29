#!/usr/bin/env python3
"""Check repository shape, hidden Unicode, and copy-pasted license text."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

ALLOWED_ROOT_MD = {
    "AGENTS.md",
    "CHANGELOG.md",
    "CLAUDE.md",
    "CODE_OF_CONDUCT.md",
    "CONTRIBUTING.md",
    "DEVELOPMENT.md",
    "LICENSE.md",
    "NOTICE.md",
    "README.md",
    "README.zh-CN.md",
    "SECURITY.md",
}
FORBIDDEN_ROOT_FILE_RE = re.compile(r"^(test_.*\.sh|.*\.test\.sh)$")
FORBIDDEN_ROOT_DIR_RE = re.compile(r"^(fixtures|profiles|test-data)/")
FORBIDDEN_DOC_NAME_RE = re.compile(
    r"(STATUS|REPORT|SUMMARY|PROGRESS|IMPLEMENTATION|_LOG|_NOTES|QUICKREF|QUICK|_V[0-9]|_OLD|_NEW)",
    re.IGNORECASE,
)
DOC_NAME_EXCEPTIONS = {
    "docs/STATUS.md",
    "docs/wiki/log.md",
    "docs/wiki/maintenance-notes.md",
}
GENERATED_PARTS = {
    ".next",
    ".turbo",
    ".vite",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
}
GENERATED_SUFFIXES = {".pyc", ".pyo"}
TEXT_SUFFIXES = {
    ".css",
    ".html",
    ".js",
    ".jsx",
    ".json",
    ".md",
    ".mdx",
    ".mjs",
    ".py",
    ".rs",
    ".sh",
    ".toml",
    ".ts",
    ".tsx",
    ".txt",
    ".yaml",
    ".yml",
}
LICENSE_FILES = {"LICENSE", "LICENSE.md", "NOTICE", "NOTICE.md", "COPYING", "COPYING.md"}
LICENSE_TEXT_ALLOWLIST = {"scripts/ci/check_repository_hygiene.py"}
LICENSE_TEXT_RE = re.compile(
    r"(Permission is hereby granted, free of charge|"
    r"THE SOFTWARE IS PROVIDED \"AS IS\"|"
    r"WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND|"
    r"TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION|"
    r"END OF TERMS AND CONDITIONS|"
    r"GNU GENERAL PUBLIC LICENSE|"
    r"GNU LESSER GENERAL PUBLIC LICENSE)",
    re.IGNORECASE,
)
HIDDEN_UNICODE_RE = re.compile(r"[\u200b-\u200f\u202a-\u202e\u2066-\u2069\ufeff]")
CONFLICT_MARKER_RE = re.compile(r"^(<<<<<<<|=======|>>>>>>>)")


def git_lines(args: list[str]) -> list[str]:
    try:
        out = subprocess.check_output(args, cwd=REPO_ROOT, text=True)
    except subprocess.CalledProcessError:
        return []
    return [line for line in out.splitlines() if line]


def staged_files() -> list[str]:
    return git_lines(["git", "diff", "--cached", "--name-only", "--diff-filter=ACMR"])


def added_staged_files() -> list[str]:
    return git_lines(["git", "diff", "--cached", "--name-only", "--diff-filter=A"])


def tracked_files() -> list[str]:
    files = git_lines(["git", "ls-files"])
    if files:
        return files
    return [str(path.relative_to(REPO_ROOT)) for path in REPO_ROOT.rglob("*") if path.is_file()]


def read_content(rel: str, staged: bool) -> str | None:
    if staged:
        try:
            return subprocess.check_output(
                ["git", "show", f":{rel}"],
                cwd=REPO_ROOT,
                stderr=subprocess.DEVNULL,
            ).decode("utf-8", errors="replace")
        except subprocess.CalledProcessError:
            return None
    path = REPO_ROOT / rel
    if not path.is_file():
        return None
    return path.read_text(encoding="utf-8", errors="replace")


def is_text_file(rel: str) -> bool:
    path = Path(rel)
    return path.name in {"lefthook.yml"} or path.suffix in TEXT_SUFFIXES


def check_path_shape(files: list[str], added_files: list[str]) -> list[str]:
    violations: list[str] = []
    for rel in sorted(files):
        path = Path(rel)
        if len(path.parts) == 1 and path.suffix == ".md" and path.name not in ALLOWED_ROOT_MD:
            violations.append(f"{rel}: root Markdown files must be one of {sorted(ALLOWED_ROOT_MD)}")
        if FORBIDDEN_ROOT_FILE_RE.search(rel):
            violations.append(f"{rel}: root ad-hoc test scripts belong under scripts/ or tests/")
        if FORBIDDEN_ROOT_DIR_RE.search(rel):
            violations.append(f"{rel}: root fixture/profile data belongs under tests/fixtures/ or config examples")
        parts = set(path.parts)
        if (parts & GENERATED_PARTS) or path.suffix in GENERATED_SUFFIXES:
            violations.append(f"{rel}: generated/cache/build output must not be tracked")

    for rel in sorted(added_files):
        if not rel.endswith((".md", ".mdx")) or rel in DOC_NAME_EXCEPTIONS:
            continue
        normalized = Path(rel).name.replace("-", "_")
        if FORBIDDEN_DOC_NAME_RE.search(normalized):
            violations.append(f"{rel}: process/status notes should be folded into canonical docs or wiki facts")
    return violations


def check_text_content(files: list[str], staged: bool) -> list[str]:
    violations: list[str] = []
    for rel in sorted(files):
        if not is_text_file(rel):
            continue
        text = read_content(rel, staged)
        if text is None:
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            if HIDDEN_UNICODE_RE.search(line):
                violations.append(f"{rel}:{lineno}: hidden Unicode control character")
            if CONFLICT_MARKER_RE.search(line):
                violations.append(f"{rel}:{lineno}: merge conflict marker")
            if (
                Path(rel).name not in LICENSE_FILES
                and rel not in LICENSE_TEXT_ALLOWLIST
                and LICENSE_TEXT_RE.search(line)
            ):
                violations.append(f"{rel}:{lineno}: full license text belongs only in LICENSE/NOTICE")
    return violations


def self_test() -> int:
    failures: list[str] = []
    shape_hits = check_path_shape(
        ["README.md", "NOTES.md", "fixtures/sample.json", "scripts/ci/__pycache__/x.pyc"],
        ["docs/new-progress.md"],
    )
    expected_fragments = ["NOTES.md", "fixtures/sample.json", "__pycache__", "new-progress.md"]
    for fragment in expected_fragments:
        if not any(fragment in hit for hit in shape_hits):
            failures.append(f"expected shape violation containing {fragment!r}")
    content = "ok\nbad\u200b\n<<<<<<< HEAD\nPermission is hereby granted, free of charge\n"
    hits: list[str] = []
    for lineno, line in enumerate(content.splitlines(), 1):
        if HIDDEN_UNICODE_RE.search(line):
            hits.append(f"hidden:{lineno}")
        if CONFLICT_MARKER_RE.search(line):
            hits.append(f"conflict:{lineno}")
        if LICENSE_TEXT_RE.search(line):
            hits.append(f"license:{lineno}")
    for expected in {"hidden:2", "conflict:3", "license:4"}:
        if expected not in hits:
            failures.append(f"expected content violation {expected}")
    if failures:
        print("Self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("OK - repository-hygiene self-test passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()

    files = staged_files() if args.staged else tracked_files()
    added = added_staged_files() if args.staged else files
    if args.staged and not files:
        print("OK - no staged files to check for repository hygiene.")
        return 0

    violations = check_path_shape(files, added)
    violations.extend(check_text_content(files, args.staged))
    if violations:
        print("Repository hygiene failed:", file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        return 1
    print("OK - repository hygiene passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
