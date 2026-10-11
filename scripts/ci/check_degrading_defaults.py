#!/usr/bin/env python3
"""Refuse a trait default that silently degrades to a less-qualified sibling.

One shape produced the largest share of this repository's recurring defects.
A trait declares a qualified operation and gives it a default body that drops
the qualifier and calls the unqualified sibling:

    async fn create_as(&self, store, path, content, actor: Option<&MemoryActor>)
        -> Result<Memory, MemErr>
    {
        let _ = actor;                       // the qualifier is discarded
        self.create(store, path, content).await
    }

An implementation that does not override it satisfies the trait while providing
none of what the qualifier names, and the caller cannot tell. The qualifier has
been the tenant, the effect fence, the operation identity, the claim, the
attribution and the observation — which is why fixes for these read as six
unrelated families instead of one shape.

The rule is not "no defaults". A default that fails closed is fine: an
implementation that forgets it gets an error, not a quiet downgrade. What is
refused is a default that *succeeds* by calling a weaker sibling.

A second shape says the same thing one layer out. A *read* that can fail is
defaulted:

    let runtimes = store.list(workspace).await.unwrap_or_default();

"the store could not tell me" and "there is nothing there" are different facts,
and `unwrap_or_default` renders the first as the second. One instance of exactly
this (`3fe9d936a9`) reported a store outage to the caller as "no runtime". It is
the same defect as the degrading default -- an operation succeeds while providing
none of what it names -- so it is gated here rather than somewhere new.

A third shape says it from the other side. Instead of dropping a qualifier the
signature names, an implementation *accepts* one and then substitutes its own:

    async fn renew_lease(&self, claim: &RunClaim, lease_ms: u64, _now_ms: u64) {
        let now_ms = postgres_now_ms(&mut *tx).await?;   // the caller's fact,
        ...                                              // discarded and reissued

The caller passed a fact the signature asked for; the implementation answered
with a different one under the same name. Ignoring a parameter is not the
defect -- a test double ignores everything and lies to no one. Discarding it and
binding its own value of the same name is, because the signature now means two
things: whatever the honouring implementations do with it, and whatever this one
substitutes. `lease_renewal_on_postgres` is what that cost: the same lease call
was evaluated against the caller's clock on two backends and the database clock
on the third, so a shared property held for two and was unprovable for the
third, and the test failed about one run in ten on a loaded machine. Which
authority is right is a port decision; a signature that implies both is not a
decision, it is a silence. That one was settled by taking the parameter off the
port, which is why this count now sits at zero rather than at eight.

Whole-repo scan plus an exemption registry, never a file allow-list: a list
freezes today's instances, a scan also refuses tomorrow's.

Every count is ratcheted, and each ratchet records **who owns retiring it and
by when**. An undated allowance is how a ratchet stops being temporary: the
number stops falling, nobody owns it, and the gate ends up certifying the debt.
The self-test checks all three records' owner, reason and expiry;
`check_fabricated_facts.py` also validates the degrading-default record from
this source. Those existing owners keep the date policy; the default quantity
check does not introduce another expiry checker.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

import _architecture_ratchet
from _architecture_ratchet import Rule

ROOT = Path(__file__).resolve().parents[2]

RULES = (
    Rule("degrading-default", "Require the method, make its default fail closed, or review the exact DEGRADING_DEFAULT_EXEMPTIONS decision"),
    Rule("fail-open-read", "Propagate the read error or review the exact FAIL_OPEN_READ_EXEMPTIONS decision"),
    Rule("substituted-input", "Take the parameter off the port, honour the argument, or review the exact SUBSTITUTED_INPUT_EXEMPTIONS decision"),
)

# Key: "<trait>::<method>". Value: why the degraded call is the correct
# behaviour for an implementation that does not override it. An entry is a
# decision someone made and can be argued with.
DEGRADING_DEFAULT_EXEMPTIONS: dict[str, str] = {
    "RunApplication::resume_streaming": (
        "Best-effort observation of the same authoritative resume mutation. "
        "Non-live engines return the committed outcome; the sole protocol encoder "
        "projects that outcome as its documented fallback. SessionRunApplication "
        "overrides it to register live observation before reply publication. "
        "AI SDK streaming/router_errors tests cover both live and committed paths."
    ),
    "AcpToolExporter::export": (
        "not a degrade: `export` has nothing to drop. It is the singular "
        "spelling of `export_set`, delegating with a one-element set, which is "
        "exactly what exporting one tool means"
    ),
    "CommittedThreadView::run_state": (
        "not a degrade: `run_state` asks a narrower question of the same input "
        "than `run`, and the default answers it by reading the state off the "
        "record `run` returns. Nothing the signature names is discarded"
    ),
    "FileApplicationService::create_uploaded_file": (
        "not a degrade: the default supplies an explicit `None` expiry, which "
        "is what an upload with no expiry means. The caller gets exactly what "
        "the signature promises"
    ),
    "FileApplicationService::create_uploaded_file_with_expiry": (
        "not a degrade: the default supplies an explicit `None` idempotency "
        "key, which is what a non-idempotent upload means"
    ),
    # The five attribution pairs: the attributed method is the required one and
    # these are its unattributed spellings, each delegating with an explicit
    # `None` author. An implementation cannot satisfy the trait without
    # answering the attributed question, which is the property the rule is
    # protecting; see `MemoryRepository::create_as`.
    "MemoryRepository::create": (
        "not a degrade: the unattributed spelling of the required `create_as`, "
        "delegating with an explicit `None` author"
    ),
    "MemoryRepository::update_head": (
        "not a degrade: the unattributed spelling of the required "
        "`update_head_as`, delegating with an explicit `None` author"
    ),
    "MemoryRepository::delete_by_path": (
        "not a degrade: the unattributed spelling of the required "
        "`delete_by_path_as`, delegating with an explicit `None` author"
    ),
    "MemoryRepository::delete_if_match": (
        "not a degrade: the unattributed spelling of the required "
        "`delete_if_match_as`, delegating with an explicit `None` author"
    ),
    "MemoryRepository::redact_version": (
        "not a degrade: the unattributed spelling of the required "
        "`redact_version_as`, delegating with an explicit `None` author"
    ),
    "MemoryRepository::update": (
        "not a degrade: the content-only convenience over `update_head`, "
        "delegating with an explicit `None` target path, which is what an "
        "update that does not move the memory means"
    ),
    "EnvRegistry::create": (
        "not a degrade: `create` has no `scope` parameter to drop. It is the "
        "unqualified spelling delegating to the qualified `create_scoped` with an "
        "explicit `None`, which is what an unscoped create means — the caller gets "
        "exactly what the signature promises"
    ),
}

# The shape predates the gate. Converting every instance at once would be a
# migration, not a fix, so the untriaged remainder is ratcheted instead:
# the count may fall and may not rise. That is what makes this gate prevent
# *new* instances today rather than only after the migration finishes — the
# failure mode of a file allow-list, which freezes the debt and guards nothing.
# Lower this number as instances are converted or registered; never raise it.
UNTRIAGED_BASELINE = 12
DEGRADING_DEFAULT_RATCHET = {
    "owner": "chaizhenhua",
    "expires": "2026-12-31",
    "reason": (
        "Each remaining default is a separate port decision: making it required "
        "changes every implementor, and making it fail closed changes a live "
        "call path. They are converted one at a time, each with a test proving "
        "the new refusal, not in one sweep."
    ),
}

# A fallible read defaulted to emptiness. The receiver is an `.await`, which is
# the I/O boundary: an awaited read can fail for reasons that are not absence,
# and `unwrap_or_default` collapses the two. A pure `Option` from an in-memory
# lookup is deliberately NOT in scope — nothing failed there, so nothing is
# being hidden.
FAIL_OPEN_READ = re.compile(r"\.await\s*(?://[^\n]*)?\s*\.unwrap_or_default\(\)")

# Key: "<relative path>:<line>". Value: why emptiness is the right answer for a
# read that could not be performed. Two admissible reasons recur: the value is
# only ever used to enrich a message about a failure that is already being
# reported, and the read is a documented best-effort enrichment whose absence
# the caller already handles identically.
FAIL_OPEN_READ_EXEMPTIONS: dict[str, str] = {
    "crates/control/awaken-control/src/admin_assistant.rs": (
        "the assistant's platform-capability projection is best-effort by "
        "design at its only call site, which already answers an unreachable "
        "config plane with the same empty inventory (`Err(_) => Vec::new()` "
        "beside it). Both halves therefore degrade identically, and no caller "
        "can distinguish the two"
    ),
    "crates/server/awaken-coordinator-client/src/dispatch_client.rs": (
        "the defaulted read is the body of a response already known to be an "
        "error; an unreadable body makes the detail string empty, and the error "
        "itself is raised either way"
    ),
    "crates/runtime/awaken-ext-memory/src/plugin.rs": (
        "recall is fail-soft by design (ADR-0055): a memory-store outage must "
        "not fail the turn, and an empty recall is the documented degraded "
        "behaviour the caller already handles"
    ),
}

# The remainder, ratcheted per file exactly like the defaults above.
FAIL_OPEN_READ_BASELINE: dict[str, int] = {}
FAIL_OPEN_READ_RATCHET = {
    "owner": "chaizhenhua",
    "expires": "2026-12-31",
    "reason": (
        "Each remaining read is behind a port that returns a bare value rather "
        "than a Result, so converting it is a port change plus a caller change; "
        "they are done one at a time with a test proving the new refusal."
    ),
}

# An input the signature names, discarded by the body and reissued from another
# authority under the same name. The underscore is what makes it findable: the
# author had to silence the compiler to keep the parameter while not using it,
# and then bound `x` anyway.
#
# Key: "<relative path>::<fn>". Value: why this implementation is entitled to
# answer with its own value instead of the caller's. The only argument that
# holds is that the substituted authority is the *correct* one for every caller
# -- in which case the parameter does not belong in the signature, and the entry
# is a note that the port change is owed, not that the shape is fine.
SUBSTITUTED_INPUT_EXEMPTIONS: dict[str, str] = {}

# Zero, and it stays zero. The eight instances this rule was written for were
# all one decision in the Postgres dispatch backend: a lease is a
# mutual-exclusion primitive, its expiry must be judged by the one process every
# contender talks to, so that backend read `clock_timestamp()` and answered with
# it under the caller's parameter name. The fix was to take `now_ms` off the
# `DispatchQueue` port entirely -- the store owns its clock -- rather than to
# make Postgres trust a worker's clock, because a claimant that times its own
# lease can extend it without bound, which is the split-brain the lease exists
# to prevent.
#
# One parameter survived that sweep and is the reason this rule is worth
# keeping: `quarantine_retry_exhausted` still takes a caller-supplied time,
# because an operator picks it ("quarantine what was already expired at T"). It
# is now named `expired_before_ms`. Under its old name, `now_ms`, one backend
# had quietly substituted its own "now" for the operator's cutoff -- a different
# question, answered without anyone being told. A name that says what a value is
# is what makes that substitution visible.
SUBSTITUTED_INPUT_BASELINE = 0
SUBSTITUTED_INPUT_RATCHET = {
    "owner": "chaizhenhua",
    "expires": "2026-12-31",
    "reason": (
        "Nothing is owed: the shape is gone from the repository. The record "
        "stays so the rule keeps its owner and its review date, and so a future "
        "instance has to be argued for rather than absorbed into a count."
    ),
}

TRAIT = re.compile(r"pub trait (\w+)[^{;]*\{")
METHOD = re.compile(r"(?:async )?fn ([a-z_]\w*)\((?:[^()]|\([^()]*\))*\)\s*->\s*[^;{]*\{")


def _brace_span(text: str, open_index: int) -> int:
    depth = 0
    index = open_index
    while index < len(text):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return len(text) - 1


def degrading_default_findings(sources: dict[str, str]) -> list[tuple[str, str]]:
    """Raw (trait::method, message) findings; decisions are checked separately."""
    found: list[tuple[str, str]] = []
    for relpath, text in sorted(sources.items()):
        for trait_match in TRAIT.finditer(text):
            open_index = text.index("{", trait_match.start())
            body = text[open_index : _brace_span(text, open_index) + 1]
            siblings = {m.group(1) for m in re.finditer(r"fn ([a-z_]\w*)\(", body)}
            for method in METHOD.finditer(body):
                name = method.group(1)
                impl = body[method.end() - 1 : _brace_span(body, method.end() - 1) + 1]
                if "Err(" in impl:
                    continue  # a default that fails closed is not a downgrade
                # Two independent signals, either of which means the default
                # provides less than the signature promises:
                #   (a) a parameter is explicitly thrown away (`let _ = x;`)
                #       and the body then delegates — the qualifier is named in
                #       the signature and discarded in the body;
                #   (b) the delegate's name is the qualified name's stem, so the
                #       default is the unqualified operation under a qualified
                #       name. (a) is the precise one and catches an instance
                #       whose names are unrelated; (b) catches a drop that is
                #       implicit rather than spelled `let _ =`.
                discards = bool(re.search(r"let\s+_\s*(?::[^=]+)?=\s*\w+\s*;", impl))
                for sibling in siblings:
                    if sibling == name or not re.search(rf"self\.{sibling}\(", impl):
                        continue
                    stem = name.split("_for_")[0]
                    related = name.startswith(sibling) or sibling.startswith(stem)
                    if not (discards or related):
                        continue
                    key = f"{trait_match.group(1)}::{name}"
                    line = text[: open_index + method.start()].count("\n") + 1
                    found.append((key,
                        f"{relpath}:{line}: {key} defaults to self.{sibling}(), dropping what "
                        f"{name!r} names; an implementation that omits it satisfies the trait "
                        "while providing none of the qualifier. Require the method (no default), "
                        "make the default fail closed, or record it in "
                        "DEGRADING_DEFAULT_EXEMPTIONS"
                    ))
                    break
    return found


def _paren_span(text: str, open_index: int) -> int:
    depth = 0
    for index in range(open_index, len(text)):
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth == 0:
                return index
    return len(text)


def _declared_parameters(signature: str) -> list[str]:
    """Parameter names in one signature, `self` and unnamed receivers removed."""
    parts: list[str] = []
    depth = 0
    current = ""
    for character in signature:
        if character in "<([":
            depth += 1
        elif character in ">)]":
            depth -= 1
        if character == "," and depth == 0:
            parts.append(current)
            current = ""
        else:
            current += character
    if current.strip():
        parts.append(current)
    names: list[str] = []
    for part in parts:
        part = part.strip()
        if not part or part.lstrip("&").startswith("self") or ":" not in part:
            continue
        names.append(part.split(":", 1)[0].strip())
    return names


def substituted_input_findings(sources: dict[str, str]) -> list[tuple[str, str]]:
    """Raw (path::function, message) substitutions, before decision filtering."""
    found: list[tuple[str, str]] = []
    for relpath, text in sorted(sources.items()):
        for match in re.finditer(r"(?:async\s+)?fn\s+([a-z_]\w*)\s*\(", text):
            open_paren = text.index("(", match.end() - 1)
            close_paren = _paren_span(text, open_paren)
            # A trait *declaration* ends at `;` and has no body to substitute in.
            brace = text.find("{", close_paren)
            semicolon = text.find(";", close_paren)
            if brace < 0 or (semicolon != -1 and semicolon < brace):
                continue
            body = text[brace : _brace_span(text, brace) + 1]
            for name in _declared_parameters(text[open_paren + 1 : close_paren]):
                if not name.startswith("_") or name == "_":
                    continue
                bare = name.lstrip("_")
                if not re.search(rf"\blet\s+(?:mut\s+)?{re.escape(bare)}\s*[=:]", body):
                    continue
                key = f"{relpath}::{match.group(1)}"
                line = text[: match.start()].count("\n") + 1
                found.append((key,
                    f"{relpath}:{line}: {match.group(1)} takes {name} and then binds its own "
                    f"{bare!r}, so the caller's value is discarded and answered with another "
                    "authority under the same name. Remove the parameter from the port if the "
                    "substituted authority is the right one for every caller, honour the "
                    "argument if it is not, or record the decision in "
                    "SUBSTITUTED_INPUT_EXEMPTIONS"
                ))
    return found


def fail_open_read_violations(sources: dict[str, str]) -> dict[str, list[str]]:
    """Every `.await.unwrap_or_default()`, keyed by file (exemptions removed)."""
    findings: dict[str, list[str]] = {}
    for relpath, text in sorted(sources.items()):
        if relpath in FAIL_OPEN_READ_EXEMPTIONS:
            continue
        hits = [
            f"{relpath}:{text[: match.start()].count(chr(10)) + 1}: an awaited read is "
            "defaulted to empty, so a read that could not be performed is reported as "
            "nothing found. Propagate the error, or register the decision in "
            "FAIL_OPEN_READ_EXEMPTIONS"
            for match in FAIL_OPEN_READ.finditer(text)
        ]
        if hits:
            findings[relpath] = hits
    return findings


def fail_open_read_errors(sources: dict[str, str]) -> list[str]:
    """Keep the original per-file limit and independent stale-file rule.

    D2/D8: B1-B3 cross hard stale/equal/grow/fall and per-file cancellation;
    formatting the explicit stale state cannot add it to actual debt.
    """
    findings = fail_open_read_violations(sources)
    rule = RULES[1]
    hard = [_architecture_ratchet.diagnostic(
                rule.name, path, "registered", "missing", "remove its stale decision from FAIL_OPEN_READ_EXEMPTIONS",
                details=f"FAIL_OPEN_READ_EXEMPTIONS names {path}, which no longer defaults a fallible read")
            for path in sorted(FAIL_OPEN_READ_EXEMPTIONS)
            if path not in sources or not FAIL_OPEN_READ.search(sources[path])]
    return hard + _architecture_ratchet.compare(
        (rule,), {rule.name: {path: len(hits) for path, hits in findings.items()}},
        {rule.name: findings}, {rule.name: FAIL_OPEN_READ_BASELINE},
        f"{__file__}::FAIL_OPEN_READ_BASELINE",
        lower_advice=f"lower the exact file allowance in {__file__}::FAIL_OPEN_READ_BASELINE",
        full_details=True,
    )


def _sources(root: Path) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): path.read_text(encoding="utf-8", errors="ignore")
        for path in sorted((root / "crates").rglob("*.rs"))
        if not path.name.endswith(("tests.rs", "test_support.rs")) and "/tests/" not in str(path)
    }


def selftest() -> None:
    # Causes: C1 default delegates to a weaker sibling; C2 default fails closed;
    # C3 no default at all; C4 the degrade is registered; C5 a registry entry
    # names a method that no longer degrades.
    # Effects: E1 admitted, E2 rejected.
    # R1 = C1 => E2.  R2 = C2|C3|C4 => E1.  R3 = C5 => E2.
    global DEGRADING_DEFAULT_EXEMPTIONS
    DEGRADING_DEFAULT_EXEMPTIONS = {}

    def defaults(sources):
        return _architecture_ratchet.compare_registered(
            degrading_default_findings(sources), DEGRADING_DEFAULT_EXEMPTIONS, 0,
            "DEGRADING_DEFAULT_EXEMPTIONS", rule=RULES[0], baseline_name="test fixture::UNTRIAGED_BASELINE")

    def substitutions(sources):
        return _architecture_ratchet.compare_registered(
            substituted_input_findings(sources), SUBSTITUTED_INPUT_EXEMPTIONS, 0,
            "SUBSTITUTED_INPUT_EXEMPTIONS", rule=RULES[2], baseline_name="test fixture::SUBSTITUTED_INPUT_BASELINE")

    degrading = {"a.rs": (
        "pub trait Repo {\n"
        "    async fn write(&self, p: &str) -> R;\n"
        "    async fn write_as(&self, p: &str, actor: A) -> R {\n"
        "        let _ = actor;\n"
        "        self.write(p).await\n"
        "    }\n"
        "}\n"
    )}
    assert defaults(degrading), "C1/E2"

    fail_closed = {"a.rs": (
        "pub trait Repo {\n"
        "    async fn write(&self, p: &str) -> R;\n"
        "    async fn write_as(&self, p: &str, actor: A) -> R {\n"
        "        Err(Error::new(\"unimplemented\"))\n"
        "    }\n"
        "}\n"
    )}
    assert not defaults(fail_closed), "C2/E1"

    required = {"a.rs": "pub trait Repo {\n    async fn write_as(&self, p: &str, actor: A) -> R;\n}\n"}
    assert not defaults(required), "C3/E1"

    DEGRADING_DEFAULT_EXEMPTIONS = {"Repo::write_as": "test"}
    assert not defaults(degrading), "C4/E1"
    assert defaults(required), "C5/E2"
    DEGRADING_DEFAULT_EXEMPTIONS = {}

    # Fail-open reads. Causes: F1 an awaited read defaulted to empty; F2 the same
    # call with the error propagated; F3 `unwrap_or_default` on a source that
    # cannot fail (a plain Option from an in-memory lookup, no await); F4 a
    # registered file; F5 a registry entry whose file no longer defaults a read.
    # Effects: E1 admitted, E2 rejected. Rules: F1 => E2; F2|F3|F4 => E1;
    # F5 => E2.
    global FAIL_OPEN_READ_EXEMPTIONS, FAIL_OPEN_READ_BASELINE
    FAIL_OPEN_READ_EXEMPTIONS, FAIL_OPEN_READ_BASELINE = {}, {}

    fail_open = {"a.rs": "let rows = store.list(id).await.unwrap_or_default();\n"}
    assert fail_open_read_violations(fail_open), "F1/E2"
    assert fail_open_read_errors(fail_open), "F1/E2 ratchet"

    propagated = {"a.rs": "let rows = store.list(id).await?;\n"}
    assert not fail_open_read_violations(propagated), "F2/E1"

    infallible = {"a.rs": "let name = map.get(id).cloned().unwrap_or_default();\n"}
    assert not fail_open_read_violations(infallible), "F3/E1"

    FAIL_OPEN_READ_EXEMPTIONS = {"a.rs": "the body of an error already reported"}
    assert not fail_open_read_violations(fail_open), "F4/E1"
    assert fail_open_read_errors(propagated), "F5/E2 stale registry entry"
    FAIL_OPEN_READ_EXEMPTIONS = {}

    # The ratchet holds a known remainder and refuses one more.
    FAIL_OPEN_READ_BASELINE = {"a.rs": 1}
    assert not fail_open_read_errors(fail_open), "a held ratchet is admitted"
    assert fail_open_read_errors(
        {"a.rs": fail_open["a.rs"] * 2}
    ), "a rise is refused"
    assert fail_open_read_errors({"a.rs": ""}), "a fall must lower the baseline"
    FAIL_OPEN_READ_BASELINE = {}

    # Causes: S1 a `_x` parameter whose body binds its own `x`; S2 a `_x`
    # parameter the body never rebinds (a double that ignores, and lies to
    # nobody); S3 an honoured parameter rebound from itself, which is ordinary
    # Rust and not a substitution; S4 a trait declaration, which has no body;
    # S5 the substitution is registered; S6 a registry entry that no longer
    # substitutes.
    # Effects: E1 admitted, E2 rejected.
    # R1 = S1 => E2.  R2 = S2|S3|S4|S5 => E1.  R3 = S6 => E2.
    global SUBSTITUTED_INPUT_EXEMPTIONS
    SUBSTITUTED_INPUT_EXEMPTIONS = {}

    substituted = {
        "a.rs": "fn renew(&self, _now_ms: u64) -> u64 {\n    let now_ms = db_now();\n    now_ms\n}\n"
    }
    assert substitutions(substituted), "S1/E2"

    ignored = {"a.rs": "fn renew(&self, _now_ms: u64) -> u64 {\n    0\n}\n"}
    assert not substitutions(ignored), "S2/E1"

    honoured = {
        "a.rs": "fn renew(&self, now_ms: u64) -> u64 {\n    let now_ms = now_ms + 1;\n    now_ms\n}\n"
    }
    assert not substitutions(honoured), "S3/E1"

    declared = {"a.rs": "trait Q {\n    fn renew(&self, _now_ms: u64) -> u64;\n}\n"}
    assert not substitutions(declared), "S4/E1"

    SUBSTITUTED_INPUT_EXEMPTIONS = {"a.rs::renew": "the database is the only authority"}
    assert not substitutions(substituted), "S5/E1"
    assert substitutions(ignored), "S6/E2 stale registry entry"
    SUBSTITUTED_INPUT_EXEMPTIONS = {}

    # Every ratchet must carry an owner, an argued reason and a live expiry —
    # `check_fabricated_facts.py` enforces the same rule from outside.
    import datetime as _dt

    for name, record in (
        ("DEGRADING_DEFAULT_RATCHET", DEGRADING_DEFAULT_RATCHET),
        ("FAIL_OPEN_READ_RATCHET", FAIL_OPEN_READ_RATCHET),
        ("SUBSTITUTED_INPUT_RATCHET", SUBSTITUTED_INPUT_RATCHET),
    ):
        assert record["owner"].strip(), f"{name} owner"
        assert record["reason"].strip(), f"{name} reason"
        assert _dt.date.fromisoformat(record["expires"]) >= _dt.date.today(), (
            f"{name} has expired: convert the remainder or re-argue and re-date it"
        )

    _selftest_entrypoint()
    print("check_degrading_defaults selftest: all three decision tables hold")


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_degrading_defaults.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the decision table instead of the scan"
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    if parse(sys.argv[1:] if argv is None else argv).self_test:
        selftest()
        return 0
    sources = _sources(ROOT)
    errors = fail_open_read_errors(sources)
    errors += _architecture_ratchet.compare_registered(
        substituted_input_findings(sources), SUBSTITUTED_INPUT_EXEMPTIONS, SUBSTITUTED_INPUT_BASELINE,
        "SUBSTITUTED_INPUT_EXEMPTIONS", rule=RULES[2],
        baseline_name=f"{__file__}::SUBSTITUTED_INPUT_BASELINE",
    )
    errors += _architecture_ratchet.compare_registered(
        degrading_default_findings(sources), DEGRADING_DEFAULT_EXEMPTIONS, UNTRIAGED_BASELINE,
        "DEGRADING_DEFAULT_EXEMPTIONS", rule=RULES[0],
        baseline_name=f"{__file__}::UNTRIAGED_BASELINE",
    )
    if errors:
        for problem in errors:
            print(f"ERROR: {problem}", file=sys.stderr)
        return 1
    held = sum(FAIL_OPEN_READ_BASELINE.values())
    print(
        f"OK - degrading defaults held at the baseline of {UNTRIAGED_BASELINE} "
        f"(owner {DEGRADING_DEFAULT_RATCHET['owner']}, expires "
        f"{DEGRADING_DEFAULT_RATCHET['expires']}); fail-open reads held at {held} "
        f"(owner {FAIL_OPEN_READ_RATCHET['owner']}, expires "
        f"{FAIL_OPEN_READ_RATCHET['expires']}); substituted inputs held at "
        f"{SUBSTITUTED_INPUT_BASELINE} (owner {SUBSTITUTED_INPUT_RATCHET['owner']}, "
        f"expires {SUBSTITUTED_INPUT_RATCHET['expires']})."
    )
    return 0


def _selftest_entrypoint() -> None:
    """B1-B4 cause/effect graph and true-entrypoint decision table.

    Valid argv -> one source snapshot -> raw shape findings; exact decision
    keys filter actual debt. Independently stale keys -> hard failure. Global
    default/substitution and per-file read counts compare only actual debt.
    No hard error can consume a held count; no cross-file fall offsets growth.
    Effects: status, stdout/stderr, once-only reads/matchers, original metadata
    and no writer. B1 stale x debt relation; B2 original predicates/exemptions;
    B3 per-file + original metadata owner; B4 true main + strict parser.
    """
    import contextlib
    import datetime as dt
    import io
    from unittest.mock import patch

    default = ("pub trait Repo {\n    fn write(&self) -> R;\n"
               "    fn write_as(&self, actor: A) -> R { let _ = actor; self.write() }\n}\n")
    substitute = "fn renew(_now_ms: u64) -> u64 { let now_ms = db_now(); now_ms }\n"
    read = "let rows = store.list(id).await.unwrap_or_default();\n"
    original_defaults, original_substitutions = degrading_default_findings, substituted_input_findings
    original_reads = fail_open_read_violations

    def invoke(texts, argv=(), **overrides):
        observed = []

        def snapshot(_root):
            observed.append("sources")
            return texts

        def defaults(sources):
            observed.append("defaults")
            return original_defaults(sources)

        def substitutions(sources):
            observed.append("substitutions")
            return original_substitutions(sources)

        def reads(sources):
            observed.append("reads")
            return original_reads(sources)

        values = {"_sources": snapshot, "degrading_default_findings": defaults,
                  "substituted_input_findings": substitutions, "fail_open_read_violations": reads,
                  "DEGRADING_DEFAULT_EXEMPTIONS": {}, "FAIL_OPEN_READ_EXEMPTIONS": {},
                  "SUBSTITUTED_INPUT_EXEMPTIONS": {}, "UNTRIAGED_BASELINE": 0,
                  "SUBSTITUTED_INPUT_BASELINE": 0, "FAIL_OPEN_READ_BASELINE": {}, **overrides}
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.dict(main.__globals__, values), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            try:
                status = main(list(argv))
            except SystemExit as error:
                status = error.code
        if observed:
            assert sorted(observed) == ["defaults", "reads", "sources", "substitutions"], "B4 one source/each matcher"
        return status, stdout.getvalue(), stderr.getvalue(), observed

    # B1 debt 0/1/2 x held1 x stale/no stale. The stale0actual/held1 case
    # previously became len(errors)==held and the real entrypoint printed OK.
    for count in (0, 1, 2):
        texts = {f"{index}.rs": default.replace("Repo", f"Repo{index}") for index in range(count)}
        for stale in (False, True):
            status, output, errors, _ = invoke(texts, UNTRIAGED_BASELINE=1,
                DEGRADING_DEFAULT_EXEMPTIONS={"Gone::write_as": "reviewed"} if stale else {})
            assert status == (1 if stale or count != 1 else 0), "B1 hard cannot consume default debt"
            if stale:
                assert "registered -> missing" in errors and not output, "B1 stale always stderr/failure"
    # B1 same isolation for substituted inputs: a missing registered function
    # remains hard at equality, below and above a held global allowance.
    for count in (0, 1, 2):
        texts = {f"{index}.rs": substitute for index in range(count)}
        for stale in (False, True):
            status, _, errors, _ = invoke(texts, SUBSTITUTED_INPUT_BASELINE=1,
                SUBSTITUTED_INPUT_EXEMPTIONS={"gone.rs::renew": "reviewed"} if stale else {})
            assert status == (1 if stale or count != 1 else 0), "B1 hard cannot consume substituted debt"
            if stale:
                assert "registered -> missing" in errors, "B1 substituted stale hard"
    # B2 exact valid decisions still exempt all occurrences, without broadening
    # another method/file/rule or introducing a CLI reason authorization.
    assert invoke({"a.rs": default}, DEGRADING_DEFAULT_EXEMPTIONS={"Repo::write_as": "test"})[0] == 0, "B2 default decision"
    assert invoke({"a.rs": substitute}, SUBSTITUTED_INPUT_EXEMPTIONS={"a.rs::renew": "test"})[0] == 0, "B2 substitute decision"
    assert invoke({"a.rs": read}, FAIL_OPEN_READ_EXEMPTIONS={"a.rs": "test"})[0] == 0, "B2 read decision"
    # B3 total is two in both snapshots, but a.rs grows while b.rs falls.
    # A global sum comparison would incorrectly pass; per-file compare must fail.
    status, _, errors, _ = invoke({"a.rs": read * 2, "b.rs": ""}, FAIL_OPEN_READ_BASELINE={"a.rs": 1, "b.rs": 1})
    assert status == 1 and "a.rs" in errors and "b.rs" in errors, "B3 per-file growth never offsets a fall"
    assert invoke({"a.rs": ""}, FAIL_OPEN_READ_EXEMPTIONS={"a.rs": "test"})[0] == 1, "B3 stale read file hard"
    # B3 reuse the original metadata owner, not a second expiry algorithm.
    # These are copies: all three inline owner/reason/expires records stay exact.
    import check_fabricated_facts

    today = dt.date.today()
    for name, record in (("default", DEGRADING_DEFAULT_RATCHET), ("read", FAIL_OPEN_READ_RATCHET),
                         ("substitute", SUBSTITUTED_INPUT_RATCHET)):
        assert not check_fabricated_facts.expired_entries({name: {**record, "files": {}}}, today), "B3 original metadata valid"
        for change in ({"expires": (today - dt.timedelta(days=1)).isoformat()}, {"owner": " "}, {"reason": ""}):
            assert check_fabricated_facts.expired_entries({name: {**record, **change, "files": {}}}, today), "B3 original owner refuses invalid metadata"
    # B4 strict parser precedes snapshot/matchers; no fake write/check alias.
    for argv in (["--self-tes"], ["--selftest"], ["--write-baseline"], ["--reason", "reviewed"], ["--unknown"]):
        status, _, errors, observed = invoke({}, argv)
        assert status == 2 and "usage:" in errors and not observed, "B4 refusal before source read"
    # Self-test dispatch is exercised without recursively invoking this table.
    calls = []
    with patch.dict(main.__globals__, {"selftest": lambda: calls.append("selftest")}):
        status, _, _, observed = invoke({}, ["--self-test"])
    assert status == 0 and calls == ["selftest"] and not observed, "B4 existing self-test mode"


if __name__ == "__main__":
    raise SystemExit(main())
