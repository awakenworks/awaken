#!/usr/bin/env python3
"""Refuse an observation time that was invented rather than observed.

Two shapes produced the same defect, and the second is what made the first
attractive.

**R1 — a fabricated constant reaching an observation field.**  A module declared
``pub const OBJECT_AT: &str = "2026-01-01T00:00:00Z"`` and stamped it on
``created_at``/``started_at``/``archived_at``.  Every client then read an
invented instant that was identical on every install, for every record, forever.
It looked harmless because the wire shape was satisfied — which is exactly why
it survived a vault-side fix that removed one copy and left eight behind.

**R2 — an ambient clock.**  ``SystemTime::now()`` called deep inside a command
cannot be named by a test, so the honest value is untestable and the frozen
constant becomes the *testable* option.  Injecting the clock removes the
incentive: a store takes an ``awaken_agent_contract::clock::Clock`` at
construction, a test hands it a ``ManualClock``, and the durable value is a fact
the test names.  So the clock may be read only where the port is declared or
where the program is composed; everywhere else it arrives as an argument.

Both rules are ratcheted **per file** against a baseline, because converting
every instance at once would be a migration rather than a fix, and a whole-file
allow-list would freeze today's debt while guarding nothing.  A file may lose
occurrences and may not gain them, and a file absent from the baseline may have
none at all.

Every baseline entry carries an **owner and an expiry date**.  A ratchet with no
end date is how a temporary allowance becomes permanent: the number stops
falling, nobody owns it, and the gate quietly certifies the debt instead of
retiring it.  An entry past its expiry fails this check, which forces the
decision — finish the conversion, or re-argue the allowance and re-date it.
"""

from __future__ import annotations

import datetime as dt
import argparse
import json
import re
import sys
from pathlib import Path

INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    "scripts/ci/fabricated_facts_baseline.json",
    # The expiry rule below reads the other gate's ratchet record, so an edit
    # there must re-run this check.
    "scripts/ci/check_degrading_defaults.py",
)

ROOT = Path(__file__).resolve().parents[2]

# ── R1: a date/time literal reaching a field that names an observation time ──

# Field names that assert *when something was observed*. A `*_at` name, and the
# bare participles the wire vocabulary uses. `expires*` is included because an
# invented expiry is an invented authority window, not just an invented display.
OBSERVATION_FIELD = re.compile(
    r"\b([A-Za-z_][A-Za-z0-9_]*_at|created|updated|expires[A-Za-z0-9_]*)\b"
)

# An RFC-3339-shaped literal: the shape the frozen constant took. A duration, a
# version or a plain number is not one, so this does not fire on `"1.0.0"`.
DATETIME_LITERAL = re.compile(r'"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}')

# A `const`/`static` whose *name* denotes an observation time and whose value is
# such a literal — the exact declaration this eradication removed. Caught even
# when the assignment to a field happens in another file.
OBSERVATION_CONST = re.compile(
    r"(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+"
    r"(?P<name>[A-Z][A-Z0-9_]*(?:_AT|_TIME|_TIMESTAMP|_CREATED|_UPDATED|_EXPIRES))\b"
    r"[^=;]*=\s*(?P<value>\"[^\"]*\")"
)

# ── R2: reading the wall clock where it was not injected ─────────────────────

AMBIENT_CLOCK = re.compile(r"\b(?:SystemTime|Instant)::now\s*\(\s*\)")

# A file that *declares* a clock port may read the real clock: that is what
# implementing `Clock for SystemClock` means. So may a composition root, which
# is where the program chooses its real-world edges.
CLOCK_PORT_DECLARATION = re.compile(
    r"\b(?:trait\s+\w*Clock\b|impl\s+(?:\w+\s+for\s+)?\w*Clock\b"
    r"|fn\s+system_clock\s*\(|impl\s+Clock\s+for\b)"
)

# Composition roots: the binaries and the modules whose whole job is to build
# the object graph. Named, not pattern-matched, so adding one is a decision.
COMPOSITION_ROOTS: frozenset[str] = frozenset(
    {
        "crates/bin/awaken-cli/src/process_stores.rs",
        "crates/bin/awaken-cli/src/local_process_stores.rs",
    }
)


# The per-file counts each rule still allows, with the owner accountable for
# retiring them and the date by which that must happen. Data, not code, because
# the file lists are long and because a diff to a count is then the reviewable
# artefact.
BASELINE_PATH = ROOT / "scripts" / "ci" / "fabricated_facts_baseline.json"

RULES: tuple[tuple[str, str], ...] = (
    ("fabricated_observation_literal", "fabricated observation literal"),
    ("ambient_clock_read", "ambient clock read"),
)

# Every ratchet baseline in this repository must carry an owner and an expiry.
# `check_degrading_defaults.py` holds its own, in the same shape, and is checked
# here so a single expiry rule covers both gates rather than each inventing one.
EXTERNAL_RATCHETS: tuple[tuple[str, str, str], ...] = (
    (
        "scripts/ci/check_degrading_defaults.py",
        "UNTRIAGED_BASELINE",
        "DEGRADING_DEFAULT_RATCHET",
    ),
)


def _today() -> dt.date:
    return dt.date.today()


def load_baseline(path: Path) -> dict[str, dict]:
    return json.loads(path.read_text(encoding="utf-8"))["rules"]


def expired_entries(baseline: dict[str, dict], today: dt.date) -> list[str]:
    """Every rule allowance whose expiry has passed, is unusable, or is unowned."""
    errors: list[str] = []
    for rule, record in sorted(baseline.items()):
        if not str(record.get("owner", "")).strip():
            errors.append(f"{rule}: baseline has no owner; a ratchet nobody owns never falls")
        if not str(record.get("reason", "")).strip():
            errors.append(f"{rule}: baseline records no reason for the remaining occurrences")
        raw = str(record.get("expires", ""))
        try:
            expires = dt.date.fromisoformat(raw)
        except ValueError:
            errors.append(f"{rule}: baseline expiry {raw!r} is not an ISO date (YYYY-MM-DD)")
            continue
        if expires < today:
            held = sum(record.get("files", {}).values())
            errors.append(
                f"{rule}: the baseline of {held} occurrence(s) expired on {raw} "
                f"(owner {record.get('owner')}). Convert the remainder, or re-argue the "
                "allowance and record a new owner and expiry — a ratchet with a past expiry "
                "is permanent debt, which is exactly what the date exists to prevent"
            )
    return errors


def _strip_test_code(text: str) -> str:
    """Blank out `#[cfg(test)]` and `#[cfg(any(test, ...))]` items and `mod tests`.

    A literal inside a test is a fixture naming an expected value; it never
    reaches a client. Replacing the span with newlines keeps line numbers exact.
    """
    out = list(text)
    for match in re.finditer(
        r"#\[cfg\((?:test|any\([^)]*\btest\b[^)]*\)|all\([^)]*\btest\b[^)]*\))\)\]", text
    ):
        start = text.find("{", match.end())
        if start < 0:
            continue
        depth = 0
        index = start
        while index < len(text):
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
                if depth == 0:
                    break
            index += 1
        for position in range(match.start(), min(index + 1, len(text))):
            if out[position] != "\n":
                out[position] = " "
    return "".join(out)


def _is_test_path(relpath: str) -> bool:
    return (
        "/tests/" in relpath
        or relpath.endswith(("_tests.rs", "tests.rs", "test_support.rs", "testkit.rs"))
        or "/test_support/" in relpath
        or "-testkit/" in relpath
    )


def fabricated_literal_violations(sources: dict[str, str]) -> dict[str, list[str]]:
    """R1 findings, keyed by file."""
    findings: dict[str, list[str]] = {}
    for relpath, text in sorted(sources.items()):
        if _is_test_path(relpath):
            continue
        production = _strip_test_code(text)
        hits: list[str] = []
        for number, line in enumerate(production.splitlines(), start=1):
            stripped = line.lstrip()
            if stripped.startswith("//") or stripped.startswith("*"):
                continue
            literal = DATETIME_LITERAL.search(line)
            if literal is None:
                continue
            if OBSERVATION_CONST.search(line):
                hits.append(
                    f"{relpath}:{number}: a constant named for an observation time is "
                    "assigned a date literal. A record's time is a fact its owner observes, "
                    "not a value the code chooses; take a Clock and stamp the reading"
                )
                continue
            if OBSERVATION_FIELD.search(line[: literal.start()]):
                hits.append(
                    f"{relpath}:{number}: a date literal reaches a field that names an "
                    "observation time. Read it from the injected Clock at the event that "
                    "creates or updates the record, or make its absence explicit (Option)"
                )
        if hits:
            findings[relpath] = hits
    return findings


def ambient_clock_violations(sources: dict[str, str]) -> dict[str, list[str]]:
    """R2 findings, keyed by file."""
    findings: dict[str, list[str]] = {}
    for relpath, text in sorted(sources.items()):
        if _is_test_path(relpath) or relpath in COMPOSITION_ROOTS:
            continue
        if CLOCK_PORT_DECLARATION.search(text):
            continue
        production = _strip_test_code(text)
        hits = [
            f"{relpath}:{number}: {match.group(0)} is read where no clock port is declared. "
            "Take an `awaken_agent_contract::clock::Clock` (or the caller's sampled instant) "
            "so a test can name the value this writes"
            for number, line in enumerate(production.splitlines(), start=1)
            if not line.lstrip().startswith("//")
            for match in [AMBIENT_CLOCK.search(line)]
            if match is not None
        ]
        if hits:
            findings[relpath] = hits
    return findings


def ratchet_errors(
    rule: str,
    findings: dict[str, list[str]],
    allowed_per_file: dict[str, int],
) -> list[str]:
    """Per-file ratchet: a count may fall, never rise, and a stale entry is reported."""
    errors: list[str] = []
    for relpath, hits in sorted(findings.items()):
        allowed = allowed_per_file.get(relpath, 0)
        if len(hits) > allowed:
            errors.extend(f"ERROR: {hit}" for hit in hits)
            errors.append(
                f"{relpath}: {len(hits)} {rule} occurrence(s), baseline {allowed}: "
                f"{len(hits) - allowed} new one(s)."
            )
    for relpath, allowed in sorted(allowed_per_file.items()):
        actual = len(findings.get(relpath, ()))
        if actual < allowed:
            errors.append(
                f"{relpath}: {actual} {rule} occurrence(s), below its baseline of "
                f"{allowed}. Lower the baseline to {actual} so the ground gained is held."
            )
    return errors


def external_ratchet_errors(root: Path) -> list[str]:
    """Every other ratchet in scripts/ci must carry an owner and an expiry too."""
    errors: list[str] = []
    for relpath, number_name, ratchet_name in EXTERNAL_RATCHETS:
        path = root / relpath
        if not path.exists():
            errors.append(f"{relpath}: named by EXTERNAL_RATCHETS but missing")
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        if not re.search(rf"^{number_name}\b", text, re.MULTILINE):
            errors.append(f"{relpath}: no {number_name} to hold an owner and an expiry")
            continue
        match = re.search(
            rf"^{ratchet_name}\s*(?::[^=]*)?=\s*\{{.*?"
            r"\"owner\"\s*:\s*\"(?P<owner>[^\"]*)\".*?"
            r"\"expires\"\s*:\s*\"(?P<expires>[^\"]*)\"",
            text,
            re.MULTILINE | re.DOTALL,
        )
        if match is None:
            errors.append(
                f"{relpath}: {number_name} has no {ratchet_name} recording an owner and an "
                "expiry beside it. A ratchet nobody owns and nothing dates is permanent debt"
            )
            continue
        try:
            expires = dt.date.fromisoformat(match.group("expires"))
        except ValueError:
            errors.append(f"{relpath}: {ratchet_name} expiry is not an ISO date")
            continue
        if not match.group("owner").strip():
            errors.append(f"{relpath}: {ratchet_name} has no owner")
        if expires < _today():
            errors.append(
                f"{relpath}: {ratchet_name} expired on {match.group('expires')} "
                f"(owner {match.group('owner')}). Convert the remainder or re-date it"
            )
    return errors


def _sources(root: Path) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): path.read_text(encoding="utf-8", errors="ignore")
        for path in sorted((root / "crates").rglob("*.rs"))
    }


def selftest() -> None:
    """Decision tables for R1, R2 and the expiry rule.

    R1 causes: L1 a date literal assigned to an `*_at` field; L2 a constant
    named for an observation time holding one; L3 the same literal inside
    `#[cfg(test)]`; L4 the same inside a `tests/` file; L5 a date literal that
    reaches no observation field (a migration default keyed by another name, a
    version string); L6 a literal in a comment.
    R2 causes: C1 `SystemTime::now()` in an ordinary module; C2 `Instant::now()`
    likewise; C3 either inside a file declaring a clock port; C4 either in a
    named composition root; C5 either inside `#[cfg(test)]`.
    Effects: E1 admitted, E2 reported.
    Rules: R1: L1|L2 => E2; L3|L4|L5|L6 => E1.
           R2: C1|C2 => E2; C3|C4|C5 => E1.
           Expiry: X1 a future expiry with an owner => E1; X2 a past expiry =>
           E2; X3 a malformed expiry => E2; X4 an empty owner => E2.
    """
    # ── R1 ──
    l1 = {"a.rs": 'fn f() { let item = Item { created_at: "2026-01-01T00:00:00Z".into() }; }\n'}
    assert fabricated_literal_violations(l1), "L1/E2"

    l2 = {"a.rs": 'pub const OBJECT_AT: &str = "2026-01-01T00:00:00Z";\n'}
    assert fabricated_literal_violations(l2), "L2/E2"

    l3 = {
        "a.rs": "#[cfg(test)]\nmod tests {\n"
        '    fn fixture() { let _ = Item { created_at: "2026-01-01T00:00:00Z".into() }; }\n'
        "}\n"
    }
    assert not fabricated_literal_violations(l3), "L3/E1"

    l4 = {
        "crates/x/tests/conformance.rs": 'let _ = Item { created_at: "2026-01-01T00:00:00Z".into() };\n'
    }
    assert not fabricated_literal_violations(l4), "L4/E1"

    l5 = {
        "a.rs": 'const SEED_RELEASE: &str = "2026-01-01T00:00:00Z";\n'
        'let sql = "ALTER TABLE t ADD COLUMN label TEXT DEFAULT \'2026-01-01T00:00:00Z\'";\n'
        'const VERSION: &str = "1.0.0";\n'
    }
    assert not fabricated_literal_violations(l5), "L5/E1"

    l6 = {"a.rs": '// created_at used to be "2026-01-01T00:00:00Z" here\n'}
    assert not fabricated_literal_violations(l6), "L6/E1"

    # ── R2 ──
    c1 = {"a.rs": "fn stamp() -> u64 { SystemTime::now().elapsed() }\n"}
    assert ambient_clock_violations(c1), "C1/E2"

    c2 = {"a.rs": "fn started() -> Instant { Instant::now() }\n"}
    assert ambient_clock_violations(c2), "C2/E2"

    c3 = {
        "clock.rs": "pub trait Clock { fn now_ms(&self) -> u64; }\n"
        "impl Clock for SystemClock { fn now_ms(&self) -> u64 { SystemTime::now().x() } }\n"
    }
    assert not ambient_clock_violations(c3), "C3/E1"

    root = sorted(COMPOSITION_ROOTS)[0]
    assert not ambient_clock_violations({root: "let t = Instant::now();\n"}), "C4/E1"

    c5 = {"a.rs": "#[cfg(test)]\nmod tests {\n    fn t() { let _ = Instant::now(); }\n}\n"}
    assert not ambient_clock_violations(c5), "C5/E1"

    # ── Ratchet arithmetic ──
    findings = {"a.rs": ["one", "two"]}
    assert not ratchet_errors("R2", findings, {"a.rs": 2}), "a held ratchet is admitted"
    assert ratchet_errors("R2", findings, {"a.rs": 1}), "a rise is refused"
    assert ratchet_errors("R2", {"a.rs": ["one"]}, {"a.rs": 2}), "a fall must lower the baseline"
    assert ratchet_errors("R2", findings, {}), "an unlisted file gets no allowance"

    # ── Expiry: what stops a ratchet from becoming permanent ──
    today = dt.date(2026, 9, 19)

    def rule(**overrides: object) -> dict[str, dict]:
        record = {
            "owner": "chaizhenhua",
            "expires": "2099-01-01",
            "reason": "pre-existing",
            "files": {"a.rs": 1},
        }
        record.update(overrides)
        return {"R2": record}

    assert not expired_entries(rule(), today), "X1/E1"
    assert expired_entries(rule(expires="2026-09-18"), today), "X2/E2"
    assert expired_entries(rule(expires="soon"), today), "X3/E2"
    assert expired_entries(rule(owner="  "), today), "X4/E2"
    assert expired_entries(rule(reason=""), today), "X5/E2 an unargued allowance"
    # The live baseline must itself satisfy the rule it enforces.
    assert not expired_entries(load_baseline(BASELINE_PATH), today), "the shipped baseline"

    print("check_fabricated_facts selftest: R1, R2 and the expiry table hold")


def findings_by_rule(sources: dict[str, str]) -> dict[str, dict[str, list[str]]]:
    return {
        "fabricated_observation_literal": fabricated_literal_violations(sources),
        "ambient_clock_read": ambient_clock_violations(sources),
    }


def main() -> int:
    baseline = load_baseline(BASELINE_PATH)
    findings = findings_by_rule(_sources(ROOT))
    problems: list[str] = []
    for key, label in RULES:
        allowed = {path: int(n) for path, n in baseline.get(key, {}).get("files", {}).items()}
        problems += ratchet_errors(label, findings[key], allowed)
    problems += expired_entries(baseline, _today())
    problems += external_ratchet_errors(ROOT)
    if problems:
        for problem in problems:
            print(problem if problem.startswith("ERROR: ") else f"ERROR: {problem}", file=sys.stderr)
        return 1
    summary = ", ".join(
        f"{label}: {sum(baseline.get(key, {}).get('files', {}).values())} held across "
        f"{len(baseline.get(key, {}).get('files', {}))} file(s) until "
        f"{baseline.get(key, {}).get('expires')}"
        for key, label in RULES
    )
    print(f"OK - no invented observation times beyond the recorded baseline ({summary}).")
    return 0


if __name__ == "__main__":
    # Strict, one spelling per flag: a misspelling must refuse, never fall
    # through to the default scan and report a green it did not earn.
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true",
                        help="run the decision table instead of scanning")
    parser.add_argument("--report", action="store_true",
                        help="print each rule's findings per file")
    arguments = parser.parse_args()
    if arguments.self_test:
        selftest()
        raise SystemExit(0)
    if arguments.report:
        for key, label in RULES:
            for relpath, hits in sorted(findings_by_rule(_sources(ROOT))[key].items()):
                print(f"{label}\t{relpath}\t{len(hits)}")
        raise SystemExit(0)
    raise SystemExit(main())
