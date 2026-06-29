#!/usr/bin/env python3
"""Enforce file length limits for source, config, and design files."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

CODE_WARNING_LIMIT = 1000
CODE_ERROR_LIMIT = 2000
DOC_LIMIT = 1200
CONFIG_LIMIT = 1200

CODE_SUFFIXES = {
    ".rs",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".mjs",
    ".cjs",
    ".py",
    ".go",
    ".java",
    ".sh",
    ".rb",
    ".astro",
    ".vue",
    ".sql",
}
DOC_SUFFIXES = {".md", ".mdx"}
CONFIG_SUFFIXES = {".json", ".toml", ".yaml", ".yml"}
LOCKFILES = {
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
}
EXCLUDED_PARTS = {
    ".git",
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
TEST_PATH_RE = re.compile(r"(^|/)(fixtures|test-data|snapshots|tests|test)/")
TEST_FILE_RE = re.compile(
    r"(^|/)[^/]*(\.test|\.spec)\.(ts|tsx|js|jsx)$|(^|/)tests\.rs$|_test\.(go|rs)$"
)


def is_excluded(rel: str) -> bool:
    parts = set(rel.split("/"))
    if parts & EXCLUDED_PARTS:
        return True
    return "/generated/" in f"/{rel}/" or ".generated." in rel


def is_test_path(rel: str) -> bool:
    return bool(TEST_PATH_RE.search(rel) or TEST_FILE_RE.search(rel))


def limit_for(rel: str) -> int | None:
    path = Path(rel)
    if path.name in LOCKFILES or is_excluded(rel):
        return None
    if path.suffix in CODE_SUFFIXES:
        return None if is_test_path(rel) else CODE_ERROR_LIMIT
    if path.suffix in DOC_SUFFIXES:
        return DOC_LIMIT
    if path.suffix in CONFIG_SUFFIXES:
        return CONFIG_LIMIT
    return None


def git_lines(args: list[str]) -> list[str]:
    try:
        out = subprocess.check_output(args, cwd=REPO_ROOT, text=True)
    except subprocess.CalledProcessError:
        return []
    return [line for line in out.splitlines() if line]


def staged_files() -> list[str]:
    return git_lines(["git", "diff", "--cached", "--name-only", "--diff-filter=ACMR"])


def tracked_files() -> list[str]:
    files = git_lines(["git", "ls-files"])
    if files:
        return files
    return [
        str(path.relative_to(REPO_ROOT))
        for path in REPO_ROOT.rglob("*")
        if path.is_file() and not is_excluded(str(path.relative_to(REPO_ROOT)))
    ]


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


def count_lines(text: str) -> int:
    if not text:
        return 0
    return text.count("\n") + (0 if text.endswith("\n") else 1)


def is_code_file(rel: str) -> bool:
    return Path(rel).suffix in CODE_SUFFIXES and not is_test_path(rel)


def warning_limit_for(rel: str) -> int | None:
    return CODE_WARNING_LIMIT if is_code_file(rel) and limit_for(rel) is not None else None


def classify_line_count(rel: str, lines: int) -> str | None:
    limit = limit_for(rel)
    if limit is None:
        return None
    if is_code_file(rel):
        if lines >= limit:
            return "error"
        warning_limit = warning_limit_for(rel)
        if warning_limit is not None and lines >= warning_limit:
            return "warning"
        return None
    return "error" if lines > limit else None


def find_results(files: list[str], staged: bool) -> tuple[list[str], list[str], bool]:
    errors: list[str] = []
    warnings: list[str] = []
    has_code_violation = False
    for rel in sorted(files):
        limit = limit_for(rel)
        if limit is None:
            continue
        text = read_content(rel, staged)
        if text is None:
            continue
        lines = count_lines(text)
        classification = classify_line_count(rel, lines)
        if classification == "error":
            if is_code_file(rel):
                errors.append(f"{rel}: {lines} lines >= hard limit {limit}")
                has_code_violation = True
            else:
                errors.append(f"{rel}: {lines} lines > {limit}")
            continue
        if classification == "warning":
            warnings.append(f"{rel}: {lines} lines >= warning threshold {CODE_WARNING_LIMIT}")
    return warnings, errors, has_code_violation


def self_test() -> int:
    failures: list[str] = []
    cases = [
        ("src/lib.rs", CODE_ERROR_LIMIT),
        ("docs/design/runtime.md", DOC_LIMIT),
        ("package.json", CONFIG_LIMIT),
        ("tests/runtime.rs", None),
        ("src/generated/schema.rs", None),
        ("Cargo.lock", None),
    ]
    for rel, expected in cases:
        actual = limit_for(rel)
        if actual != expected:
            failures.append(f"{rel}: expected {expected}, got {actual}")
    if count_lines("a\nb\n") != 2 or count_lines("a\nb") != 2:
        failures.append("line counter returned an unexpected count")
    line_cases = [
        ("src/lib.rs", CODE_WARNING_LIMIT - 1, None),
        ("src/lib.rs", CODE_WARNING_LIMIT, "warning"),
        ("src/lib.rs", CODE_ERROR_LIMIT - 1, "warning"),
        ("src/lib.rs", CODE_ERROR_LIMIT, "error"),
        ("docs/design/runtime.md", DOC_LIMIT, None),
        ("docs/design/runtime.md", DOC_LIMIT + 1, "error"),
    ]
    for rel, lines, expected in line_cases:
        actual = classify_line_count(rel, lines)
        if actual != expected:
            failures.append(f"{rel} at {lines} lines: expected {expected}, got {actual}")
    warnings, errors, has_code_error = find_results(["src/lib.rs", "src/large.rs"], staged=False)
    if warnings or errors or has_code_error:
        failures.append("empty fixture paths should not produce warnings/errors")
    if warning_limit_for("src/lib.rs") != CODE_WARNING_LIMIT:
        failures.append("source files should receive a 1000-line warning threshold")
    if warning_limit_for("tests/runtime.rs") is not None:
        failures.append("test files should not receive a source warning threshold")
    if failures:
        print("Self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("OK - file-limit self-test passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()

    files = staged_files() if args.staged else tracked_files()
    if args.staged and not files:
        print("OK - no staged files to size-check.")
        return 0

    warnings, errors, has_code_violation = find_results(files, args.staged)
    if warnings:
        print("File length warnings:", file=sys.stderr)
        for warning in warnings:
            print(f"  {warning}", file=sys.stderr)
        print(
            "\nBackend/frontend source files over 1000 lines should be split before "
            "they reach the 2000-line hard limit. Do not respond by moving tests "
            "away, deleting tests, or weakening coverage; split production "
            "responsibilities and keep tests with the behavior they verify.",
            file=sys.stderr,
        )
    if errors:
        print("File length limits failed:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        if has_code_violation:
            print(
                "\nFor backend/frontend source files, do not pass this gate by "
                "moving tests away, deleting tests, or weakening coverage. Split "
                "the production module/component by responsibility and keep tests "
                "with the behavior they verify.",
                file=sys.stderr,
            )
        print("\nSplit large docs by bounded design topic before committing.", file=sys.stderr)
        return 1
    print("OK - file length limits passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
