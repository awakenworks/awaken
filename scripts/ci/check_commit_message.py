#!/usr/bin/env python3
"""Validate commit messages (subject shape, length, body wrap, banned trailers).

Run as a lefthook ``commit-msg`` command with the message file path:

    python3 scripts/ci/check_commit_message.py "$1"

Rules (ported from ../awaken-next, plus a 72-column body wrap):

  * Subject must match ``<emoji> <type>(<scope>): <subject>``.
  * Subject length <= 100 characters.
  * At most 4 non-comment lines; a body must be separated by one blank line.
  * Body lines wrap at <= 72 columns (URLs / unbreakable tokens exempt).
  * No ``Co-Authored-By:`` trailers.
  * No AI-generation markers (``Generated with/by``, ``🤖 ... Generated``).
  * No external-tool provenance (``via [..](url)``, ``Tool:``, ``Platform:``).
  * No project-management jargon (phases, owners, % done, 预计/计划/负责人, ...).
"""

from __future__ import annotations

import argparse
import re
import sys

SUBJECT_MAX = 100
BODY_WRAP = 72
MAX_LINES = 4

ALLOWED_TYPES = {
    "feat",
    "fix",
    "docs",
    "refactor",
    "test",
    "chore",
    "perf",
    "style",
    "build",
    "ci",
    "revert",
}

# <emoji-or-nonspace-token> <type>(<scope>): <subject>
SUBJECT_RE = re.compile(r"^\S+ ([a-z]+)\(([a-z0-9-]+)\): (.+)$")

CO_AUTHORED_RE = re.compile(r"co-authored-by:", re.IGNORECASE)
AI_MARKER_RE = re.compile(r"(Generated (with|by))|(🤖.*Generated)", re.IGNORECASE)
EXTERNAL_TOOL_RE = re.compile(
    r"(^via \[[^]]+\]\(https?://[^)]+\)$)"
    r"|(^via https?://)"
    r"|(^Tool: [A-Za-z0-9._-]+$)"
    r"|(^Platform: [A-Za-z0-9._-]+$)",
    re.IGNORECASE,
)
# Latin terms are anchored with \b so a real PM word ("Owner", "est.") is caught
# but an innocent substring is not ("ownership", "latest.", "Friday 5"). CJK
# terms take no \b: Python's \b sits between \w chars, and CJK is \w, so a word
# boundary never falls inside CJK text — anchoring there would match nothing.
PM_TERMS_RE = re.compile(
    r"(\bPhases? [0-9])|(\bStages? [0-9])|(\bSteps? [0-9])|(\bWeek [0-9])|(\bDay [0-9])"
    r"|(\b[0-9]+% done\b)|(\bSprint [0-9])|(\bMilestone\b)|(\best\.)|(\bestimated\b)"
    r"|(\bIn Progress\b)|(预计)|(计划)|(负责人)|(工作量)|(\bOwner\b)|(\bAssignee\b)",
    re.IGNORECASE,
)
URL_RE = re.compile(r"https?://")

# Plan/progress markers an assistant tends to emit — slice / phase / step ids
# like A1, D0, P2, A3a, R7. Case-SENSITIVE (uppercase-letter start), so lowercase
# words, the branch name `p2-...`, and identifiers like `x0` are unaffected.
# Two legitimate namespaces are exempt: G-invariant ids (G1..G29, excluded via
# the negative lookahead) and ADR-#### references (ADR is multi-letter, so a
# single-letter-then-digit token never matches inside it). `P2P`, `SHA256`,
# `ARM64` are safe too — the digits are not followed by a word boundary.
PLAN_MARKER_RE = re.compile(r"\b(?!G\d)[A-Z]-?\d{1,3}[a-z]?\b")

# Uppercase tech acronyms that share the marker shape but are legitimate nouns
# (no lowercase spelling that reads well). Compared case-folded.
PLAN_MARKER_EXEMPT = {"S3", "K8S", "L10N"}


def clean_lines(raw: str) -> list[str]:
    """Drop git comment lines and the verbose-diff scissors section."""
    lines: list[str] = []
    for line in raw.splitlines():
        if line.startswith("# ------------------------ >8"):
            break
        if line.startswith("#"):
            continue
        lines.append(line.rstrip("\r"))
    while lines and lines[-1].strip() == "":
        lines.pop()
    return lines


def check_message(raw: str) -> list[str]:
    errors: list[str] = []
    lines = clean_lines(raw)
    if not lines:
        return ["commit message is empty."]

    subject = lines[0]

    if len(lines) > MAX_LINES:
        errors.append(
            f"commit message has too many lines (max {MAX_LINES}); keep it concise."
        )
    if len(lines) > 1 and lines[1].strip() != "":
        errors.append("commit body must be separated from the subject by a blank line.")

    if len(subject) > SUBJECT_MAX:
        errors.append(f"commit subject exceeds {SUBJECT_MAX} characters.")

    match = SUBJECT_RE.match(subject)
    if not match:
        errors.append(
            "commit subject must match '<emoji> <type>(<scope>): <subject>'.\n"
            "  Example: ✨ feat(auth): add OAuth2 login"
        )
    elif match.group(1) not in ALLOWED_TYPES:
        errors.append(
            f"unknown commit type '{match.group(1)}'. "
            f"Allowed: {', '.join(sorted(ALLOWED_TYPES))}."
        )

    for line in lines[2:]:
        if len(line) > BODY_WRAP and " " in line.strip() and not URL_RE.search(line):
            errors.append(
                f"body line exceeds {BODY_WRAP} columns; wrap it:\n  {line}"
            )

    if CO_AUTHORED_RE.search(raw):
        errors.append("commit message contains 'Co-Authored-By:' which is not allowed.")
    if AI_MARKER_RE.search(raw):
        errors.append("commit message contains AI-generation markers which are not allowed.")
    for line in lines:
        if EXTERNAL_TOOL_RE.match(line):
            errors.append("commit message contains external-tool provenance markers.")
            break
    pm_hit = PM_TERMS_RE.search("\n".join(lines))
    if pm_hit:
        errors.append(
            f"commit message contains a prohibited project-management term: "
            f"{pm_hit.group(0)!r} — reword without status/phase/ownership jargon."
        )

    plan_hit = next(
        (
            m.group(0)
            for m in PLAN_MARKER_RE.finditer("\n".join(lines))
            if m.group(0).upper() not in PLAN_MARKER_EXEMPT
        ),
        None,
    )
    if plan_hit:
        errors.append(
            f"commit message contains a plan/progress marker: {plan_hit!r} — "
            f"drop slice/phase ids (A1, D0, P2, A3a) from the log; describe the change "
            f"itself. (G-invariant ids like G18 and ADR-#### are exempt.)"
        )

    return errors


def self_test() -> int:
    good = [
        "✨ feat(auth): add OAuth2 login",
        "🐛 fix(runtime): guard against empty thread commit",
        "📝 docs(wiki): summarise invariant G18\n\nClarify the boundary rule.",
        # PM terms are whole-word: these substrings must not trip the filter.
        "📝 docs(wiki): update ownership index",
        "🐛 fix(runtime): return the latest request.",
        "🔧 chore(ci): ship the fix on Friday 5pm",
        # Plan-marker exemptions: invariant ids, ADR refs, and lookalikes.
        "🔧 chore(runtime): satisfy invariant G18 boundary",
        "📝 docs(adr): record ADR-0038 resource injection",
        "✨ feat(net): add P2P transport with SHA256 digests",
        "🔧 chore(store): wire the S3 backend behind a flag",
    ]
    bad = [
        "feat: missing emoji and scope",
        "✨ feat(auth): " + "x" * 100,
        "✨ feat(auth): ok\nno blank line body",
        "✨ feat(auth): ok\n\n" + "word " * 20,
        "✨ feat(auth): ok\n\nFix.\n\nCo-Authored-By: Someone <a@b.c>",
        "✨ feat(auth): ok\n\n🤖 Generated with a tool",
        "✨ feat(auth): ok\n\nvia [HAPI](https://hapi.run)",
        "✨ feat(auth): ok\n\nOwner: alice; 预计 done",
        "✨ wat(auth): unknown type",
        "🔧 chore(runtime): land slice A3a of the resolver",
        "🔧 chore(runtime): finish D0 before the cutover",
    ]
    failures = 0
    for msg in good:
        errs = check_message(msg)
        if errs:
            failures += 1
            print(f"self-test: expected PASS but failed: {msg!r} -> {errs}", file=sys.stderr)
    for msg in bad:
        if not check_message(msg):
            failures += 1
            print(f"self-test: expected FAIL but passed: {msg!r}", file=sys.stderr)
    if failures:
        print(f"commit-message self-test: {failures} case(s) failed.", file=sys.stderr)
        return 1
    print("OK - commit-message self-test passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("msgfile", nargs="?", help="path to the commit message file")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()
    if not args.msgfile:
        parser.error("the commit message file path is required (lefthook passes {1})")

    try:
        with open(args.msgfile, encoding="utf-8") as handle:
            raw = handle.read()
    except OSError as exc:
        print(f"ERROR: cannot read commit message file: {exc}", file=sys.stderr)
        return 1

    errors = check_message(raw)
    if errors:
        print("Commit message rejected:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        print(
            "\nUse a single-line message by default; add a blank line + short body "
            "only when essential context is needed.",
            file=sys.stderr,
        )
        return 1
    print("OK - commit message passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
