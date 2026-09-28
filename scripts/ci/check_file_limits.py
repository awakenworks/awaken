#!/usr/bin/env python3
"""Enforce file length limits for source, config, and design files."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# Input set: `--staged` size-checks every staged file; the suffix tables below
# only choose which limit applies, never whether the file is read. The trigger
# is therefore the whole staged set.
INPUT_GLOBS: tuple[str, ...] = ("**/*",)

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

CODE_WARNING_LIMIT = 1000
CODE_ERROR_LIMIT = 2000
DOC_LIMIT = 1200
CONFIG_LIMIT = 1200
# Test code had no limit at all, so single files grew to 27k lines and became
# the most-rebased files in the tree (runtime-host host/tests.rs was touched by
# 46 of 500 commits). Tests get more room than production, not unlimited room.
TEST_ERROR_LIMIT = 3000
# A test file that was already over TEST_ERROR_LIMIT when the limit arrived
# carries its ceiling in its own first lines, as `awaken-file-limit-ceiling: N`.
# The ceiling lives with the file, not in a path list here, so a split or a
# rename carries it (or drops it) in the same diff. At commit time the ceiling
# may only fall: it may not exceed the committed one, and a file that had none
# may take one only if it was already over the limit, which after the first
# commit is never. Once the file is back under the limit the marker is stale.
CEILING_MARKER_RE = re.compile(r"awaken-file-limit-ceiling:\s*(\d+)")
CEILING_MARKER_SCAN_LINES = 5

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
        if is_test_path(rel):
            return TEST_ERROR_LIMIT
        return CODE_ERROR_LIMIT
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


def is_test_code_file(rel: str) -> bool:
    return Path(rel).suffix in CODE_SUFFIXES and is_test_path(rel)


def declared_ceiling(text: str) -> int | None:
    for line in text.splitlines()[:CEILING_MARKER_SCAN_LINES]:
        match = CEILING_MARKER_RE.search(line)
        if match:
            return int(match.group(1))
    return None


def committed_text(rel: str) -> str | None:
    try:
        return subprocess.check_output(
            ["git", "show", f"HEAD:{rel}"], cwd=REPO_ROOT, stderr=subprocess.DEVNULL
        ).decode("utf-8", errors="replace")
    except subprocess.CalledProcessError:
        return None


def test_file_errors(rel: str, text: str, previous: str | None, staged: bool) -> list[str]:
    """The test-file limit with its in-file ceiling, judged against HEAD when staged."""

    lines = count_lines(text)
    ceiling = declared_ceiling(text)
    errors: list[str] = []
    if ceiling is None:
        if lines > TEST_ERROR_LIMIT:
            errors.append(
                f"{rel}: {lines} lines > test limit {TEST_ERROR_LIMIT}; split the file by the "
                "behaviour its tests verify"
            )
        return errors
    if lines <= TEST_ERROR_LIMIT:
        errors.append(
            f"{rel}: {lines} lines is within the {TEST_ERROR_LIMIT}-line test limit; remove its "
            "awaken-file-limit-ceiling marker so it cannot grow back"
        )
        return errors
    if lines > ceiling:
        errors.append(
            f"{rel}: {lines} lines > its awaken-file-limit-ceiling {ceiling}; an oversized test "
            "file may shrink but never grow -- add new tests in a new file"
        )
    if staged:
        if previous is None:
            errors.append(
                f"{rel}: a new file may not declare awaken-file-limit-ceiling; keep it under "
                f"{TEST_ERROR_LIMIT} lines"
            )
        else:
            before = declared_ceiling(previous)
            bound = before if before is not None else count_lines(previous) + 1
            if before is None and count_lines(previous) <= TEST_ERROR_LIMIT:
                errors.append(
                    f"{rel}: declares awaken-file-limit-ceiling {ceiling} but was within the "
                    f"{TEST_ERROR_LIMIT}-line limit; a ceiling may not be introduced to admit growth"
                )
            elif ceiling > bound:
                errors.append(
                    f"{rel}: raises awaken-file-limit-ceiling to {ceiling} from {bound}; the "
                    "ceiling only falls"
                )
    return errors


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
        if is_test_code_file(rel):
            previous = committed_text(rel) if staged else None
            found = test_file_errors(rel, text, previous, staged)
            errors.extend(found)
            has_code_violation = has_code_violation or bool(found)
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
        ("tests/runtime.rs", TEST_ERROR_LIMIT),

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
    # Cause/effect table for test files. Causes: C1 lines vs limit, C2 marker
    # present, C3 lines vs marker, C4 staged against HEAD (absent / unmarked
    # under limit / unmarked over limit / marked). Effects: E1 accept, E2 reject.
    big = "// awaken-file-limit-ceiling: 3005\n" + "x\n" * 3000
    small = "x\n" * 10
    rows = [
        ("T1 under the limit, no marker", small, None, False, False),
        ("T2 over the limit, no marker", "x\n" * 3001, None, False, True),
        ("T3 marked, at its ceiling", big, None, False, False),
        ("T4 marked, over its ceiling", big + "x\n" * 10, None, False, True),
        ("T5 marked but back under the limit", "// awaken-file-limit-ceiling: 3005\n" + small, None, False, True),
        ("T6 staged new file with a marker", big, None, True, True),
        ("T7 staged marker added to a file under the limit", big, small, True, True),
        ("T8 staged marker added to a legacy oversized file", big, "x\n" * 3004, True, False),
        ("T9 staged ceiling raised", big, "// awaken-file-limit-ceiling: 3002\n" + "x\n" * 3000, True, True),
        ("T10 staged ceiling held", big, big, True, False),
    ]
    for name, text, previous, staged, rejected in rows:
        if bool(test_file_errors("tests/x.rs", text, previous, staged)) != rejected:
            failures.append(f"{name}: expected {'reject' if rejected else 'accept'}")
    if failures:
        print("Self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("OK - file-limit self-test passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
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
