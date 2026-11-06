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

* Rust: `awaken_protocol_managed_testkit::wait_for_session_events`, whose
  settle parameter is the typed `DurableFact`, and its sibling
  `wait_for_delivered_lifecycle_facts`.
* TypeScript: `e2e/harness.mjs`'s `waitForValue` (bounded poll on a committed
  projection) and `waitForSessionEventReceipt`, which requires the anchoring
  command's own `processed_at` before consulting the caller's predicate.
* Shell has no shared wait library to delegate to, so its owner is a shape
  rather than a symbol: a named `wait_for_…` helper that polls a committed fact
  and reports its own expiry. It therefore carries three of the four rules and
  not `private-wait-loop`, which would otherwise name every correct poll.

Four rules, each ratcheted per file so today's debt is frozen and tomorrow's
is refused. Genuine timing tests -- a timeout, a TTL, a lease expiry, a
backoff -- are legitimate and stay; they say why through an `awaken-allow(wait):`
reason comment, which is a decision someone made and can be argued with.

The corpus is the `INPUT_GLOBS` declaration, walked -- not a second list beside
it. It was a second list, and the two disagreed: the globs covered `*.rs` and
`*.mjs`, so fourteen shell suites and the console's Playwright specs were never
read at all. Two of those suites fell out of a readiness poll with no failure
branch and ran their load profile against a port nothing was listening on, and
the rule that exists to refuse exactly that reported OK for as long as it had
been written.

Default/`--check` is read-only and requires the measured debt to equal its
baseline. `--tighten` and `--write-baseline` record decreases only, against an
existing baseline; neither may admit a new unexempted wait, even with `--reason`.
The shared CLI owns parsing and JSON IO, while this guard retains that rule.
Convert the wait to its durable owner, or document the existing genuine timing
exemption, before recording a decrease. No CLI reason waives a hard rule:

* `wait`: `awaken-allow(wait): <reason>` documents the existing genuine timing
  exemption on the same or one of the three preceding lines. The source reason
  has at least 20 characters; another rule's marker never exempts a wait.
* `wait-reason`: every matched `awaken-allow(wait):` reason must have at least 20
  characters, including markers whose waits the scanner already exempted.
* `baseline-present`: the tracked test-waits baseline must already exist;
  restore it rather than initialize a new allowance for unconverted waits.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

import _architecture_ratchet
import formal_sources

# This checker reads the repository's test trees and its own baseline, so its
# lefthook trigger is rendered from exactly that set.
#
# `sources()` reads exactly these patterns: the declaration is the scan scope,
# not a second copy of it. A corpus that is tested but absent here -- the shell
# e2e suites and the console's Playwright specs both were -- is a corpus where
# the rule silently never ran.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/tests/**/*.rs",
    "crates/**/src/**/*.rs",
    "e2e/**/*.mjs",
    "e2e/**/*.ts",
    "e2e/**/*.js",
    "e2e/**/*.sh",
    "e2e/**/*.rs",
    "web/e2e/**/*.ts",
    "web/e2e/**/*.mjs",
    "scripts/ci/test_waits_baseline.json",
    # CLI imports and the generator/navigation self-test read the Python
    # owners as source; these data inputs never widen the dialect corpus.
    "scripts/ci/*.py",
    "lefthook.yml",
)

# An in-crate `#[cfg(test)]` module waits on the same product the integration
# tests do, and the deepest lifecycle tests live there, so every `src` file is
# read -- but only from its first `#[cfg(test)]` onward. Production code holds
# real poll loops and real backoffs; this rule governs tests, not the product.
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")
INLINE_TEST_MODULE = re.compile(r"^\s*(?:pub(?:\([\w:]+\))?\s+)?mod\s+\w+\s*\{")

ROOT = Path(__file__).resolve().parents[2]
BASELINE = Path(__file__).resolve().parent / "test_waits_baseline.json"

RUST_OWNER = "awaken_protocol_managed_testkit::wait_for_session_events"
TS_OWNER = "e2e/harness.mjs waitForValue / waitForSessionEventReceipt"
# Shell suites have no cross-script library to own their waits, so the owner is
# a shape rather than a symbol: one named `wait_for_…` helper per script that
# polls a committed fact and reports its own expiry (`e2e/graceful_drain_e2e.sh`
# `wait_for_assistants` is the worked example).
SHELL_OWNER = "a named wait_for_… helper that polls a committed fact and fails at its deadline"

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
MIN_REASON = 20

# ---------------------------------------------------------------------------
# Shell. The same four rules, spelled in the grammar the e2e suites are written
# in. `e2e/k3d/distributed_control_e2e.sh` alone held eight bare `sleep 1`s
# between an action and the assertion about its effect; none of them was ever
# read by this rule, because the rule's scope said `*.rs` and `*.mjs`.

# A bare `sleep`, not `sleep_until()`, not `k3d_sleep`, not the word in prose.
SHELL_SLEEP = re.compile(r"(?<![\w.$-])sleep\s+[\w.\"'${]")
SHELL_SLEEP_DEFINITION = re.compile(r"^\s*(?:function\s+)?[\w-]*(?:sleep|delay)[\w-]*\s*\(\s*\)")
# `[ … ]`, `[[ … ]]`, a `|| fail`, or a comparison against an expected count.
SHELL_ASSERTION = re.compile(
    r"^\s*\[\[?[\s(]|\|\|\s*(?:fail|die)\b|\|\|\s*(?:exit|return)\s+[1-9]|"
    r"^\s*assert\w*\s|\btest\s+(?:-\w|\"\$)",
)
# A wait spins or counts down. Iterating a word list (`for d in ingress egress`),
# an array (`for t in "${threads[@]}"`) or a command's output lines
# (`while read -r …`) is a traversal, not a wait, so those are not loop heads.
SHELL_LOOP_HEAD = re.compile(
    r"^\s*(?:for\s*\(\(|for\s+\w+\s+in\s+(?:\{\d+\s*\.\.|\$\(\s*seq\b)|"
    r"while(?!\s+read\b)[\s(]|until[\s(])",
)
# `name() {` — shell's only function form.
SHELL_DEFINITION = re.compile(r"^\s*(?:function\s+)?(?P<name>[\w-]+)\s*\(\s*\)")
# Only a loop that can run out can run out quietly: a `{1..N}` range, a counted
# C-style loop, a `$(seq …)` list, or a wall-clock deadline comparison.
SHELL_BOUNDED_LOOP = re.compile(
    r"for\s+\w+\s+in\s+\{\d+\s*\.\.\s*\d+\}|"
    r"for\s*\(\(\s*\w+\s*=\s*\d+\s*;|"
    r"for\s+\w+\s+in\s+\$\(\s*seq\b|"
    r"while\s*\(\(\s*\w+\s*[<>]|"
    r"(?:while|until)\b.*(?:SECONDS|deadline|EPOCHSECONDS|date\s+\+%s)\b.*(?:-lt|-gt|-le|-ge|[<>])",
)
SHELL_LOOP_EXIT = re.compile(r"\bbreak\b|\breturn\b|\bexit\b")
# Loud on expiry: `fail`/`err` (the suites' own reporters), a non-zero
# exit/return, or a message on stderr.
SHELL_LOUD_FAILURE = re.compile(
    r"\bfail\b|\berr\b|\bdie\b|\bexit\s+[1-9]|\breturn\s+\"?\$?[1-9?]|>&\s*2|\bexit\s+\"?\$",
)
# Shell has no shared wait library; delegating to a named `wait_*` helper (or to
# `timeout`, which fails by itself) is the shape that replaces one.
SHELL_OWNED_WAIT = re.compile(r"\bwait_for_\w+|\b\w*_wait\b|\bwait_\w+\s*\(?|\btimeout\s+[\w\"$]")
SHELL_WAIT_CALL = re.compile(r"\bwait_for_\w+\s")

RULES = ("sleep-then-assert", "generic-settle", "private-wait-loop", "silent-timeout")


class Dialect:
    """One language's spelling of the four rules, plus its wait owner.

    A dialect is selected by file suffix, and the suffixes a dialect claims are
    the only ones `sources()` reads, so a corpus cannot be scanned without a
    rule that understands it and a rule cannot be written for a corpus nothing
    reads.
    """

    def __init__(
        self,
        *,
        name: str,
        suffixes: tuple[str, ...],
        comments: tuple[str, ...],
        closers: tuple[str, ...],
        sleep: re.Pattern[str],
        sleep_definition: re.Pattern[str],
        assertion: re.Pattern[str],
        loop_head: re.Pattern[str],
        bounded_loop: re.Pattern[str],
        loop_exit: re.Pattern[str],
        loud_failure: re.Pattern[str],
        owned_wait: re.Pattern[str],
        wait_call: re.Pattern[str],
        owner: str,
        definition: re.Pattern[str] | None = None,
        deadline_span: int = 6,
        rules: tuple[str, ...] = RULES,
    ) -> None:
        self.name = name
        self.suffixes = suffixes
        self.comments = comments
        self.closers = closers
        self.sleep = sleep
        self.sleep_definition = sleep_definition
        self.assertion = assertion
        self.loop_head = loop_head
        self.bounded_loop = bounded_loop
        self.loop_exit = loop_exit
        self.loud_failure = loud_failure
        self.owned_wait = owned_wait
        self.wait_call = wait_call
        self.owner = owner
        # Set only where the owner is a *shape* rather than a shared symbol: a
        # loop inside a function the dialect's `owned_wait` recognises is the
        # owner's own loop, not a private one. Rust and TypeScript have one
        # shared owner each, so a private `wait_for_x` there is the violation.
        self.definition = definition
        self.deadline_span = deadline_span
        # Which of `RULES` this dialect carries. `private-wait-loop` exists
        # because Rust and TypeScript each have one shared wait library; shell
        # has none to delegate to, so "private" would name every correct poll in
        # the suites and nothing else. Shell is protected instead by the two
        # rules that describe a *wrong* shell wait: a bare sleep standing in for
        # a fact, and a deadline nobody reports.
        self.rules = rules


RUST = Dialect(
    name="rust",
    suffixes=(".rs",),
    comments=("//", "///", "#", "*"),
    closers=("}",),
    sleep=SLEEP,
    sleep_definition=SLEEP_DEFINITION,
    assertion=ASSERTION,
    loop_head=LOOP_HEAD,
    bounded_loop=BOUNDED_LOOP,
    loop_exit=LOOP_EXIT,
    loud_failure=LOUD_FAILURE,
    owned_wait=OWNED_WAIT,
    wait_call=WAIT_CALL,
    owner=RUST_OWNER,
)

SCRIPT = Dialect(
    name="typescript",
    suffixes=(".ts", ".tsx", ".mjs", ".js"),
    comments=("//", "///", "#", "*"),
    closers=("}",),
    sleep=SLEEP,
    sleep_definition=SLEEP_DEFINITION,
    assertion=ASSERTION,
    loop_head=LOOP_HEAD,
    bounded_loop=BOUNDED_LOOP,
    loop_exit=LOOP_EXIT,
    loud_failure=LOUD_FAILURE,
    owned_wait=OWNED_WAIT,
    wait_call=WAIT_CALL,
    owner=TS_OWNER,
)

SHELL = Dialect(
    name="shell",
    suffixes=(".sh", ".bash"),
    comments=("#",),
    closers=("done", "fi", "}"),
    sleep=SHELL_SLEEP,
    sleep_definition=SHELL_SLEEP_DEFINITION,
    assertion=SHELL_ASSERTION,
    loop_head=SHELL_LOOP_HEAD,
    bounded_loop=SHELL_BOUNDED_LOOP,
    loop_exit=SHELL_LOOP_EXIT,
    loud_failure=SHELL_LOUD_FAILURE,
    owned_wait=SHELL_OWNED_WAIT,
    wait_call=SHELL_WAIT_CALL,
    owner=SHELL_OWNER,
    definition=SHELL_DEFINITION,
    # A shell deadline path is prose-shaped: the fall-through usually reads a
    # last value, prints it, and only then decides. Twelve lines reaches that
    # decision; six stops inside the read.
    deadline_span=12,
    rules=("sleep-then-assert", "generic-settle", "silent-timeout"),
)

# Suffix -> dialect. This mapping is also the corpus filter: `sources()` reads a
# declared input only when some dialect claims its suffix.
DIALECTS: dict[str, Dialect] = {
    suffix: dialect
    for dialect in (RUST, SCRIPT, SHELL)
    for suffix in dialect.suffixes
}

MESSAGES = {
    "sleep-then-assert": (
        "this wait names no durable fact -- it sleeps a fixed time and then "
        f"asserts. Settle on a committed value through {{owner}} and anchor "
        "on the command's own processed_at, an outbox that drained, a revision, "
        "or a lifecycle fact. A real timeout/expiry test keeps its sleep and "
        "says why in an `awaken-allow(wait): <reason>` comment above it"
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
    """Every test source this rule governs, keyed by repository-relative path.

    The corpus is the declared `INPUT_GLOBS`, walked. There is no second list:
    the trigger `generate_hooks.py` renders, the scope this scan reads and the
    scope the baseline is measured over are all the one declaration, so a corpus
    added to the hook cannot be missing from the scan and vice versa.
    """

    found: dict[str, str] = {}
    for pattern in INPUT_GLOBS:
        if Path(pattern).suffix not in DIALECTS:
            continue  # a declared data input (the baseline), not a scanned corpus
        for path in sorted(root.glob(pattern)):
            if "node_modules" in path.parts or not path.is_file():
                continue
            relative = path.relative_to(root).as_posix()
            text = path.read_text(encoding="utf-8", errors="ignore")
            # Production Rust holds real poll loops and real backoffs; only the
            # inline test module of a `src` file is a test.
            if path.suffix == ".rs" and "/src/" in f"/{relative}":
                text = test_module_of(text)
            found[relative] = text
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
    """The `awaken-allow(wait):` reason covering line `index`, if one is in force."""

    for offset in range(max(0, index - 3), index + 1):
        reason = formal_sources.allow_reason(lines[offset], "wait")
        if reason:
            return reason
    return None


def _in_poll_loop(lines: list[str], index: int, dialect: Dialect) -> bool:
    """Is line `index` inside a loop body (so the sleep is a poll interval)?"""

    for offset in range(max(0, index - 14), index):
        if dialect.loop_head.match(lines[offset]):
            return True
    return False


def _loop_end(lines: list[str], index: int, dialect: Dialect) -> int:
    """The index of the line closing the loop opened at `index`."""

    indent = len(lines[index]) - len(lines[index].lstrip())
    for offset in range(index + 1, min(len(lines), index + 60)):
        line = lines[offset]
        if not line.strip():
            continue
        if len(line) - len(line.lstrip()) <= indent and line.strip().startswith(dialect.closers):
            return offset
    return min(len(lines) - 1, index + 60)


def _fails_loudly_on_deadline(lines: list[str], index: int, dialect: Dialect) -> bool:
    """Does the loop opened at `index` report its own expiry?

    The deadline path is whatever runs once the loop falls through. It is loud
    when the loop itself throws/asserts, when the code just after it does, or
    when the loop delegates to an owner whose timeout already fails.
    """

    end = _loop_end(lines, index, dialect)
    region = (
        _code(lines, index, end - index + 1, dialect)
        + "\n"
        + _code(lines, end + 1, dialect.deadline_span, dialect)
    )
    return bool(dialect.loud_failure.search(region) or dialect.owned_wait.search(region))


def _inside_the_owner(lines: list[str], index: int, dialect: Dialect) -> bool:
    """Is line `index` inside a function that *is* one of this dialect's waits?

    Only dialects whose owner is a shape (shell) set `definition`; for those,
    the wait helper's own poll loop is the owner, not a private copy of it.
    """

    if dialect.definition is None:
        return False
    for offset in range(index, max(-1, index - 80), -1):
        match = dialect.definition.match(lines[offset])
        if match:
            return bool(dialect.owned_wait.search(match.group("name")))
    return False


def _window(lines: list[str], index: int, span: int = 40) -> str:
    return "\n".join(lines[index : index + span])


def _code(lines: list[str], index: int, span: int, dialect: Dialect) -> str:
    """`_window` with comment lines removed: prose about a wait is not a wait."""

    return "\n".join(
        line
        for line in lines[index : index + span]
        if not line.strip().startswith(dialect.comments)
    )


# Every rule needs either a sleep (both sleep rules) or a status word (the
# lifecycle rule), so a file carrying neither is skipped whole rather than
# windowed line by line.
CANDIDATE = re.compile(r"sleep|setTimeout|status")


def _claimed_trees() -> set[str]:
    """Directories a declared pattern claims whole: `<literal dir>/**/*.<ext>`.

    Derived from `INPUT_GLOBS`, so adding a corpus adds its completeness check.
    A pattern that constrains directories below the literal head
    (`crates/**/src/**/*.rs`) claims a shape inside a tree rather than the
    tree, and is excluded.
    """

    trees: set[str] = set()
    for pattern in INPUT_GLOBS:
        head, separator, tail = pattern.partition("/**/")
        if not separator or "*" in head or Path(pattern).suffix not in DIALECTS:
            continue
        if "/" in tail:
            continue
        trees.add(head)
    return trees


def dialect_for(relative: str) -> Dialect:
    """The dialect claiming `relative`'s suffix; Rust is the fallback."""

    return DIALECTS.get(Path(relative).suffix, RUST)


def file_violations(relative: str, text: str) -> list[tuple[str, int, str]]:
    """Every (rule, line number, source line) this file breaks."""

    if not CANDIDATE.search(text):
        return []
    dialect = dialect_for(relative)
    lines = text.splitlines()
    found: list[tuple[str, int, str]] = []
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith(dialect.comments):
            continue

        if dialect.loop_head.match(line):
            body = _window(lines, index, 20)
            if (
                dialect.sleep.search(body)
                and dialect.loop_exit.search(body)
                and not dialect.owned_wait.search(body)
                and not _inside_the_owner(lines, index, dialect)
            ):
                found.append(("private-wait-loop", index + 1, stripped))
            if (
                dialect.sleep.search(body)
                and dialect.bounded_loop.search(line)
                and not _fails_loudly_on_deadline(lines, index, dialect)
            ):
                found.append(("silent-timeout", index + 1, stripped))

        if (
            dialect.sleep.search(line)
            and not dialect.sleep_definition.match(line)
            and not _in_poll_loop(lines, index, dialect)
        ):
            ahead = _code(lines, index + 1, 6, dialect)
            if dialect.assertion.search(ahead):
                found.append(("sleep-then-assert", index + 1, stripped))

        if dialect.wait_call.search(line) or dialect.loop_head.match(line):
            body = _window(lines, index, 24)
            if LIFECYCLE.search(body) and not DURABLE.search(body):
                found.append(("generic-settle", index + 1, stripped))

    return [
        (rule, number, source)
        for rule, number, source in found
        if rule in dialect.rules and _exempt(lines, number - 1) is None
    ]


def short_reasons(text: str) -> list[tuple[int, str]]:
    """Line/reason pairs for matched markers that do not actually say why."""

    return [
        (number, reason)
        for number, line in enumerate(text.splitlines(), 1)
        if (reason := formal_sources.allow_reason(line, "wait")) is not None and len(reason) < MIN_REASON
    ]


def owner_for(relative: str) -> str:
    return dialect_for(relative).owner


# The one measurement shared by every mode. Counts are derived from the exact
# occurrence lists, so diagnostics and candidates cannot measure different debt.
WaitPlaces = dict[str, dict[str, list[tuple[int, str]]]]
Measurement = tuple[_architecture_ratchet.Counts, WaitPlaces]


def measurement(found: dict[str, list[tuple[str, int, str]]]) -> Measurement:
    places: WaitPlaces = {rule: {} for rule in RULES}
    for relative, violations in found.items():
        for rule, number, source in violations:
            places[rule].setdefault(relative, []).append((number, source))
    counts = {rule: {path: len(occurrences) for path, occurrences in paths.items()}
              for rule, paths in places.items()}
    return counts, places


def measure() -> tuple[list[str], Measurement]:
    # Unlike structural ratchets, test-waits has no first-write authorization:
    # its original writer required this tracked debt record to exist as well.
    if not BASELINE.exists():
        return [f"baseline-present: {BASELINE.name} missing -> required; "
                "fix: git restore -- scripts/ci/test_waits_baseline.json"], measurement({})
    found = {}
    errors = []
    for relative, text in sources(ROOT).items():
        found[relative] = file_violations(relative, text)
        for number, reason in short_reasons(text):
            errors.append(f"wait-reason: {relative}:{number} {len(reason)} -> >= {MIN_REASON}; "
                          f"`awaken-allow(wait): {reason}` must explain the wait; "
                          "fix: python3 scripts/ci/guards.py explain check_test_waits:wait-reason")
    return errors, measurement(found)


def report(
    measured: Measurement,
    baseline: dict[str, dict[str, int]],
) -> tuple[list[str], list[str]]:
    """The sole comparison: new waits are errors; decreases must be recorded."""

    counts, places = measured
    errors = []
    gained = []
    for rule in RULES:
        for relative in sorted(set(counts[rule]) | set(baseline.get(rule, {}))):
            now, was = counts[rule].get(relative, 0), baseline.get(rule, {}).get(relative, 0)
            if now > was:
                message = MESSAGES[rule].format(owner=owner_for(relative))
                for number, source in places[rule][relative][was:]:
                    errors.append(f"{rule}: {relative} {was} -> {now} ({relative}:{number}); {message}; "
                                  f"fix: python3 scripts/ci/guards.py explain check_test_waits:{rule}\n    {source}")
            elif now < was:
                gained.append(f"{rule}: {relative} {was} -> {now}; lower the baseline to hold the ground gained; "
                              "fix: python3 scripts/ci/check_test_waits.py --tighten")
    return errors, gained


def selftest() -> None:
    """Decision table for the four rules, once per dialect, plus the corpus.

    Rust and TypeScript (`crates/**`, `e2e/**`):

    | Rule | Cause                                              | Effect |
    |------|----------------------------------------------------|--------|
    | S1   | fixed sleep, then an assertion                      | reject |
    | S2   | same, carrying an `awaken-allow(wait):` reason             | accept |
    | S3   | sleep as the poll interval of a loop                | not S1 |
    | S4   | wait settles on a lifecycle status, no durable value | reject |
    | S5   | same wait also naming a committed value             | accept |
    | S6   | private poll loop with a sleep and an exit          | reject |
    | S7   | loop delegating to an owned wait                    | accept |
    | S8   | an `awaken-allow(wait):` marker that says nothing          | reject |
    | S9   | a `src` file's inline `#[cfg(test)] mod … {`        | in scope |
    | S10  | a `#[cfg(test)]` on one item, or `mod tests;`       | out of scope |
    | S11  | bounded poll loop that just falls through           | reject |
    | S12  | same loop throwing, asserting, or panicking at exit | accept |
    | S13  | unbounded loop (a fake server, a producer)          | not S11 |

    Shell (`e2e/**/*.sh`), same four rules in the other grammar:

    | Rule | Cause                                                 | Effect |
    |------|-------------------------------------------------------|--------|
    | H1   | bare `sleep 2`, then `[ … ] || fail`                   | reject |
    | H2   | same, carrying an `awaken-allow(wait):` reason                | accept |
    | H3   | `sleep 0.2` as a bounded loop's poll interval          | not H1 |
    | H4   | `for i in {1..30}` poll that just falls through        | reject |
    | H5   | same loop ending in `echo … >&2` / `return 2`          | accept |
    | H6   | `while true` producer with a sleep                     | not H4 |
    | H7   | private `while` poll with a sleep and a `break`        | accept |
    | H8   | loop delegating to a named `wait_for_…` helper         | accept |
    | H9   | a function *named* `sleep_until()`                     | accept |
    | H10  | a `# sleep 1` comment describing a wait                | accept |
    | H11  | wait settling on `status_idle` with no committed value | reject |
    | H12  | `for d in ingress egress both` (a traversal)           | not H7 |
    | H13  | `while read -r a b` over a command's lines             | not H7 |
    | H14  | the poll loop *inside* a `wait_for_…` helper           | accept |
    | H15  | that same helper falling through without reporting     | reject |
    | H16  | shell carries no `private-wait-loop` rule at all        | by design |

    Console Playwright specs (`web/e2e/**`) use the script dialect:

    | Rule | Cause                                                  | Effect |
    |------|--------------------------------------------------------|--------|
    | W1   | `while (Date.now() < d)` + `setTimeout` + `break`       | reject |
    | W2   | a `.spec.ts` under `web/e2e` is in the scanned corpus   | in scope |

    And over the corpus itself, so a tree added tomorrow inherits the rule:

    | Rule | Cause                                                  | Effect |
    |------|--------------------------------------------------------|--------|
    | C1   | every scanned suffix is claimed by some dialect         | holds  |
    | C2   | every declared code pattern matches at least one file    | holds  |
    | C3   | a file under a claimed tree whose suffix a dialect reads | scanned |
    """

    def rules(text: str) -> set[str]:
        return {rule for rule, _, _ in file_violations("a.rs", text)}

    def shell_rules(text: str) -> set[str]:
        return {rule for rule, _, _ in file_violations("e2e/probe.sh", text)}

    def spec_rules(text: str) -> set[str]:
        return {rule for rule, _, _ in file_violations("web/e2e/probe.spec.ts", text)}

    s1 = "    tokio::time::sleep(d).await;\n    assert_eq!(calls, 1);\n"
    assert "sleep-then-assert" in rules(s1), "S1"

    s2 = (
        "    // awaken-allow(wait): the lease TTL itself is the subject of this test\n"
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

    assert short_reasons("// awaken-allow(wait): flaky\n"), "S8"
    assert not short_reasons(
        "// awaken-allow(wait): the lease TTL itself is the subject of this test\n"
    ), "S8"

    baseline = {"sleep-then-assert": {"a.rs": 1}}
    one = {"a.rs": [("sleep-then-assert", 3, "sleep(d)")]}
    two = {"a.rs": [("sleep-then-assert", 3, "sleep(d)"), ("sleep-then-assert", 9, "sleep(d)")]}
    assert report(measurement(one), baseline) == ([], []), "ratchet holds at the baseline"
    assert report(measurement(two), baseline)[0], "a new wait above the baseline is refused"
    assert report(measurement({}), baseline)[1], "ground gained must lower the baseline"

    h1 = (
        "submit_background inflight 'x'\n"
        "sleep 2\n"
        'n="$(assistant_count inflight)"\n'
        '[ "$n" -eq 1 ] || fail "inflight has $n replies"\n'
    )
    assert "sleep-then-assert" in shell_rules(h1), "H1"

    h2 = (
        "# awaken-allow(wait): the drain grace period itself is the subject here\n"
        "sleep 2\n"
        'n="$(assistant_count inflight)"\n'
        '[ "$n" -eq 1 ] || fail "inflight has $n replies"\n'
    )
    assert "sleep-then-assert" not in shell_rules(h2), "H2"

    h3 = (
        "for attempt in {1..50}; do\n"
        '  observed=$(read_revision) || return "$?"\n'
        '  [[ "$observed" != "2" ]] || return 0\n'
        "  sleep 0.2\n"
        "done\n"
        'echo "the revision never advanced" >&2\n'
        "return 2\n"
    )
    assert "sleep-then-assert" not in shell_rules(h3), "H3"
    assert "silent-timeout" not in shell_rules(h3), "H5"

    h4 = (
        "for attempt in {1..30}; do\n"
        "  ready=$(curl -sf \"$base/readyz\") && break\n"
        "  sleep 1\n"
        "done\n"
        'echo "continuing"\n'
    )
    assert "silent-timeout" in shell_rules(h4), "H4"

    h6 = (
        "while true; do\n"
        "  emit_heartbeat\n"
        "  sleep 1\n"
        "done\n"
    )
    assert "silent-timeout" not in shell_rules(h6), "H6"

    h7 = (
        "while read_pending; do\n"
        "  [ -f \"$marker\" ] && break\n"
        "  sleep 1\n"
        "done\n"
    )
    assert "private-wait-loop" not in shell_rules(h7), "H7"
    assert "private-wait-loop" not in SHELL.rules, "H16"
    assert "private-wait-loop" in RUST.rules and "private-wait-loop" in SCRIPT.rules, "H16"

    h8 = (
        "for attempt in {1..3}; do\n"
        "  wait_for_assistants inflight 1 20 && break\n"
        "  sleep 1\n"
        "done\n"
    )
    assert "private-wait-loop" not in shell_rules(h8), "H8"

    h9 = (
        "sleep_until() {\n"
        '  local deadline="$1"\n'
        "}\n"
        '[ "$n" -eq 1 ] || fail "x"\n'
    )
    assert "sleep-then-assert" not in shell_rules(h9), "H9"

    h10 = (
        "# sleep 1 here would race the commit\n"
        '[ "$n" -eq 1 ] || fail "x"\n'
    )
    assert not shell_rules(h10), "H10"

    h11 = (
        "for attempt in {1..30}; do\n"
        '  last=$(curl -sf "$base/events" | jq -r ".data[-1].type")\n'
        '  [ "$last" = "session.status_idle" ] && break\n'
        "  sleep 1\n"
        "done\n"
        'echo "never idled" >&2\n'
        "exit 1\n"
    )
    assert "generic-settle" in shell_rules(h11), "H11"

    h12 = (
        "for direction in ingress egress both; do\n"
        "  apply_partition \"$direction\"\n"
        "  sleep 1\n"
        "  break\n"
        "done\n"
    )
    assert not shell_rules(h12), "H12"

    h13 = (
        "while read -r sandbox node; do\n"
        "  record \"$sandbox\" \"$node\"\n"
        "  sleep 1\n"
        "  break\n"
        "done < <(list_failures)\n"
    )
    assert not shell_rules(h13), "H13"

    h14 = (
        "wait_for_assistants() {\n"
        "  local deadline=$(( $(date +%s) + $3 ))\n"
        "  while :; do\n"
        '    n="$(assistant_count "$1")"\n'
        '    [ "$n" -ge "$2" ] && return 0\n'
        '    [ "$(date +%s)" -ge "$deadline" ] && fail "timed out; saw $n"\n'
        "    sleep 0.2\n"
        "  done\n"
        "}\n"
    )
    assert "private-wait-loop" not in shell_rules(h14), "H14"

    h15 = (
        "wait_for_ready() {\n"
        "  for _ in $(seq 1 100); do\n"
        '    curl -sf "$BASE/readyz" >/dev/null && return 0\n'
        "    sleep 0.1\n"
        "  done\n"
        "}\n"
    )
    assert "silent-timeout" in shell_rules(h15), "H15"

    w1 = (
        "  const deadline = Date.now() + 60_000;\n"
        "  while (Date.now() < deadline) {\n"
        "    await new Promise((r) => setTimeout(r, 2000));\n"
        "    const seen = await read();\n"
        "    if (seen) break;\n"
        "  }\n"
        "  expect(seen).toBe(true);\n"
    )
    assert "private-wait-loop" in spec_rules(w1), "W1"

    scanned = sources(ROOT)
    assert any(name.startswith("web/e2e/") and name.endswith(".spec.ts") for name in scanned), "W2"
    assert any(name.startswith("e2e/") and name.endswith(".sh") for name in scanned), "H0"
    # Corpus assertion: the declaration is the scope. Every suffix `sources()`
    # returns is one a dialect claims, and every code pattern declared in
    # INPUT_GLOBS contributed to the scan, so a corpus cannot be read without a
    # rule that understands it, nor declared without being read.
    assert {Path(name).suffix for name in scanned} <= set(DIALECTS), "every scanned suffix has a rule"
    for pattern in INPUT_GLOBS:
        if Path(pattern).suffix not in DIALECTS:
            continue
        assert any(ROOT.glob(pattern)), f"declared corpus {pattern} matched nothing"
    # A pattern rooted at a literal directory claims that whole tree, so every
    # file under it that some dialect can read must be read. This is the
    # assertion that would have failed the day `e2e/**/*.mjs` was declared while
    # `e2e/**/*.sh` was not, instead of the rule quietly skipping 14 suites.
    for root_name in _claimed_trees():
        for path in sorted((ROOT / root_name).rglob("*")):
            if not path.is_file() or "node_modules" in path.parts:
                continue
            if path.suffix not in DIALECTS:
                continue
            relative = path.relative_to(ROOT).as_posix()
            assert relative in scanned, (
                f"{relative} is inside the claimed tree {root_name}/ and a dialect "
                "reads its suffix, but no INPUT_GLOBS pattern covers it"
            )
    _selftest_cli()
    _selftest_source_exemptions()
    print("check_test_waits selftest: the decision table holds")


def main(argv: list[str] | None = None) -> int:
    def check(current, previous):
        growth, gained = report(current, previous.get("baseline", {}))
        return growth + gained

    def lower(current, previous):
        growth, _ = report(current, previous.get("baseline", {}))
        if growth:
            # Keep the old authority so the shared candidate check refuses a
            # new bad wait, even if an explicit write carries a CLI reason.
            return previous
        counts, _ = current
        return {**previous, "baseline": {
            **previous.get("baseline", {}),
            **{rule: dict(sorted(counts[rule].items())) for rule in RULES},
        }}

    return _architecture_ratchet.baseline_cli(
        argv, prog="check_test_waits.py", description=__doc__, baseline_path=BASELINE,
        self_test=selftest, measure=measure, check=check, tighten=lower,
        write=lambda current, previous, _reason: (lower(current, previous), []),
    )


def _selftest_cli() -> None:
    """CLI cause/effect graph and decision table, attached to the tested cases.

    Valid argv AND existing baseline AND valid source reasons -> one
    scan -> per-key growth? refuse : decrease? read-only refusal / write a
    checked lower candidate : equal/no write. Any hard failure blocks writes.
    Constraints: modes exclusive; a nonempty reason belongs only to write;
    growth and decrease may coexist and never cancel; CLI reason cannot admit
    bad waits or replace a source decision. Effects: status, exact bytes,
    one source/scanner invocation, diagnostics and retained foreign metadata.

    TCLI1 equal/fall x default/check -> read-only success/refusal.
    TCLI2 equal/fall x tighten/write -> no write/shrink, preserve metadata.
    TCLI3 growth/new path/new rule/mixed fall -> refuse, even reasoned write.
    TCLI4 short source reason -> hard refusal; length 20 is admitted.
    TCLI5 a genuine-timing source marker is the only exemption; a baseline
          `allow` entry (retired) exempts nothing.
    TCLI6 missing/empty baseline cannot initialize unconverted waits.
    TCLI7 invalid/abbreviated/conflicting argv -> usage before measurement.
    TCLI8 self-test -> existing tests, no normal scan or baseline write.
    TCLI9 every measured mode -> one real scan, once per actual source file.
    TCLI10 original corpus/argv and helper closure -> generated hooks/navigation.
    """
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch

    path = "e2e/probe.mjs"
    sleep = "await sleep(10);\nassert.equal(calls, 1);\n"
    held = {"baseline": {rule: {} for rule in RULES},
            "owner": "waits", "expiry": "after-conversion", "foreign": {"exact": [1, 2]}}
    held["baseline"]["sleep-then-assert"][path] = 1
    held["baseline"]["future-rule"] = {"foreign-path": 7, "owner": "other", "expiry": "later"}
    held["growth_reasons"] = {"future-rule": {"foreign-path": "  preserve exact reason  "}}
    original_sources, original_scanner = sources, file_violations

    def invoke(argv, texts=None, baseline=held):
        # TCLI9 uses the actual sources(), file reads and scanner, rather than
        # a stub measurement that would miss a writer's second corpus scan.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "baseline.json"
            if baseline is not None:
                target.write_text(json.dumps(baseline) + "\n", encoding="utf-8")
            for relative, text in (texts if texts is not None else {path: sleep}).items():
                source = root / relative
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text(text, encoding="utf-8")
            before = target.read_bytes() if target.exists() else None
            observed, tests = [], []

            def scan(actual_root):
                observed.append("sources")
                return original_sources(actual_root)

            def classify(relative, text):
                observed.append(relative)
                return original_scanner(relative, text)

            output, errors = io.StringIO(), io.StringIO()
            with patch.dict(main.__globals__, {"ROOT": root, "BASELINE": target, "sources": scan,
                    "file_violations": classify, "selftest": lambda: tests.append("selftest")}), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                try:
                    status = main(argv)
                except SystemExit as error:
                    status = error.code
            after = target.read_bytes() if target.exists() else None
            if observed:
                assert observed.count("sources") == 1, "TCLI9 exactly one corpus read"
                scanned = [item for item in observed if item != "sources"]
                assert set(scanned) == set(texts if texts is not None else {path: sleep}), "TCLI9 complete fixture"
                assert len(scanned) == len(set(scanned)), "TCLI9 each actual file scanned once"
            return status, before, after, observed, tests, output.getvalue(), errors.getvalue()

    for argv in ([], ["--check"]):
        status, before, after, observed, _, _, _ = invoke(argv)
        assert status == 0 and before == after and observed, f"TCLI1 equal {argv}"
        status, before, after, observed, _, _, errors = invoke(argv, {})
        assert status == 1 and before == after and observed and "--tighten" in errors, "TCLI1 strict fall"
    writes = (["--tighten"], ["--write-baseline"], ["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")])
    for argv in writes:
        status, before, after, _, _, _, _ = invoke(argv)
        assert status == 0 and before == after, "TCLI2 equal never rewrites bytes"
        status, _, after, _, _, _, _ = invoke(argv, {})
        document = json.loads(after)
        assert status == 0 and document["baseline"]["sleep-then-assert"] == {}, "TCLI2 shrink"
        assert {key: value for key, value in document.items() if key != "baseline"} == \
               {key: value for key, value in held.items() if key != "baseline"}, "TCLI2 root metadata exact"
        assert document["baseline"]["future-rule"] == held["baseline"]["future-rule"], "TCLI2 foreign rule exact"

    modes = ([], ["--check"], *writes)
    empty = {**held, "baseline": {"future-rule": held["baseline"]["future-rule"]}}
    cases = (({path: sleep + sleep}, held), ({"e2e/new.mjs": sleep}, held),
             ({path: sleep}, empty))
    for texts, baseline in cases:
        for argv in modes:
            status, before, after, _, _, _, errors = invoke(argv, texts, baseline)
            assert status == 1 and before == after, f"TCLI3/TCLI6 refuse growth {argv}"
            assert "sleep-then-assert:" in errors and " -> " in errors and "fix:" in errors, "TCLI3 diagnostic"
    # TCLI3 mixed decrease of old path and increase of new path is not a net
    # zero allowance transfer: the same report must still name the new wait.
    status, before, after, _, _, _, errors = invoke(["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("move")],
                                                  {"e2e/new.mjs": sleep})
    assert status == 1 and before == after and "e2e/new.mjs:1" in errors, "TCLI3 mixed growth/fall"

    for argv in modes:
        status, before, after, _, _, _, errors = invoke(argv, {path: "// awaken-allow(wait): flaky\n" + sleep})
        assert status == 1 and before == after and "wait-reason:" in errors, "TCLI4 short exempt marker"
    for reason in ("x" * MIN_REASON, "the lease TTL itself is the subject of this test"):
        for argv in writes:
            status, _, after, _, _, _, _ = invoke(argv, {path: "// awaken-allow(wait): " + reason + "\n" + sleep})
            assert status == 0 and json.loads(after)["baseline"]["sleep-then-assert"] == {}, "TCLI4/TCLI5 valid reason"
    decision = {"path": "e2e/new.mjs", "rule": "sleep-then-assert", "reason": "x" * MIN_REASON}
    retired = {**held, "allow": [decision]}
    for argv in modes:
        status, before, after, _, _, _, errors = invoke(argv, {path: sleep, "e2e/new.mjs": sleep}, retired)
        assert status == 1 and before == after and "e2e/new.mjs:1" in errors, "TCLI5 baseline allow exempts nothing"

    for argv in modes:
        for texts in ({}, {path: sleep}):
            status, before, after, observed, _, _, _ = invoke(argv, texts, None)
            assert status == 1 and before is None and after is None and not observed, "TCLI6 missing refuses before scan"
    for argv in (["--awaken-gate-argument-probe"], ["--self-tes"], ["--selftest"], ["--write-baselin"],
                 ["--check", "--tighten"], ["--self-test", "--write-baseline"],
                 ["--check", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], ["--tighten", "--reason", _architecture_ratchet._test_growth_reason("reviewed")],
                 ["--write-baseline", "--reason", ""], ["--write-baseline", "--reason", "   "]):
        status, before, after, observed, _, _, errors = invoke(argv)
        assert status == 2 and before == after and not observed and "usage:" in errors, f"TCLI7 {argv}"
    status, before, after, observed, tests, _, _ = invoke(["--self-test"])
    assert status == 0 and before == after and not observed and tests == ["selftest"], "TCLI8 self-test only"
    # TCLI9 language owners share the same measurement and candidate: shell's
    # sleep/assert and Rust's inline test projection are still actually scanned.
    texts = {"e2e/probe.sh": "sleep 2\n[ \"$n\" -eq 1 ] || fail bad\n",
             "crates/fixture/src/lib.rs": "pub fn product() {}\n#[cfg(test)]\nmod tests {\n"
                 "    // awaken-allow(wait): the lease TTL itself is the subject of this test\n"
                 "    tokio::time::sleep(d).await;\n    assert_eq!(calls, 1);\n}\n"}
    shell_held = {**held, "baseline": {"sleep-then-assert": {"e2e/probe.sh": 1}}}
    for argv in modes:
        status, _, _, observed, _, _, _ = invoke(argv, texts, shell_held)
        assert status == 0 and observed.count("sources") == 1, "TCLI9 real multi-dialect scan"
    # TCLI10 source declaration remains the corpus/filter and sole hook input
    # closure. Navigation reads RULES/docstring as AST, not another rule registry.
    import generate_hooks
    import guards
    import ast
    helpers = ("_architecture_ratchet.py", "_crate_boundary_workspace.py", "_arch_fitness.py", "_crate_dependency_fitness.py")
    for name in helpers:
        assert any(generate_hooks.glob_matches(pattern, "scripts/ci/" + name)
                   for pattern in INPUT_GLOBS), "TCLI10 helper input closure"
    assert ".py" not in DIALECTS, "TCLI10 helpers do not widen test corpus"
    own = Path(__file__).read_text(encoding="utf-8")
    tree = ast.parse(own)
    assert set(RULES) <= set(guards.rule_names(tree, ast.get_docstring(tree))), "TCLI10 source rule navigation"
    declarations = generate_hooks.declared_input_globs(ROOT / "scripts/ci")
    hooks = (ROOT / "lefthook.yml").read_text(encoding="utf-8")
    assert generate_hooks.render(hooks, declarations) == hooks, "TCLI10 generated hook fixpoint"
    region = "".join(generate_hooks._region([Path(__file__).name], declarations))
    declared = declarations[Path(__file__).name]
    assert all(pattern in region for pattern in declared), "TCLI10 hook uses the complete owned declaration"
    for name in helpers:
        assert any(generate_hooks.glob_matches(pattern, "scripts/ci/" + name)
                   for pattern in declared), "TCLI10 helper edits trigger owning hook"


def _selftest_source_exemptions() -> None:
    """SX2 causes/effects of the shared spelling under the original wait policy.

    Rule identity + same/-1/-2/-3 line -> scanner exemption; -4/other rule/old
    current spelling -> original debt. Independently payload length<20 -> hard
    source error even when already exempted; length20 is valid. A blank payload
    cannot waive debt. Cross every boundary with the three existing dialects;
    TCLI4 above separately proves every real CLI mode refuses short reasons.
    """
    for path, sleep, assertion, comment in (
        ("tests/probe.rs", "tokio::time::sleep(d).await;", "assert_eq!(calls, 1);", "//"),
        ("e2e/probe.mjs", "await sleep(10);", "assert.equal(calls, 1);", "//"),
        ("e2e/probe.sh", "sleep 2", '[ "$n" -eq 1 ] || fail bad', "#"),
    ):
        for distance in (0, 1, 2, 3, 4):
            for reason in ("x" * 19, "x" * 20):
                marker = f"{comment} awaken-allow(wait): {reason}"
                text = (f"{sleep} {marker}\n{assertion}\n" if distance == 0 else
                        marker + "\n" * distance + sleep + "\n" + assertion + "\n")
                found = file_violations(path, text)
                assert bool(found) == (distance == 4), "SX2 exact original covering window"
                assert bool(short_reasons(text)) == (len(reason) < MIN_REASON), "SX2 separate hard reason rule"
        for marker in ("awaken-" + "wait: " + "x" * 20,
                       "awaken-allow(secret): " + "x" * 20):
            assert file_violations(path, f"{comment} {marker}\n{sleep}\n{assertion}\n"), "SX2 no current legacy/foreign-rule exemption"
        text = f"{comment} awaken-allow(wait):\n{sleep}\n{assertion}\n"
        assert short_reasons(text) and file_violations(path, text), "SX2 blank source reason is hard and cannot waive debt"



if __name__ == "__main__":
    raise SystemExit(main())
