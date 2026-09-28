#!/usr/bin/env python3
"""Check repository shape, hidden Unicode, and copy-pasted license text."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# Input set: `--staged` asks git for the complete staged list and inspects every
# one of those paths (root/name shape) and their contents (hidden Unicode,
# pasted license text). Nothing narrower is readable here, so the trigger is the
# whole staged set and the rendered hook carries no glob.
INPUT_GLOBS: tuple[str, ...] = ("**/*",)

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
# A path component that begins with `-` is read as an option by every tool that
# is later handed the path, and in practice it is the footprint of an argument
# that some command wrote to instead of refusing (a 1.7 MB `awaken-sandbox`
# build was committed as `--awaken-gate-argument-probe` exactly this way).
OPTION_SHAPED_PART_RE = re.compile(r"^-")
# Compiled executables and objects are build output wherever they sit. Detected
# by content, not name, because the committed artifact had no suffix at all.
EXECUTABLE_MAGIC: tuple[tuple[bytes, str], ...] = (
    (b"\x7fELF", "ELF"),
    (b"\xfe\xed\xfa\xce", "Mach-O"),
    (b"\xfe\xed\xfa\xcf", "Mach-O"),
    (b"\xce\xfa\xed\xfe", "Mach-O"),
    (b"\xcf\xfa\xed\xfe", "Mach-O"),
    (b"\xca\xfe\xba\xbe", "Mach-O universal"),
)


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


def read_head(rel: str, staged: bool, size: int = 512) -> bytes | None:
    if staged:
        try:
            return subprocess.check_output(
                ["git", "cat-file", "blob", f":{rel}"],
                cwd=REPO_ROOT,
                stderr=subprocess.DEVNULL,
            )[:size]
        except subprocess.CalledProcessError:
            return None
    path = REPO_ROOT / rel
    if not path.is_file():
        return None
    with path.open("rb") as handle:
        return handle.read(size)


def executable_format(head: bytes) -> str | None:
    for magic, name in EXECUTABLE_MAGIC:
        if head.startswith(magic):
            return name
    # PE: an MZ stub whose e_lfanew points at a `PE\0\0` signature. The stub
    # alone is two printable bytes and would misfire on ordinary text.
    if head.startswith(b"MZ") and len(head) >= 0x40:
        offset = int.from_bytes(head[0x3C:0x40], "little")
        if head[offset : offset + 4] == b"PE\0\0":
            return "PE"
    return None


def check_executable_content(files: list[str], staged: bool) -> list[str]:
    violations: list[str] = []
    for rel in sorted(files):
        head = read_head(rel, staged)
        if head is None:
            continue
        kind = executable_format(head)
        if kind is not None:
            violations.append(f"{rel}: tracked {kind} binary; compiled output must not be committed")
    return violations


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
        if any(OPTION_SHAPED_PART_RE.search(part) for part in path.parts):
            violations.append(
                f"{rel}: a path component beginning with `-` is parsed as an option by the next "
                "tool handed this path; it is usually a flag some command wrote to instead of refusing"
            )
        parts = set(path.parts)
        if (
            (parts & GENERATED_PARTS)
            or path.suffix in GENERATED_SUFFIXES
        ):
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
    # Cause/effect table: R1 expanded generated output (any package) -> reject;
    # R2 immutable dependency archive -> accept without a dist exception.
    # This removes the former UI-only exemption together with its manual fork.
    shape_hits = check_path_shape(
        [
            "README.md",
            "NOTES.md",
            "fixtures/sample.json",
            "scripts/ci/__pycache__/x.pyc",
            "web/vendor/awaken-ui/dist/index.js",
            "web/vendor/other-ui/dist/index.js",
            "web/vendor/awaken-ui-0.4.0.tgz",
            "--awaken-gate-argument-probe",
            "crates/x/-o",
        ],
        ["docs/new-progress.md"],
    )
    expected_fragments = [
        "NOTES.md",
        "fixtures/sample.json",
        "__pycache__",
        "new-progress.md",
        "web/vendor/other-ui/dist/index.js",
        "web/vendor/awaken-ui/dist/index.js",
        "--awaken-gate-argument-probe",
        "crates/x/-o",
    ]
    for fragment in expected_fragments:
        if not any(fragment in hit for hit in shape_hits):
            failures.append(f"expected shape violation containing {fragment!r}")

    if any("awaken-ui-0.4.0.tgz" in hit for hit in shape_hits):
        failures.append("immutable dependency archive must be accepted")
    # Cause/effect table: X1 ELF/Mach-O/PE header -> reject by content; X2 a
    # text file that merely starts with `MZ` -> accept; X3 PNG -> accept.
    pe = bytearray(0x80)
    pe[0:2] = b"MZ"
    pe[0x3C:0x40] = (0x40).to_bytes(4, "little")
    pe[0x40:0x44] = b"PE\0\0"
    for head, expected in (
        (b"\x7fELF\x02\x01\x01", "ELF"),
        (b"\xcf\xfa\xed\xfe\x07", "Mach-O"),
        (bytes(pe), "PE"),
        (b"MZ is a note, not a program" + b" " * 80, None),
        (b"\x89PNG\r\n\x1a\n", None),
    ):
        if executable_format(head) != expected:
            failures.append(f"executable_format({head[:8]!r}) != {expected!r}")
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
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
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
    violations.extend(check_executable_content(files, args.staged))
    if violations:
        print("Repository hygiene failed:", file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        return 1
    print("OK - repository hygiene passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
