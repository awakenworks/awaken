#!/usr/bin/env python3
"""Block committed credentials and high-signal secret literals."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

ALLOW_MARKER = "awaken-allow: secret"
CODE_SUFFIXES = {
    ".cjs",
    ".env",
    ".go",
    ".java",
    ".js",
    ".jsx",
    ".mjs",
    ".py",
    ".rb",
    ".rs",
    ".sh",
    ".ts",
    ".tsx",
}
TEXT_SUFFIXES = CODE_SUFFIXES | {
    ".css",
    ".html",
    ".json",
    ".md",
    ".mdx",
    ".toml",
    ".txt",
    ".yaml",
    ".yml",
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
SELF_TEST_ALLOWLIST = {"scripts/ci/check_secrets.py"}
HIGH_SIGNAL = [
    ("private key block", re.compile(r"-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----")),
    ("AWS access key id", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("GitHub token", re.compile(r"\bgh[pousr]_[A-Za-z0-9]{36,}\b|\bgithub_pat_[A-Za-z0-9_]{22,}\b")),
    ("Slack token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b")),
    ("Google API key", re.compile(r"\bAIza[0-9A-Za-z_\-]{35}\b")),
]
GENERIC_SECRET_RE = re.compile(
    r"(?:password|passwd|secret|api[_-]?key|access[_-]?key|private[_-]?key|"
    r"client[_-]?secret|auth[_-]?token|token)(?![A-Za-z])"
    r"['\"]?\s*[:=]\s*(?P<quote>['\"])(?P<value>[^'\"]{8,})(?P=quote)",
    re.IGNORECASE,
)
PLACEHOLDER_SUBSTRINGS = (
    "${",
    "{{",
    "changeme",
    "dummy",
    "example",
    "os.environ",
    "placeholder",
    "process.env",
    "redacted",
    "sample",
    "your-",
    "your_",
)
PLACEHOLDER_EXACT = {
    "apikey",
    "api_key",
    "false",
    "none",
    "null",
    "password",
    "secret",
    "test",
    "token",
    "true",
}


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
    return [str(path.relative_to(REPO_ROOT)) for path in REPO_ROOT.rglob("*") if path.is_file()]


def is_excluded(rel: str) -> bool:
    return bool(set(rel.split("/")) & EXCLUDED_PARTS)


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


def looks_secret(value: str) -> bool:
    normalized = value.strip()
    low = normalized.lower()
    if not normalized or normalized.startswith("<") or low in PLACEHOLDER_EXACT:
        return False
    if any(marker in low for marker in PLACEHOLDER_SUBSTRINGS):
        return False
    if re.fullmatch(r"x{3,}", low) or re.fullmatch(r"\*+", normalized):
        return False
    return bool(re.search(r"[A-Za-z]", normalized) and re.search(r"[0-9!@#$%^&*_\-./+=]", normalized))


def scan_text(text: str, suffix: str) -> list[str]:
    findings: list[str] = []
    for lineno, line in enumerate(text.splitlines(), 1):
        if ALLOW_MARKER in line:
            continue
        for label, pattern in HIGH_SIGNAL:
            if pattern.search(line):
                findings.append(f"line {lineno}: {label}")
        if suffix in CODE_SUFFIXES:
            match = GENERIC_SECRET_RE.search(line)
            if match and looks_secret(match.group("value")):
                findings.append(f"line {lineno}: hard-coded secret assignment")
    return findings


def self_test() -> int:
    private_key = "-----BEGIN " + "PRIVATE KEY-----"
    aws_key = "AKIA" + ("0" * 16)
    cases = [
        ("private key", private_key, ".md", True),
        ("aws key", aws_key, ".txt", True),
        ("generic secret", 'token = "abc12345SECRET"', ".rs", True),
        ("placeholder", 'token = "your-token-here"', ".rs", False),
        ("docs example", 'token = "abc12345SECRET"', ".md", False),
        ("allow marker", f'token = "abc12345SECRET" # {ALLOW_MARKER}', ".rs", False),
    ]
    failures: list[str] = []
    for name, text, suffix, expected in cases:
        hits = scan_text(text, suffix)
        if expected and not hits:
            failures.append(f"{name}: expected a finding")
        if not expected and hits:
            failures.append(f"{name}: expected no finding, got {hits}")
    if failures:
        print("Self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("OK - secrets self-test passed.")
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
        print("OK - no staged files to scan for secrets.")
        return 0

    violations: list[str] = []
    for rel in sorted(files):
        if rel in SELF_TEST_ALLOWLIST or is_excluded(rel) or Path(rel).suffix not in TEXT_SUFFIXES:
            continue
        text = read_content(rel, args.staged)
        if text is None:
            continue
        for hit in scan_text(text, Path(rel).suffix):
            violations.append(f"{rel}:{hit}")
    if violations:
        print("Possible committed secrets found:", file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        print(
            f"\nMove credentials to environment/vault/config injection. "
            f"For a deliberate false positive, append '{ALLOW_MARKER}' on that line.",
            file=sys.stderr,
        )
        return 1
    print("OK - no committed secrets found.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
