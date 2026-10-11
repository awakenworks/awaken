#!/usr/bin/env python3
"""Block committed credentials and high-signal secret literals.

* `secret`: a deliberate false positive is exempted only on the same line by
  `awaken-allow(secret):`. Its original optional explanation stays optional;
  this rule has no minimum reason, owner or expiry policy.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

import formal_sources

# Input set: `--staged` scans every staged file whose suffix is in TEXT_SUFFIXES.
# A glob built from that suffix list would silently stop scanning the next file
# type the list grows, which is the exact drift this declaration exists to
# prevent, so the input set is the whole staged set.
INPUT_GLOBS: tuple[str, ...] = ("**/*",)

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

ALLOW_MARKER = "awaken-allow(secret):"
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


def is_excluded(rel: str) -> bool:
    return bool(set(rel.split("/")) & EXCLUDED_PARTS)


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
        if formal_sources.allow_reason(line, "secret") is not None:
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
    formal_sources._selftest_guard_inputs()
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
    # SX1 cause/effect graph: exact secret rule AND same line -> exempt,
    # independent of optional payload length. Other rule/line/old spelling
    # -> original finding. Blank payload is valid; no owner/expiry/minimum
    # reason permission is invented. Existing six matcher cases stay above.
    legacy = "awaken-" + "allow: secret"
    for reason in ("", "x", "fixture; owner=unchanged; expires=unchanged; foreign=text"):
        cases.append(("SX1 optional secret payload", f'token = "abc12345SECRET" # {ALLOW_MARKER} {reason}', ".rs", False))
    for marker in (legacy, "awaken-allow(wait): long timing reason that belongs to another rule"):
        cases.append(("SX1 other/old rule", f'token = "abc12345SECRET" # {marker}', ".rs", True))
    cases.extend((
        ("SX1 prior line", f'# {ALLOW_MARKER}\ntoken = "abc12345SECRET"', ".rs", True),
        ("SX1 later line", f'token = "abc12345SECRET"\n# {ALLOW_MARKER}', ".rs", True),
        ("SX1 high signal", aws_key + f" # {ALLOW_MARKER}", ".txt", False),
    ))
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
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()

    files = (formal_sources.git_paths(REPO_ROOT, "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR")
             if args.staged else formal_sources.tracked_files(REPO_ROOT, allow_nonrepository=True))
    if args.staged and not files:
        print("OK - no staged files to scan for secrets.")
        return 0

    violations: list[str] = []
    for rel in sorted(files):
        if rel in SELF_TEST_ALLOWLIST or is_excluded(rel) or Path(rel).suffix not in TEXT_SUFFIXES:
            continue
        text = formal_sources.file_text(REPO_ROOT, rel, args.staged)
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
