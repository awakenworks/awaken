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
incentive: a store takes a ``WallClock`` at construction, a test hands it a
``ManualClock``, and the durable value is a fact the test names.  So the clock
may be read only inside the one crate that owns the wall-clock port
(``CLOCK_PORT_CRATE``) or where the program is composed; everywhere else it
arrives as an argument.  The library reads (``SystemTime::now()``,
``Instant::now()`` from std and tokio, ``Utc::now()``, ``Local::now()``,
``OffsetDateTime::now_utc()``, under any alias) are denied by the workspace
``clippy.toml`` in production code, where a reviewed composition-root site
carries ``#[expect(clippy::disallowed_methods, reason = ..)]``
(docs/design/guard-consolidation.md R13).  This rule keeps what clippy cannot
name: a concrete clock *value* asked for its time (``SystemClock.now_ms()``,
``Clock::now_ms(&SystemClock)``) and ``system_clock().now()``.  It is at zero
and a hard rule; it retires when no concrete clock type outside the
clock-port crate exposes a ``now*`` method.

Three more shapes are how the clock *authority* splits even when every read goes
through a port, and each one produced a live defect (``53c4fa0d36``,
``e943ca55c6``, ``3328fd529f``): two owners stamping the same aggregate from two
different clocks, so an ``erased_at`` and the ``updated_at`` beside it disagree
under test and a lease judged by one clock was minted by another.

**R3 — a second clock trait.**  ``trait WorkerClock``, ``trait RegistryClock``,
``trait DataSubjectClock``: each context declared its own wall-clock port with
its own spelling of "now", its own system implementation and its own test
double, and nothing stopped a component from holding two of them.  The one
wall-clock trait lives in ``CLOCK_PORT_CRATE``; a trait named ``*Clock``
anywhere else is a second authority.

**R4 — a constructor that defaults the system clock.**  ``Arc::new(SystemClock)``
inside ``fn new()`` makes the injected clock optional: the production path takes
the default, the test path injects, and the two are different programs.  The
real clock is chosen once, where the program is composed, and handed down.

**R5 — a shadow ``fn now*()`` helper.**  A private ``fn now_ms() -> u64`` is a
clock with no port: it is how an ambient read hides behind a name, and how one
file came to stamp half an aggregate from the injected clock and half from this
helper.

Every rule is ratcheted **per file** against a baseline, because converting
every instance at once would be a migration rather than a fix, and a whole-file
allow-list would freeze today's debt while guarding nothing.  A file may lose
occurrences and may not gain them, and a file absent from the baseline may have
none at all.  The scan reads the production projection of each file
(``_crate_boundary_workspace.production_rust``): only items provably disabled
without ``cfg(test)``/``cfg(kani)`` are removed, so ``cfg(any(test, feature =
"x"))`` code — which ships when the feature is on — is scanned, and a braceless
``#[cfg(test)] use ...;`` removes that one item and nothing after it.

R2, R3, R4 and R5 reached zero and are hard rules (``HARD_RULES``): every
occurrence fails and the baseline holds no record for them (plan v5 S2).

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

import _crate_boundary_workspace as workspace

INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    # The clock-port crate is located by its package name, so a manifest edit
    # can move the one place the real clock may be read.
    "crates/**/Cargo.toml",
    "scripts/ci/fabricated_facts_baseline.json",
    # The production projection every rule reads is the shared lexer's.
    "scripts/ci/_crate_boundary_workspace.py",
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

# ── R2..R5: the clock authority ──────────────────────────────────────────────

# The one crate that owns the wall-clock port. Its production files are where
# the real clock is read and where the one clock trait, its system factory and
# its time helpers are declared, so R2..R5 do not apply inside it. Located by
# package name through the workspace manifests, never by a hand-written path.
CLOCK_PORT_CRATE = "awaken-time"

AMBIENT_CLOCK = re.compile(
    # A concrete clock *value* asked for its time: `SystemClock.now_ms()`,
    # `awaken_dispatch::SystemClock.now_ms()`, `TokioModelInferenceClock.now()`.
    # A handle (`self.clock.now()`) is lower-case and is not matched. The std,
    # tokio, chrono and time reads are clippy's (clippy.toml, R13).
    r"\b[A-Z]\w*Clock\s*(?:::\s*default\s*\(\s*\)\s*)?\.\s*now\w*\s*\("
    # The same read spelled as a qualified call: `Clock::now_ms(&SystemClock)`.
    r"|\b\w*Clock\s*::\s*now\w*\s*\(\s*&\s*(?:[A-Za-z_]\w*\s*::\s*)*[A-Z]\w*Clock\b"
    # The composition-root factory read in place instead of handed down.
    r"|\bsystem_clock\s*\(\s*\)\s*\.\s*now\w*\s*\("
)

CLOCK_TRAIT = re.compile(r"\btrait\s+\w*Clock\b")

DEFAULTED_SYSTEM_CLOCK = re.compile(
    # `Arc::new(SystemClock)`, possibly path-qualified and wrapped across lines.
    r"\b(?:Arc|Box|Rc)\s*::\s*new\s*\(\s*(?:[A-Za-z_]\w*\s*::\s*)*System\w*Clock\b"
    r"|\bSystem\w*Clock\s*::\s*(?:default|new)\s*\("
    # The factory called outside a composition root (its declaration is not a call).
    r"|(?<!fn )\bsystem_clock\s*\(\s*\)(?!\s*\.\s*now)"
)

SHADOW_CLOCK_HELPER = re.compile(r"\bfn\s+now\w*\s*[<(]")

# Composition roots: the binaries and the modules whose whole job is to build
# the object graph. Named, not pattern-matched, so adding one is a decision.
COMPOSITION_ROOTS: frozenset[str] = frozenset(
    {
        "crates/apps/awaken-process-startup/src/process_stores.rs",
        "crates/apps/awaken-process-startup/src/local_process_stores.rs",
        # The Coordinator/AllInOne and Control process compositions: each
        # chooses the process's one wall clock and hands it to every store and
        # component it builds.
        "crates/apps/awaken-process-startup/src/deployment_process.rs",
        "crates/apps/awaken-process-startup/src/control.rs",
        # The CLI binary's Config and Doctor commands are their own process
        # entry points and select the clock before lease admission.
        "crates/apps/awaken-cli/src/main.rs",
        # Service startup composes its local ACP Worker before the later
        # Control/Coordinator graph; it selects their shared clock once.
        "crates/apps/awaken-process-startup/src/service.rs",
        # The Worker daemon's bootstrap: it composes the Worker transport and
        # chooses the clock its signed request assertions are stamped with.
        "crates/apps/awaken-worker/src/bootstrap.rs",
        # The Sandbox process's `main`: each role it dispatches to (the Hand,
        # the Git credential helper) is composed here and handed its clock.
        "crates/apps/awaken-sandbox/src/main.rs",
        # Standalone example programs: each `main` composes one embedded
        # runtime and chooses the clock its commit store stamps with.
        "crates/sdk/devtools/awaken-runtime-examples/examples/direct_runtime.rs",
        "crates/sdk/devtools/awaken-runtime-examples/examples/hello_agent.rs",
        "crates/devtools/awaken-sdk-server/src/main.rs",
        # Standalone Worker example programs: each `main` composes one Worker
        # process and chooses the clock its Host and request assertions share.
        "crates/apps/awaken-cli/examples/credential_reference_worker.rs",
        "crates/apps/awaken-cli/examples/hand_brain_lazy_worker.rs",
        # Harness processes. Each is the root of its own process graph: the
        # Scenario host's main and the modules that compose its platform, the
        # evaluation runners that compose one runtime per case, and the coding
        # example that composes one embedded agent.
        "crates/devtools/awaken-scenario-host/src/main.rs",
        "crates/devtools/awaken-scenario-host/src/deployment.rs",
        "crates/devtools/awaken-scenario-host/src/lib.rs",
        # Standalone qualification processes: each main chooses one clock for
        # its Session/Dispatch/Vault or ACP graph; helpers receive that clock.
        "crates/devtools/awaken-scenario-host/examples/application_credential_fixture.rs",
        "crates/devtools/awaken-scenario-host/examples/application_credential_acp_fixture.rs",
        "crates/devtools/awaken-eval/src/acp_runner.rs",
        "crates/devtools/awaken-eval/src/replay.rs",
        # Clock selection moved from CodingSession's library constructor to
        # this declared coding_agent example's actual process entry point.
        "crates/sdk/devtools/awaken-runtime-examples/examples/coding_agent/main.rs",
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
    ("clock_trait_declaration", "clock trait outside the clock-port crate"),
    ("defaulted_system_clock", "system clock chosen below the composition root"),
    ("shadow_clock_helper", "shadow `fn now*()` clock helper"),
)
# Rules at zero: plain denies with no baseline record.
HARD_RULES = frozenset(
    {"ambient_clock_read", "clock_trait_declaration", "defaulted_system_clock", "shadow_clock_helper"}
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


def _production(text: str) -> str:
    """The production projection: test-only items and comments blanked.

    Delegates to the shared lexer so this gate and every crate-boundary gate
    agree on what ships. Offsets and line numbers are preserved.
    """
    return workspace.production_rust(text)


def _is_test_path(relpath: str) -> bool:
    return (
        "/tests/" in relpath
        or relpath.endswith(("_tests.rs", "tests.rs", "test_support.rs", "testkit.rs"))
        or "/test_support/" in relpath
        or "-testkit/" in relpath
    )


def _line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def fabricated_literal_violations(sources: dict[str, str]) -> dict[str, list[str]]:
    """R1 findings, keyed by file."""
    findings: dict[str, list[str]] = {}
    for relpath, text in sorted(sources.items()):
        if _is_test_path(relpath) or DATETIME_LITERAL.search(text) is None:
            continue
        production = _production(text)
        hits: list[str] = []
        for number, line in enumerate(production.splitlines(), start=1):
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


def _clock_rule_violations(
    sources: dict[str, str],
    clock_port_files: frozenset[str],
    pattern: re.Pattern[str],
    message: str,
    *,
    composition_roots_exempt: bool,
) -> dict[str, list[str]]:
    """Findings of one clock-authority rule, keyed by file.

    Exempt: test paths, the clock-port crate's own files and, where the rule
    says so, the named composition roots. The raw text is searched first only
    to skip projecting a file that cannot match; every counted hit is found in
    the production projection.
    """
    findings: dict[str, list[str]] = {}
    for relpath, text in sorted(sources.items()):
        if _is_test_path(relpath) or relpath in clock_port_files:
            continue
        if composition_roots_exempt and relpath in COMPOSITION_ROOTS:
            continue
        if pattern.search(text) is None:
            continue
        production = _production(text)
        matches = list(pattern.finditer(production))
        hits = [
            f"{relpath}:{_line_of(production, match.start())}: "
            f"`{' '.join(match.group(0).split())}` {message}"
            for match in sorted(matches, key=lambda match: match.start())
        ]
        if hits:
            findings[relpath] = hits
    return findings


def ambient_clock_violations(
    sources: dict[str, str], clock_port_files: frozenset[str] = frozenset()
) -> dict[str, list[str]]:
    """R2 findings, keyed by file."""
    return _clock_rule_violations(
        sources,
        clock_port_files,
        AMBIENT_CLOCK,
        f"reads the real clock outside {CLOCK_PORT_CRATE}. Take its `WallClock` (or the "
        "caller's sampled instant) so a test can name the value this writes",
        composition_roots_exempt=True,
    )


def clock_trait_violations(
    sources: dict[str, str], clock_port_files: frozenset[str] = frozenset()
) -> dict[str, list[str]]:
    """R3 findings, keyed by file."""
    return _clock_rule_violations(
        sources,
        clock_port_files,
        CLOCK_TRAIT,
        f"declares a second wall-clock port. Take `{CLOCK_PORT_CRATE}::WallClock`; one "
        "trait is what keeps two owners of one aggregate on one timeline",
        composition_roots_exempt=False,
    )


def defaulted_clock_violations(
    sources: dict[str, str], clock_port_files: frozenset[str] = frozenset()
) -> dict[str, list[str]]:
    """R4 findings, keyed by file."""
    return _clock_rule_violations(
        sources,
        clock_port_files,
        DEFAULTED_SYSTEM_CLOCK,
        "chooses the real clock below the composition root. Require the clock as a "
        "constructor argument and let the root pass `system_clock()` once",
        composition_roots_exempt=True,
    )


def shadow_clock_violations(
    sources: dict[str, str], clock_port_files: frozenset[str] = frozenset()
) -> dict[str, list[str]]:
    """R5 findings, keyed by file."""
    return _clock_rule_violations(
        sources,
        clock_port_files,
        SHADOW_CLOCK_HELPER,
        "is a clock with no port. Read the injected `WallClock` (its `WallClockExt` "
        "spellings cover milliseconds and RFC-3339) instead of a private helper",
        composition_roots_exempt=False,
    )


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


def clock_port_files(root: Path, sources: dict[str, str]) -> frozenset[str]:
    """The files of the crate named ``CLOCK_PORT_CRATE``, derived from its manifest.

    Empty when no such crate exists: then nothing is exempt, which is the
    fail-closed reading of "the one place the clock may be read".
    """
    for manifest in workspace.iter_crate_manifests():
        if workspace.package_name(workspace.load_manifest(manifest)) != CLOCK_PORT_CRATE:
            continue
        prefix = manifest.parent.relative_to(root).as_posix() + "/"
        return frozenset(path for path in sources if path.startswith(prefix))
    return frozenset()


def selftest() -> None:
    """Decision tables for R1..R5 and the expiry rule.

    R1 causes: L1 a date literal assigned to an `*_at` field; L2 a constant
    named for an observation time holding one; L3 the same literal inside
    `#[cfg(test)]`; L4 the same inside a `tests/` file; L5 a date literal that
    reaches no observation field (a migration default keyed by another name, a
    version string); L6 a literal in a comment.
    R2 causes: C3 a concrete clock read inside the clock-port crate; C4 in a
    named composition root; C5 inside `#[cfg(test)]`; C6 in a file that
    declares its own clock trait (formerly a whole-file exemption); C8 a
    concrete clock value read in place (`SystemClock.now_ms()`, qualified,
    UFCS, `TokioModelInferenceClock.now()`, `system_clock().now()`); C9 a read
    under `cfg(any(test, feature = ...))`, which ships with the feature; C10 a
    read after a braceless `#[cfg(test)] use ...;`; C11 a handle read
    (`self.clock.now_ms()`); C26 a std/tokio/chrono library read, aliased or
    not (C1, C2, C7, C12-C25 before): clippy.toml's, not this rule's.
    R3 causes: T1 `trait *Clock` in an ordinary module; T2 inside the
    clock-port crate; T3 inside `#[cfg(test)]`.
    R4 causes: D1 `Arc::new(SystemClock)`; D2 the same path-qualified across
    lines; D3 `system_clock()` below the root; D4 `system_clock()` in a named
    composition root; D5 the factory's declaration in the clock-port crate;
    D6 `SystemXClock::default()`; D7 inside `#[cfg(test)]`.
    R5 causes: S1 a free `fn now_ms()`; S2 a method `fn now_rfc3339(&self)`;
    S3 inside the clock-port crate; S4 inside `#[cfg(test)]`.
    Effects: E1 admitted, E2 reported.
    Rules: R1: L1|L2 => E2; L3|L4|L5|L6 => E1.
           R2: C6|C8|C9|C10 => E2; C3|C4|C5|C11|C26 => E1.
           R3: T1 => E2; T2|T3 => E1.   R4: D1|D2|D3|D6 => E2; D4|D5|D7 => E1.
           R5: S1|S2 => E2; S3|S4 => E1.
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
    port = frozenset({"crates/sdk/support/awaken-time/src/clock.rs"})
    port_file = sorted(port)[0]
    concrete = "SystemClock.now_ms()"

    c3 = {port_file: "fn read() -> u64 { system_clock().now_ms() }\n"}
    assert not ambient_clock_violations(c3, port), "C3/E1"

    root = sorted(COMPOSITION_ROOTS)[0]
    assert not ambient_clock_violations({root: f"let t = {concrete};\n"}), "C4/E1"

    c5 = {"a.rs": f"#[cfg(test)]\nmod tests {{\n    fn t() {{ let _ = {concrete}; }}\n}}\n"}
    assert not ambient_clock_violations(c5), "C5/E1"

    c6 = {
        "worker.rs": "pub trait WorkerClock { fn now_ms(&self) -> u64; }\n"
        f"fn request_id() -> u64 {{ {concrete} }}\n"
    }
    assert ambient_clock_violations(c6), "C6/E2 declaring a clock no longer exempts a file"

    for spelling in (
        "let now = SystemClock.now_ms();",
        "let now = awaken_dispatch::SystemClock.now_ms();",
        "let id = SystemWorkerClock.now_ms();",
        "let now = Clock::now_ms(&awaken_agent_contract::clock::SystemClock);",
        "let started = TokioModelInferenceClock.now();",
        "let now = system_clock().now();",
    ):
        assert ambient_clock_violations({"a.rs": spelling + "\n"}), f"C8/E2 {spelling}"

    c9 = {
        "a.rs": '#[cfg(any(test, feature = "durable"))]\n'
        f"pub(crate) fn stamp() -> u64 {{ {concrete} }}\n"
    }
    assert ambient_clock_violations(c9), "C9/E2 a feature-gated item ships"

    c10 = {
        "a.rs": "#[cfg(test)]\nuse crate::fixtures::Item;\n\n"
        f"pub fn stamp() -> u64 {{\n    {concrete}\n}}\n"
    }
    hits = ambient_clock_violations(c10).get("a.rs", [])
    assert len(hits) == 1 and hits[0].startswith("a.rs:5:"), f"C10/E2 {hits}"

    c11 = {"a.rs": "fn stamp(&self) -> u64 { self.clock.now_ms() }\n"}
    assert not ambient_clock_violations(c11), "C11/E1"

    for library_read in (
        "fn stamp() -> u64 { SystemTime::now().elapsed() }",
        "fn started() -> Instant { Instant::now() }",
        "let expiry = chrono::Utc::now() + ttl;",
        "use std::time::Instant as DeadlineInstant;\nfn stamp() { let _ = DeadlineInstant::now(); }",
    ):
        assert not ambient_clock_violations({"a.rs": library_read + "\n"}), f"C26/E1 clippy's {library_read}"

    # ── R3 ──
    assert clock_trait_violations({"a.rs": "pub trait RegistryClock: Send {}\n"}), "T1/E2"
    assert not clock_trait_violations(
        {port_file: "pub trait WallClock: Send + Sync {}\n"}, port
    ), "T2/E1"
    assert not clock_trait_violations(
        {"a.rs": "#[cfg(test)]\nmod tests {\n    trait FakeClock {}\n}\n"}
    ), "T3/E1"

    # ── R4 ──
    assert defaulted_clock_violations({"a.rs": "clock: Arc::new(SystemClock),\n"}), "D1/E2"
    d2 = {"a.rs": "let store = Store::new(Arc::new(\n    awaken_dispatch::SystemClock,\n));\n"}
    d2_hits = defaulted_clock_violations(d2).get("a.rs", [])
    assert len(d2_hits) == 1 and d2_hits[0].startswith("a.rs:1:"), f"D2/E2 {d2_hits}"
    assert defaulted_clock_violations({"a.rs": "Store::open(system_clock())\n"}), "D3/E2"
    assert not defaulted_clock_violations({root: "Store::open(system_clock())\n"}), "D4/E1"
    assert not defaulted_clock_violations(
        {port_file: "pub fn system_clock() -> Arc<dyn WallClock> { Arc::new(SystemWallClock) }\n"},
        port,
    ), "D5/E1"
    assert defaulted_clock_violations(
        {"a.rs": "let c = SystemDataSubjectClock::default();\n"}
    ), "D6/E2"
    assert not defaulted_clock_violations(
        {"a.rs": "#[cfg(test)]\nmod tests {\n    fn t() { Arc::new(SystemClock); }\n}\n"}
    ), "D7/E1"
    assert not defaulted_clock_violations(
        {"a.rs": "let now = system_clock().now_ms();\n"}
    ), "an in-place read is R2's, not counted twice"

    # D8 cause/effect table: the same factory at the declared example main
    # is composition (accept); moving it back into CodingSession::new is an
    # ambient library default (reject). Migrate the one named root rather than
    # retain both choices or grow a pattern/waiver. Injected library handles
    # remain ordinary use of the one WallClock port, without selecting time.
    coding_main = "crates/sdk/devtools/awaken-runtime-examples/examples/coding_agent/main.rs"
    coding_library = "crates/sdk/devtools/awaken-runtime-examples/src/coding_agent/mod.rs"
    assert not defaulted_clock_violations({coding_main: "fn main() { system_clock(); }"}), "D8 root"
    assert defaulted_clock_violations({coding_library: "fn new() { system_clock(); }"}), "D8 library"
    assert not defaulted_clock_violations({coding_library: "fn new(clock: Arc<dyn WallClock>) { store(clock); }"}), "D8 injected"
    # Register only the two actual process roots, never their helper subtree.
    for qualification_main in (
        "crates/devtools/awaken-scenario-host/examples/application_credential_fixture.rs",
        "crates/devtools/awaken-scenario-host/examples/application_credential_acp_fixture.rs",
    ):
        assert not defaulted_clock_violations({qualification_main: "fn main() { system_clock(); }"})
    qualification_helper = "crates/devtools/awaken-scenario-host/examples/application_credential_fixture/execution.rs"
    assert defaulted_clock_violations({qualification_helper: "fn open() { system_clock(); }"})
    assert not defaulted_clock_violations({qualification_helper: "fn open(clock: Arc<dyn WallClock>) { store(clock); }"})

    # ── R5 ──
    assert shadow_clock_violations({"a.rs": "fn now_ms() -> u64 { 0 }\n"}), "S1/E2"
    assert shadow_clock_violations(
        {"a.rs": "impl Vaults {\n    fn now_rfc3339(&self) -> String { x() }\n}\n"}
    ), "S2/E2"
    assert not shadow_clock_violations(
        {port_file: "fn now(&self) -> UtcTime { UtcTime::UNIX_EPOCH }\n"}, port
    ), "S3/E1"
    assert not shadow_clock_violations(
        {"a.rs": "#[cfg(test)]\nmod tests {\n    fn now_ms() -> u64 { 0 }\n}\n"}
    ), "S4/E1"

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
    # The live baseline must itself satisfy the rule it enforces, and date
    # every rule this gate enforces.
    live = load_baseline(BASELINE_PATH)
    assert not expired_entries(live, today), "the shipped baseline"
    assert set(live) == {key for key, _ in RULES} - HARD_RULES, \
        "every ratcheted rule has a dated baseline and no hard rule has one"

    print("check_fabricated_facts selftest: R1..R5 and the expiry table hold")


def findings_by_rule(
    sources: dict[str, str], port_files: frozenset[str]
) -> dict[str, dict[str, list[str]]]:
    return {
        "fabricated_observation_literal": fabricated_literal_violations(sources),
        "ambient_clock_read": ambient_clock_violations(sources, port_files),
        "clock_trait_declaration": clock_trait_violations(sources, port_files),
        "defaulted_system_clock": defaulted_clock_violations(sources, port_files),
        "shadow_clock_helper": shadow_clock_violations(sources, port_files),
    }


def _scan(root: Path) -> tuple[frozenset[str], dict[str, dict[str, list[str]]]]:
    sources = _sources(root)
    port_files = clock_port_files(root, sources)
    return port_files, findings_by_rule(sources, port_files)


def main() -> int:
    baseline = load_baseline(BASELINE_PATH)
    port_files, findings = _scan(ROOT)
    problems: list[str] = []
    if not port_files:
        # The exemption is fail-closed (nothing is exempt), but a missing port
        # crate means every clock read has nowhere legitimate to move to.
        problems.append(
            f"{CLOCK_PORT_CRATE}: the crate that owns the one wall-clock port is missing "
            "from the workspace; R2..R5 name it as the only place the real clock is read"
        )
    for key, label in RULES:
        if key in HARD_RULES:
            if key in baseline:
                problems.append(f"{key}: a hard rule holds no baseline record; remove it")
            problems += ratchet_errors(label, findings[key], {})
            continue
        if key not in baseline:
            problems.append(f"{key}: no baseline record; every rule carries an owner and an expiry")
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
        for key, label in RULES if key not in HARD_RULES
    )
    print(f"OK - no invented observation times or split clock authority beyond the recorded baseline ({summary}).")
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
        _port_files, scanned = _scan(ROOT)
        for key, _label in RULES:
            for relpath, hits in sorted(scanned[key].items()):
                print(f"{key}\t{relpath}\t{len(hits)}")
        raise SystemExit(0)
    raise SystemExit(main())
