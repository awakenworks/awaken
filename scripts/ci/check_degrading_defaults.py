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

Whole-repo scan plus an exemption registry, never a file allow-list: a list
freezes today's instances, a scan also refuses tomorrow's.

Both counts are ratcheted, and each ratchet records **who owns retiring it and
by when**. An undated allowance is how a ratchet stops being temporary: the
number stops falling, nobody owns it, and the gate ends up certifying the debt.
An expired entry fails, which forces the choice -- finish the conversion, or
re-argue the allowance and re-date it. `check_fabricated_facts.py` asserts that
this file still carries both records.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Key: "<trait>::<method>". Value: why the degraded call is the correct
# behaviour for an implementation that does not override it. An entry is a
# decision someone made and can be argued with.
DEGRADING_DEFAULT_EXEMPTIONS: dict[str, str] = {
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
UNTRIAGED_BASELINE = 28
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
FAIL_OPEN_READ_BASELINE: dict[str, int] = {
    "crates/bin/awaken-cli/src/control_component.rs": 2,
    "crates/control/awaken-control/src/admin_assistant.rs": 3,
    "crates/server/awaken-dream-application/src/lib.rs": 1,
}
FAIL_OPEN_READ_RATCHET = {
    "owner": "chaizhenhua",
    "expires": "2026-12-31",
    "reason": (
        "Each remaining read is behind a port that returns a bare value rather "
        "than a Result, so converting it is a port change plus a caller change; "
        "they are done one at a time with a test proving the new refusal."
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


def degrading_default_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    seen: set[str] = set()
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
                    seen.add(key)
                    if key in DEGRADING_DEFAULT_EXEMPTIONS:
                        break
                    line = text[: open_index + method.start()].count("\n") + 1
                    errors.append(
                        f"{relpath}:{line}: {key} defaults to self.{sibling}(), dropping what "
                        f"{name!r} names; an implementation that omits it satisfies the trait "
                        "while providing none of the qualifier. Require the method (no default), "
                        "make the default fail closed, or record it in "
                        "DEGRADING_DEFAULT_EXEMPTIONS"
                    )
                    break
    for key in sorted(DEGRADING_DEFAULT_EXEMPTIONS):
        if key not in seen:
            errors.append(
                f"DEGRADING_DEFAULT_EXEMPTIONS names {key}, which no longer degrades; "
                "drop the stale exemption"
            )
    return errors


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
    """Per-file ratchet over the fail-open reads, plus stale-registry reporting."""
    findings = fail_open_read_violations(sources)
    errors: list[str] = []
    for relpath, hits in sorted(findings.items()):
        allowed = FAIL_OPEN_READ_BASELINE.get(relpath, 0)
        if len(hits) > allowed:
            errors.extend(hits)
            errors.append(
                f"{relpath}: {len(hits)} fail-open read(s), baseline {allowed}: "
                f"{len(hits) - allowed} new one(s)."
            )
    for relpath, allowed in sorted(FAIL_OPEN_READ_BASELINE.items()):
        actual = len(findings.get(relpath, ()))
        if actual < allowed:
            errors.append(
                f"{relpath}: {actual} fail-open read(s), below its baseline of {allowed}. "
                f"Lower the baseline to {actual} so the ground gained is held."
            )
    for relpath in sorted(FAIL_OPEN_READ_EXEMPTIONS):
        if relpath not in sources or not FAIL_OPEN_READ.search(sources[relpath]):
            errors.append(
                f"FAIL_OPEN_READ_EXEMPTIONS names {relpath}, which no longer defaults a "
                "fallible read; drop the stale exemption"
            )
    return errors


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

    degrading = {"a.rs": (
        "pub trait Repo {\n"
        "    async fn write(&self, p: &str) -> R;\n"
        "    async fn write_as(&self, p: &str, actor: A) -> R {\n"
        "        let _ = actor;\n"
        "        self.write(p).await\n"
        "    }\n"
        "}\n"
    )}
    assert degrading_default_violations(degrading), "C1/E2"

    fail_closed = {"a.rs": (
        "pub trait Repo {\n"
        "    async fn write(&self, p: &str) -> R;\n"
        "    async fn write_as(&self, p: &str, actor: A) -> R {\n"
        "        Err(Error::new(\"unimplemented\"))\n"
        "    }\n"
        "}\n"
    )}
    assert not degrading_default_violations(fail_closed), "C2/E1"

    required = {"a.rs": "pub trait Repo {\n    async fn write_as(&self, p: &str, actor: A) -> R;\n}\n"}
    assert not degrading_default_violations(required), "C3/E1"

    DEGRADING_DEFAULT_EXEMPTIONS = {"Repo::write_as": "test"}
    assert not degrading_default_violations(degrading), "C4/E1"
    assert degrading_default_violations(required), "C5/E2"
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

    # Both ratchets must carry an owner, an argued reason and a live expiry —
    # `check_fabricated_facts.py` enforces the same rule from outside.
    import datetime as _dt

    for name, record in (
        ("DEGRADING_DEFAULT_RATCHET", DEGRADING_DEFAULT_RATCHET),
        ("FAIL_OPEN_READ_RATCHET", FAIL_OPEN_READ_RATCHET),
    ):
        assert record["owner"].strip(), f"{name} owner"
        assert record["reason"].strip(), f"{name} reason"
        assert _dt.date.fromisoformat(record["expires"]) >= _dt.date.today(), (
            f"{name} has expired: convert the remainder or re-argue and re-date it"
        )

    print("check_degrading_defaults selftest: both decision tables hold")


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


if __name__ == "__main__":
    if parse(sys.argv[1:]).self_test:
        selftest()
        raise SystemExit(0)
    sources = _sources(ROOT)
    fail_open = fail_open_read_errors(sources)
    if fail_open:
        for problem in fail_open:
            print(f"ERROR: {problem}", file=sys.stderr)
        raise SystemExit(1)
    problems = degrading_default_violations(sources)
    count = len(problems)
    if count > UNTRIAGED_BASELINE:
        for problem in problems:
            print(f"ERROR: {problem}", file=sys.stderr)
        print(
            f"\n{count} degrading defaults, baseline {UNTRIAGED_BASELINE}: "
            f"{count - UNTRIAGED_BASELINE} new one(s). Require the method, make the "
            "default fail closed, or register the decision.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    if count < UNTRIAGED_BASELINE:
        print(
            f"{count} degrading defaults, below the baseline of {UNTRIAGED_BASELINE}. "
            f"Lower UNTRIAGED_BASELINE to {count} so the ground gained is held.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    held = sum(FAIL_OPEN_READ_BASELINE.values())
    print(
        f"OK - degrading defaults held at the baseline of {UNTRIAGED_BASELINE} "
        f"(owner {DEGRADING_DEFAULT_RATCHET['owner']}, expires "
        f"{DEGRADING_DEFAULT_RATCHET['expires']}); fail-open reads held at {held} "
        f"(owner {FAIL_OPEN_READ_RATCHET['owner']}, expires "
        f"{FAIL_OPEN_READ_RATCHET['expires']})."
    )
