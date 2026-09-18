#!/usr/bin/env python3
"""Refuse a test that waits on *who did the work* instead of on a durable fact.

Two regressions in `main` came from one shape. A test decides it is done when
something *happened* -- a lifecycle Event arrived, a fixture counter moved, a
fixed number of milliseconds elapsed -- rather than when a value the product
*committed* became readable.

* `awaken-protocol-managed/tests/session_resources.rs` held a one-shot
  activation gate before its POST because it presumed the supervisor, not the
  request, activates the Run. When the activation moved into the request the
  POST blocked in the held gate and the tests hung for 300s.
* `awaken-cli/tests/zhike_sales.rs` returned on any `session.status_idle` after
  its receipt, including one published by a concurrent background Run. ~80%
  flaky. The fix required the anchored `user.message` to carry its committed
  `processed_at`.

Neither is a timing bug: both waits named no fact. `session.status_idle` says
some Run reached a boundary, never which command caused it; `elapsed >= 300ms`
says nothing at all. A wait whose parameter is a committed value -- a
`processed_at`, an activation receipt, a revision, an outbox that drained -- is
attributable, so no concurrent worker satisfies it early and no change of which
component does the work satisfies it late.

The owners, one per side, are the only places a wait loop belongs:

* Rust: `awaken_protocol_managed::test_support::wait_for_session_events`, whose
  settle parameter is the typed `DurableFact`, and its sibling
  `wait_for_delivered_lifecycle_facts`.
* TypeScript: `e2e/harness.mjs`'s `waitForValue` (bounded poll on a committed
  projection) and `waitForSessionEventReceipt`, which requires the anchoring
  command's own `processed_at` before consulting the caller's predicate.

Three rules, each ratcheted per file so today's debt is frozen and tomorrow's
is refused. Genuine timing tests -- a timeout, a TTL, a lease expiry, a
backoff -- are legitimate and stay; they say why through an `awaken-wait:`
reason comment, which is a decision someone made and can be argued with.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

# This checker reads the repository's test trees and its own baseline, so its
# lefthook trigger is rendered from exactly that set.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/*/*/tests/**/*.rs",
    "crates/*/*/src/**/*.rs",
    "e2e/**/*.mjs",
    "e2e/**/*.ts",
    "e2e/**/*.js",
    "scripts/ci/test_waits_baseline.json",
)

# An in-crate `#[cfg(test)]` module waits on the same product the integration
# tests do, and the deepest lifecycle tests live there, so every `src` file is
# read -- but only from its first `#[cfg(test)]` onward. Production code holds
# real poll loops and real backoffs; this rule governs tests, not the product.
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")
INLINE_TEST_MODULE = re.compile(r"^\s*(?:pub(?:\([\w:]+\))?\s+)?mod\s+\w+\s*\{")

ROOT = Path(__file__).resolve().parents[2]
BASELINE = Path(__file__).resolve().parent / "test_waits_baseline.json"

RUST_OWNER = "awaken_protocol_managed::test_support::wait_for_session_events"
TS_OWNER = "e2e/harness.mjs waitForValue / waitForSessionEventReceipt"

# A wait that names one of these is reading a value some writer committed.
# `run_id`/`id ==` count because equality against an exact identity the caller
# already holds is an anchor, not an arrival signal.
DURABLE = re.compile(
    r"processed_at|processedAt|DurableFact|receipt|Receipt|revision|committed|"
    r"commitBackendVersion|_version\b|version\b|epoch|generation|sequence|lease|"
    r"anchor|completed_at|completedAt|acknowledged_at|acknowledgedAt|expires_at|"
    r"expiresAt|content_sha256|contentSha256|next_cursor|nextCursor|run_id|runId|"
    r"pending_lifecycle|pendingLifecycle|fact\b|cursor|\bid ==|\.id ===",
)

# "Some Run reached a boundary" -- true of whichever Run got there first.
LIFECYCLE = re.compile(
    r"session\.status_\w+|status_idled|"
    r"status\s*(?:===?|==)\s*['\"](?:idle|running|completed|terminated|stopped)['\"]|"
    r"\[\"status\"\]\s*==\s*[\"'](?:idle|running|completed|terminated|stopped)[\"']",
)

SLEEP = re.compile(
    r"tokio::time::sleep\s*\(|thread::sleep\s*\(|"
    r"await\s+(?:new\s+Promise\s*\([^\n]*setTimeout|sleep\s*\(|delay\s*\()|"
    r"setTimeout\s*\(\s*resolve",
)
# Defining `sleep` is not waiting on one; the definition line is not a wait.
SLEEP_DEFINITION = re.compile(
    r"^\s*(?:export\s+)?(?:const|let|var|function|async function|fn|pub fn)\s+"
    r"\w*(?:[sS]leep|[dD]elay)\b",
)
ASSERTION = re.compile(r"\bassert(?:_eq|_ne)?!|\bassert\.\w+\(|\bexpect\(")
# A poll loop counts down or spins; iterating a collection is not a wait, so
# `for x of xs` / `for x in xs` are deliberately not loop heads here.
LOOP_HEAD = re.compile(
    r"^\s*\}?\s*(?:loop\s*\{"
    r"|while[\s(]"
    r"|do\s*\{"
    r"|for\s*\(\s*(?:let|var|const|int|size_t)?\s*\w+\s*=[^;]*;"
    r"|for\s+(?:mut\s+)?[\w(),\s_]+\s+in\s+[\w:.]*\d*\.\.)",
)
LOOP_EXIT = re.compile(r"\bbreak\b|\breturn\b|\bthrow\b")
# Only a *bounded* loop can expire, so only a bounded loop can expire quietly.
# An unbounded `loop {` or `while !stop` is a fake server or a producer; it
# never reaches a deadline path at all.
BOUNDED_LOOP = re.compile(
    r"for\s+(?:mut\s+)?[\w(),\s_]+\s+in\s+[\w:.]*\d+\s*\.\.|"
    r"for\s*\(\s*(?:let|var|const)?\s*\w+\s*=[^;]*;[^;]*<[^;]*;|"
    r"while[\s(].*(?:Date\.now|performance\.now|Instant::now|elapsed)\s*\(\s*\)\s*<",
)
# A wait that expires must say so. `throw`, an assertion, or a Rust panic all
# stop the test; anything else lets the deadline pass unremarked.
LOUD_FAILURE = re.compile(
    r"\bthrow\b|\bassert(?:_eq|_ne)?!|\bassert\b|\bexpect\(|\bpanic!|\bunwrap\(|"
    r"\bexpect!|\.expect\(",
)
# A wait that delegates to an owner is not a private loop.
OWNED_WAIT = re.compile(
    r"wait_for_session_events|wait_for_delivered_lifecycle_facts|"
    r"waitForSessionEventReceipt|waitForValue|waitForPort",
)
WAIT_CALL = re.compile(
    r"wait_for_session_events\s*\(|waitForValue\s*\(|waitForSessionEventReceipt\s*\(",
)
REASON = re.compile(r"awaken-wait:\s*(?P<reason>.+?)\s*$")
MIN_REASON = 20

RULES = ("sleep-then-assert", "generic-settle", "private-wait-loop", "silent-timeout")

MESSAGES = {
    "sleep-then-assert": (
        "this wait names no durable fact -- it sleeps a fixed time and then "
        f"asserts. Settle on a committed value through {{owner}} (Rust) and anchor "
        "on the command's own processed_at, an outbox that drained, a revision, "
        "or a lifecycle fact. A real timeout/expiry test keeps its sleep and "
        "says why in an `awaken-wait: <reason>` comment above it"
    ),
    "generic-settle": (
        "this wait settles on a Session lifecycle/status signal, which names "
        "which Run finished and not which command caused it. Anchor on the "
        "accepted command's own processed_at (Rust: "
        "DurableFact::command_processed(&receipt); TypeScript: "
        "waitForSessionEventReceipt) and keep the status shape as a refinement"
    ),
    "private-wait-loop": (
        "this is a private poll loop; the repository has one wait owner per "
        "side so a settle condition cannot drift into a generic signal. Use "
        "{owner} and give it a durable fact"
    ),
    "silent-timeout": (
        "this wait's deadline path is silent -- when it expires the test simply "
        "carries on, so a product that never reaches the awaited state still "
        "reports green and verifies nothing. Fail there, naming what was "
        "awaited and showing what was observed instead; {owner} already does"
    ),
}


def sources(root: Path) -> dict[str, str]:
    """Every test source this rule governs, keyed by repository-relative path."""

    found: dict[str, str] = {}
    for path in sorted(root.glob("crates/*/*/tests/**/*.rs")):
        found[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    for path in sorted(root.glob("crates/*/*/src/**/*.rs")):
        found[path.relative_to(root).as_posix()] = test_module_of(
            path.read_text(encoding="utf-8")
        )
    for suffix in ("mjs", "ts", "js"):
        for path in sorted(root.glob(f"e2e/**/*.{suffix}")):
            if "node_modules" in path.parts:
                continue
            found[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    return found


def test_module_of(text: str) -> str:
    """`text` from its inline `#[cfg(test)] mod … {` on, earlier lines blanked.

    Only an inline test *module* opens the region. A `#[cfg(test)]` on a single
    helper, or a `#[cfg(test)] mod tests;` declaration, would otherwise pull the
    production code that follows it into a rule about tests -- and production
    code holds real poll loops and real backoffs. The declared module's own file
    is covered by the `tests/` globs.

    Blanking rather than slicing keeps every reported line number the one a
    reader will open.
    """

    lines = text.splitlines()
    start = next(
        (
            index
            for index, line in enumerate(lines)
            if CFG_TEST.match(line)
            and any(
                INLINE_TEST_MODULE.match(following)
                for following in lines[index + 1 : index + 3]
            )
        ),
        None,
    )
    if start is None:
        return ""
    return "\n".join([""] * start + lines[start:])


def _exempt(lines: list[str], index: int) -> str | None:
    """The `awaken-wait:` reason covering line `index`, if one is in force."""

    for offset in range(max(0, index - 3), index + 1):
        match = REASON.search(lines[offset])
        if match:
            return match.group("reason")
    return None


def _in_poll_loop(lines: list[str], index: int) -> bool:
    """Is line `index` inside a loop body (so the sleep is a poll interval)?"""

    for offset in range(max(0, index - 14), index):
        if LOOP_HEAD.match(lines[offset]):
            return True
    return False


def _loop_end(lines: list[str], index: int) -> int:
    """The index of the line closing the loop opened at `index`."""

    indent = len(lines[index]) - len(lines[index].lstrip())
    for offset in range(index + 1, min(len(lines), index + 60)):
        line = lines[offset]
        if not line.strip():
            continue
        if len(line) - len(line.lstrip()) <= indent and line.strip().startswith(("}", "end")):
            return offset
    return min(len(lines) - 1, index + 60)


def _fails_loudly_on_deadline(lines: list[str], index: int) -> bool:
    """Does the loop opened at `index` report its own expiry?

    The deadline path is whatever runs once the loop falls through. It is loud
    when the loop itself throws/asserts, when the code just after it does, or
    when the loop delegates to an owner whose timeout already fails.
    """

    end = _loop_end(lines, index)
    region = _code(lines, index, end - index + 1) + "\n" + _code(lines, end + 1, 6)
    return bool(LOUD_FAILURE.search(region) or OWNED_WAIT.search(region))


def _window(lines: list[str], index: int, span: int = 40) -> str:
    return "\n".join(lines[index : index + span])


def _code(lines: list[str], index: int, span: int) -> str:
    """`_window` with comment lines removed: prose about a wait is not a wait."""

    return "\n".join(
        line
        for line in lines[index : index + span]
        if not line.strip().startswith(("//", "#", "*", "///"))
    )


# Every rule needs either a sleep (both sleep rules) or a status word (the
# lifecycle rule), so a file carrying neither is skipped whole rather than
# windowed line by line.
CANDIDATE = re.compile(r"sleep|setTimeout|status")


def file_violations(relative: str, text: str) -> list[tuple[str, int, str]]:
    """Every (rule, line number, source line) this file breaks."""

    if not CANDIDATE.search(text):
        return []
    lines = text.splitlines()
    found: list[tuple[str, int, str]] = []
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith(("//", "#", "*", "///")):
            continue

        if LOOP_HEAD.match(line):
            body = _window(lines, index, 20)
            if SLEEP.search(body) and LOOP_EXIT.search(body) and not OWNED_WAIT.search(body):
                found.append(("private-wait-loop", index + 1, stripped))
            if (
                SLEEP.search(body)
                and BOUNDED_LOOP.search(line)
                and not _fails_loudly_on_deadline(lines, index)
            ):
                found.append(("silent-timeout", index + 1, stripped))

        if (
            SLEEP.search(line)
            and not SLEEP_DEFINITION.match(line)
            and not _in_poll_loop(lines, index)
        ):
            ahead = _code(lines, index + 1, 6)
            if ASSERTION.search(ahead):
                found.append(("sleep-then-assert", index + 1, stripped))

        if WAIT_CALL.search(line) or LOOP_HEAD.match(line):
            body = _window(lines, index, 24)
            if LIFECYCLE.search(body) and not DURABLE.search(body):
                found.append(("generic-settle", index + 1, stripped))

    return [
        (rule, number, source)
        for rule, number, source in found
        if _exempt(lines, number - 1) is None
    ]


def short_reasons(text: str) -> list[str]:
    """`awaken-wait:` markers that do not actually say why."""

    return [
        match.group("reason")
        for match in (REASON.search(line) for line in text.splitlines())
        if match and len(match.group("reason")) < MIN_REASON
    ]


def owner_for(relative: str) -> str:
    return RUST_OWNER if relative.endswith(".rs") else TS_OWNER


def load_baseline(path: Path) -> tuple[dict[str, dict[str, int]], list[dict[str, str]]]:
    document = json.loads(path.read_text(encoding="utf-8"))
    return document.get("baseline", {}), document.get("allow", [])


def allowed(allow: list[dict[str, str]]) -> set[tuple[str, str]]:
    """(`path`, `rule`) pairs a registered decision exempts, reason required."""

    exempt: set[tuple[str, str]] = set()
    for entry in allow:
        reason = entry.get("reason", "")
        if len(reason) < MIN_REASON:
            raise SystemExit(
                f"check_test_waits: allowlist entry {entry.get('path')} "
                f"/ {entry.get('rule')} must say why in at least {MIN_REASON} characters"
            )
        exempt.add((entry["path"], entry["rule"]))
    return exempt


def report(
    found: dict[str, list[tuple[str, int, str]]],
    baseline: dict[str, dict[str, int]],
    exempt: set[tuple[str, str]],
) -> tuple[list[str], list[str]]:
    """Errors (new or unregistered waits) and ratchet notes (ground gained)."""

    errors: list[str] = []
    gained: list[str] = []
    counted: dict[str, dict[str, int]] = {rule: {} for rule in RULES}
    for relative, violations in found.items():
        for rule, number, source in violations:
            if (relative, rule) in exempt:
                continue
            counted[rule][relative] = counted[rule].get(relative, 0) + 1
            if counted[rule][relative] > baseline.get(rule, {}).get(relative, 0):
                message = MESSAGES[rule].format(owner=owner_for(relative))
                errors.append(f"{relative}:{number}: [{rule}] {message}\n    {source}")
    for rule in RULES:
        for relative, allowance in sorted(baseline.get(rule, {}).items()):
            if (relative, rule) in exempt:
                continue
            now = counted[rule].get(relative, 0)
            if now < allowance:
                gained.append(
                    f"{relative}: [{rule}] {now} left, baseline {allowance}. "
                    f"Lower the baseline to {now} so the ground gained is held."
                )
    return errors, gained


def selftest() -> None:
    """Decision table for the three rules.

    | Rule | Cause                                              | Effect |
    |------|----------------------------------------------------|--------|
    | S1   | fixed sleep, then an assertion                      | reject |
    | S2   | same, carrying an `awaken-wait:` reason             | accept |
    | S3   | sleep as the poll interval of a loop                | not S1 |
    | S4   | wait settles on a lifecycle status, no durable value | reject |
    | S5   | same wait also naming a committed value             | accept |
    | S6   | private poll loop with a sleep and an exit          | reject |
    | S7   | loop delegating to an owned wait                    | accept |
    | S8   | an `awaken-wait:` marker that says nothing          | reject |
    | S9   | a `src` file's inline `#[cfg(test)] mod … {`        | in scope |
    | S10  | a `#[cfg(test)]` on one item, or `mod tests;`       | out of scope |
    | S11  | bounded poll loop that just falls through           | reject |
    | S12  | same loop throwing, asserting, or panicking at exit | accept |
    | S13  | unbounded loop (a fake server, a producer)          | not S11 |
    """

    def rules(text: str) -> set[str]:
        return {rule for rule, _, _ in file_violations("a.rs", text)}

    s1 = "    tokio::time::sleep(d).await;\n    assert_eq!(calls, 1);\n"
    assert "sleep-then-assert" in rules(s1), "S1"

    s2 = (
        "    // awaken-wait: the lease TTL itself is the subject of this test\n"
        "    tokio::time::sleep(d).await;\n"
        "    assert_eq!(calls, 1);\n"
    )
    assert "sleep-then-assert" not in rules(s2), "S2"

    s3 = (
        "    loop {\n"
        "        if store.revision(id).await == 2 { break; }\n"
        "        tokio::time::sleep(d).await;\n"
        "    }\n"
        "    assert_eq!(calls, 1);\n"
    )
    assert "sleep-then-assert" not in rules(s3), "S3"

    s4 = (
        "    wait_for_session_events(\n"
        '        |events| events.iter().any(|e| e["type"] == "session.status_idle"),\n'
        "    );\n"
    )
    assert "generic-settle" in rules(s4), "S4"

    s5 = (
        "    wait_for_session_events(\n"
        "        DurableFact::command_processed(&receipt),\n"
        '        |events| events.iter().any(|e| e["type"] == "session.status_idle"),\n'
        "    );\n"
    )
    assert "generic-settle" not in rules(s5), "S5"

    s6 = (
        "    loop {\n"
        "        if inbox.lock().unwrap().len() >= 1 { break; }\n"
        "        tokio::time::sleep(d).await;\n"
        "    }\n"
    )
    assert "private-wait-loop" in rules(s6), "S6"

    s7 = (
        "    for attempt in 0..3 {\n"
        "        let seen = waitForValue(read, predicate, 'x');\n"
        "        if seen { break; }\n"
        "        await sleep(5);\n"
        "    }\n"
    )
    assert "private-wait-loop" not in rules(s7), "S7"

    production = (
        "#[cfg(test)]\n"
        "mod tests;\n"
        "\n"
        "async fn drain(&self) {\n"
        "    tokio::time::sleep(self.backoff).await;\n"
        "    assert_eq!(self.pending(), 0);\n"
        "}\n"
    )
    assert test_module_of(production) == "", "S10"

    single_item = (
        "#[cfg(test)]\n"
        "fn probe() -> u8 { 1 }\n"
        "async fn drain(&self) {\n"
        "    tokio::time::sleep(self.backoff).await;\n"
        "    assert_eq!(self.pending(), 0);\n"
        "}\n"
    )
    assert test_module_of(single_item) == "", "S10"

    inline = (
        "pub fn product() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    tokio::time::sleep(d).await;\n"
        "    assert_eq!(calls, 1);\n"
        "}\n"
    )
    scoped = test_module_of(inline)
    assert scoped.splitlines()[0] == "", "S9 line numbers are preserved"
    assert "sleep-then-assert" in rules(scoped), "S9"

    s11 = (
        "async function until(read, tries = 50) {\n"
        "  for (let i = 0; i < tries; i += 1) {\n"
        "    const seen = await read();\n"
        "    if (seen) return seen;\n"
        "    await sleep(100);\n"
        "  }\n"
        "  return null;\n"
        "}\n"
    )
    assert "silent-timeout" in rules(s11), "S11"

    s12 = (
        "async function until(read, tries = 50) {\n"
        "  for (let i = 0; i < tries; i += 1) {\n"
        "    const seen = await read();\n"
        "    if (seen) return seen;\n"
        "    await sleep(100);\n"
        "  }\n"
        "  throw new Error('the committed revision never advanced');\n"
        "}\n"
    )
    assert "silent-timeout" not in rules(s12), "S12"

    s13 = (
        "    while !stop.load(Ordering::Acquire) {\n"
        "        std::thread::sleep(Duration::from_millis(2));\n"
        "    }\n"
        "    return;\n"
    )
    assert "silent-timeout" not in rules(s13), "S13"

    assert short_reasons("// awaken-wait: flaky\n"), "S8"
    assert not short_reasons(
        "// awaken-wait: the lease TTL itself is the subject of this test\n"
    ), "S8"

    baseline = {"sleep-then-assert": {"a.rs": 1}}
    one = {"a.rs": [("sleep-then-assert", 3, "sleep(d)")]}
    two = {"a.rs": [("sleep-then-assert", 3, "sleep(d)"), ("sleep-then-assert", 9, "sleep(d)")]}
    assert report(one, baseline, set()) == ([], []), "ratchet holds at the baseline"
    assert report(two, baseline, set())[0], "a new wait above the baseline is refused"
    assert report({}, baseline, set())[1], "ground gained must lower the baseline"
    assert report(two, baseline, {("a.rs", "sleep-then-assert")}) == ([], []), "registered"
    print("check_test_waits selftest: the decision table holds")


def main() -> int:
    baseline, allow = load_baseline(BASELINE)
    exempt = allowed(allow)
    found: dict[str, list[tuple[str, int, str]]] = {}
    empty_reasons: list[str] = []
    for relative, text in sources(ROOT).items():
        violations = file_violations(relative, text)
        if violations:
            found[relative] = violations
        for reason in short_reasons(text):
            empty_reasons.append(
                f"{relative}: `awaken-wait: {reason}` must say why in at least "
                f"{MIN_REASON} characters"
            )
    errors, gained = report(found, baseline, exempt)
    for problem in empty_reasons + errors + gained:
        print(f"ERROR: {problem}", file=sys.stderr)
    if empty_reasons or errors or gained:
        print(
            f"\nRegenerate the ratchet with `python3 {Path(__file__).name} --write-baseline` "
            "only after the new waits are converted, never to admit them.",
            file=sys.stderr,
        )
        return 1
    total = sum(len(v) for v in found.values())
    print(f"OK - test waits held at the baseline ({total} registered, none new).")
    return 0


def write_baseline() -> int:
    """Rewrite the ratchet from the working tree (a deliberate, reviewed act)."""

    _, allow = load_baseline(BASELINE)
    exempt = allowed(allow)
    counted: dict[str, dict[str, int]] = {rule: {} for rule in RULES}
    for relative, text in sources(ROOT).items():
        for rule, _, _ in file_violations(relative, text):
            if (relative, rule) in exempt:
                continue
            counted[rule][relative] = counted[rule].get(relative, 0) + 1
    document = {
        "baseline": {rule: dict(sorted(paths.items())) for rule, paths in counted.items()},
        "allow": allow,
    }
    BASELINE.write_text(json.dumps(document, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    print(f"wrote {BASELINE.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        selftest()
        raise SystemExit(0)
    if "--write-baseline" in sys.argv:
        raise SystemExit(write_baseline())
    raise SystemExit(main())
