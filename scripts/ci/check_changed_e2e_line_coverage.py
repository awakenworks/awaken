#!/usr/bin/env python3
"""Fail when changed production Rust lines lack served-process API E2E coverage."""

from __future__ import annotations

import argparse
import collections
import pathlib
import re
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
TEST_MODULE = re.compile(r"^\s*#\s*\[\s*cfg\s*\([^]]*\btest\b[^]]*\)\s*\]")


def run(*args: str) -> str:
    return subprocess.run(
        args, cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE
    ).stdout


def diff_base(base: str) -> str:
    """Resolve the branch point while retaining uncommitted production edits."""

    return run("git", "merge-base", base, "HEAD").strip()


def test_only_lines(relative: str) -> set[int]:
    """Return source lines owned by Rust's compile-time test surface.

    Separate ``src/tests.rs`` modules are wholly test-only. Inline test modules
    conventionally sit at the end of their production module; once their
    ``#[cfg(test)]`` attribute begins, no later item is part of a shipped binary.
    Keeping these lines out of the denominator prevents adding tests from making
    production coverage regress by construction.
    """

    path = pathlib.PurePosixPath(relative)
    source = (ROOT / relative).read_text(encoding="utf-8").splitlines()
    if path.name == "tests.rs" or "tests" in path.parts:
        return set(range(1, len(source) + 1))
    for number, line in enumerate(source, 1):
        if TEST_MODULE.match(line):
            following = source[number : min(number + 5, len(source))]
            item = next(
                (
                    candidate
                    for candidate in following
                    if candidate.strip()
                    and not candidate.lstrip().startswith(("#", "//"))
                ),
                "",
            )
            if re.match(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{", item):
                return set(range(number, len(source) + 1))
    return set()


def changed_lines(base: str, ignore: re.Pattern[str] | None) -> dict[str, set[int]]:
    baseline = diff_base(base)
    names = run("git", "diff", "--name-only", "--diff-filter=ACMR", baseline, "--")
    result: dict[str, set[int]] = {}
    for relative in names.splitlines():
        if not (
            relative.startswith("crates/")
            and "/src/" in relative
            and relative.endswith(".rs")
        ):
            continue
        if ignore is not None and ignore.search(relative):
            continue
        lines: set[int] = set()
        diff = run("git", "diff", "--unified=0", baseline, "--", relative)
        for line in diff.splitlines():
            match = HUNK.match(line)
            if not match:
                continue
            start = int(match.group(1))
            count = int(match.group(2) or "1")
            lines.update(range(start, start + count))
        if lines:
            production = lines - test_only_lines(relative)
            if production:
                result[relative] = production
    return result


def lcov_lines(ignore_filename_regex: str | None) -> dict[str, dict[int, int]]:
    command = ["cargo", "llvm-cov", "report", "--lcov"]
    if ignore_filename_regex:
        command.extend(["--ignore-filename-regex", ignore_filename_regex])
    lcov = run(*command)
    result: dict[str, dict[int, int]] = {}
    current: str | None = None
    for line in lcov.splitlines():
        if line.startswith("SF:"):
            source = pathlib.Path(line[3:])
            try:
                current = source.resolve().relative_to(ROOT).as_posix()
            except ValueError:
                current = None
        elif current is not None and line.startswith("DA:"):
            number, count, *_ = line[3:].split(",")
            bucket = result.setdefault(current, {})
            line_number = int(number)
            bucket[line_number] = max(bucket.get(line_number, 0), int(count))
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="origin/1.0.0-dev")
    parser.add_argument("--minimum", type=float, default=0.95)
    parser.add_argument(
        "--ignore-filename-regex",
        help="apply the production reachability exclusions used by the coverage report",
    )
    parser.add_argument("--show-missing", type=int, default=80)
    parser.add_argument("--show-files", type=int, default=30)
    parser.add_argument(
        "--label",
        default="changed API E2E line coverage",
        help="human-readable authority name printed in the report and failures",
    )
    args = parser.parse_args()
    if not 0.0 < args.minimum < 1.0:
        parser.error("--minimum must be between zero and one")

    try:
        ignore = (
            re.compile(args.ignore_filename_regex)
            if args.ignore_filename_regex is not None
            else None
        )
    except re.error as error:
        parser.error(f"invalid --ignore-filename-regex: {error}")
    changed = changed_lines(args.base, ignore)
    coverage = lcov_lines(args.ignore_filename_regex)
    executable: list[tuple[str, int, int]] = []
    for relative, lines in changed.items():
        measured = coverage.get(relative, {})
        executable.extend(
            (relative, number, measured[number])
            for number in sorted(lines & measured.keys())
        )
    if not executable:
        raise SystemExit(f"{args.label}: no changed executable Rust lines found")

    covered = [(path, line, count) for path, line, count in executable if count > 0]
    missing = [(path, line) for path, line, count in executable if count == 0]
    ratio = len(covered) / len(executable)
    print(
        f"{args.label}: {len(covered)}/{len(executable)} = {ratio:.2%} "
        f"(required > {args.minimum:.0%}, base {args.base})"
    )
    by_file: dict[str, list[int]] = collections.defaultdict(lambda: [0, 0])
    for path, _line, count in executable:
        by_file[path][1] += 1
        if count > 0:
            by_file[path][0] += 1
    ranked = sorted(
        by_file.items(),
        key=lambda item: (item[1][1] - item[1][0], item[1][1]),
        reverse=True,
    )
    for path, (file_covered, file_total) in ranked[: args.show_files]:
        print(
            f"  file {file_covered}/{file_total} = {file_covered / file_total:.2%} {path}"
        )
    for path, line in missing[: args.show_missing]:
        print(f"  uncovered {path}:{line}")
    if len(missing) > args.show_missing:
        print(f"  ... and {len(missing) - args.show_missing} more")
    if ratio <= args.minimum:
        raise SystemExit(f"{args.label} is below the strict threshold")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        print(f"changed line coverage command failed: {error}", file=sys.stderr)
        raise SystemExit(error.returncode) from error
