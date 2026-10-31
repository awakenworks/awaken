"""Shared plumbing of the ADR-0091 ratchets: the production corpus and a shrink-only baseline.

The single-owner, contract-map and naming ratchets each measure a set of
counts over the same corpus (every crate's production modules, from its Cargo
targets, with `cfg(test)` items and comments blanked): per rule, a key (a crate,
a type name, ``Trait::method``) and how often it occurs. This module holds the
corpus and the one comparison they share:

* a count above its baseline (a key the baseline does not know counts 0) is an
  error naming where it occurs and how to remove it;
* a count below its baseline is an error until the baseline is lowered in the
  same commit, so the file records the real state. A rule with ``slack`` (a line
  count) may float below its baseline and must be re-recorded only once it falls
  ``slack`` or more below it;
* no mode, or ``--check``, checks the tree without changing the baseline;
* ``--tighten`` records a smaller tree against an existing baseline, refuses
  every increase (including a newly measured rule), and checks hard rules first;
* ``--write-baseline`` records the current tree. It refuses to raise a number
  unless ``--reason`` says why, and keeps that reason next to the number. A
  missing baseline file, or a rule the file does not hold yet (a rule added to
  the module), is recorded as it stands (the first write).

A ``hard`` rule is a plain deny (plan v5 S2): a ratchet that reached zero, or a
rule that never had debt. The baseline holds no record for it, every occurrence
is an error, and no mode writes one.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Callable
from dataclasses import dataclass
from datetime import date
from functools import cached_property, lru_cache
from pathlib import Path
from typing import TypeVar

import _crate_boundary_workspace

# rule -> key -> count
Counts = dict[str, dict[str, int]]
# rule -> key -> where it occurs (`path:line`), for error messages
Places = dict[str, dict[str, list[str]]]


@dataclass(frozen=True)
class Rule:
    name: str
    advice: str
    slack: int = 0
    hard: bool = False


@dataclass(frozen=True)
class CrateSource:
    name: str
    metadata: dict
    directory: Path
    # production module file -> its production projection (literals kept)
    files: dict[Path, str]

    @cached_property
    def syntax(self) -> dict[Path, str]:
        """Each file with literals blanked too, so only code is matched."""
        return {path: _crate_boundary_workspace.rust_syntax(text) for path, text in self.files.items()}


@lru_cache(maxsize=4)
def crate_sources(crates: Path) -> tuple[CrateSource, ...]:
    found = []
    for manifest_path in _crate_boundary_workspace.crate_manifests(crates):
        manifest = _crate_boundary_workspace.load_manifest(manifest_path)
        files = {
            Path(path): _crate_boundary_workspace.production_rust(Path(path).read_text(encoding="utf-8"))
            for path in sorted(_crate_boundary_workspace.production_module_files([manifest_path]))
        }
        found.append(CrateSource(_crate_boundary_workspace.package_name(manifest),
                                 _crate_boundary_workspace._awaken_metadata(manifest), manifest_path.parent, files))
    return tuple(found)


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def diagnostic(rule: str, key: str, before: object, after: object, fix: str, *, details: str = "") -> str:
    """Present an already-decided failure; this function makes no rule decision."""
    return (f"{rule}: {key} {before} -> {after}" + (f" ({details})" if details else "")
            + f"; fix: {fix}")


def compare(rules: tuple[Rule, ...], counts: Counts, places: Places, baseline: dict, baseline_name: str,
            *, lower_advice: str | None = None, full_details: bool = False) -> list[str]:
    errors: list[str] = []
    for rule in rules:
        if rule.hard and rule.name in baseline:
            errors.append(diagnostic(rule.name, baseline_name, "holds a record", "none",
                                     f"remove the {rule.name!r} key; a hard rule has no allowance"))
        now, held = counts.get(rule.name, {}), ({} if rule.hard else baseline.get(rule.name, {}))
        for key in sorted(set(now) | set(held)):
            current, was = now.get(key, 0), held.get(key, 0)
            if current > was:
                details = places.get(rule.name, {}).get(key, [])
                if full_details:
                    errors.extend(details)
                where = ", ".join(details[:5]) if not full_details else ""
                errors.append(diagnostic(rule.name, key, was, current, rule.advice, details=where))
            elif (current <= was - max(rule.slack, 1)
                  or (rule.slack == 0 and current < was)):
                advice = lower_advice or f"lower {baseline_name} in this commit with its owning guard's --tighten"
                errors.append(diagnostic(rule.name, key, was, current, advice))
    return errors


def compare_registered(
    found: list[tuple[str, str]],
    exemptions: dict[str, str],
    baseline: int,
    registry: str,
    *,
    rule: Rule,
    baseline_name: str,
) -> list[str]:
    """Adapt existing registered findings to the one count comparison.

    A stale decision is a hard error, separate from the measured live debt.
    Callers keep their scanner, exact exemption keys, inline budget and advice;
    this adapter neither writes a record nor authorizes another exemption.
    """
    seen = {key for key, _ in found}
    live = sorted(message for key, message in found if key not in exemptions)
    hard = [diagnostic(rule.name, key, "registered", "missing", f"remove its stale decision from {registry}",
                       details=f"{registry} names {key}, which no longer holds")
            for key in sorted(exemptions) if key not in seen]
    return hard + compare(
        (rule,), {rule.name: {"workspace": len(live)}}, {rule.name: {"workspace": live}},
        {rule.name: {"workspace": baseline}}, baseline_name,
        lower_advice=f"lower {baseline_name} to {len(live)} in the commit that removed the debt",
        full_details=True,
    )


def growth(rules: tuple[Rule, ...], counts: Counts, baseline: dict) -> list[str]:
    """Recorded numbers that would rise; a rule the baseline does not hold yet is its first write."""
    return [
        f"{rule.name}: {key} {baseline[rule.name].get(key, 0)} -> {count}"
        for rule in rules if rule.name in baseline and not rule.hard
        for key, count in sorted(counts.get(rule.name, {}).items())
        if count > baseline[rule.name].get(key, 0)
    ]


def growth_reason(earlier: str | None, reason: str) -> str:
    """Keep the earlier review reason when a held allowance grows again."""
    return reason if not earlier else earlier if reason in earlier else f"{earlier}; then: {reason}"


def validate_growth_reason(reason: str) -> None:
    """Validate a new D10 review record; historical reasons remain opaque.

    This is record admission, not approval authentication. The existing writer
    retains the exact supplied text and its history; no second record is made.
    """
    def unique_fields(pairs):
        record = {}
        for key, value in pairs:
            if key in record:
                raise ValueError(f"duplicate review field: {key}")
            record[key] = value
        return record

    try:
        record = json.loads(reason, object_pairs_hook=unique_fields)
        fields = {"reason", "approved_by", "date", "remove_when"}
        if (not isinstance(record, dict) or set(record) != fields
                or any(not isinstance(value, str) or not value.strip() for value in record.values())):
            raise ValueError("four nonempty review fields are required")
        if date.fromisoformat(record["date"]).isoformat() != record["date"]:
            raise ValueError("date must be YYYY-MM-DD")
    except (ValueError, TypeError) as error:
        raise ValueError("--reason must be a JSON object with nonempty string fields "
                         "reason, approved_by, date (YYYY-MM-DD), and remove_when") from error


def render(rules: tuple[Rule, ...], counts: Counts, previous: dict, reason: str | None, comment: str) -> dict:
    document: dict = {**previous, "$comment": comment}
    document.pop("growth_reasons", None)
    owned = {rule.name for rule in rules}
    reasons: dict[str, dict[str, str]] = {name: value for name, value in previous.get("growth_reasons", {}).items()
                                       if name not in owned}
    for rule in rules:
        if rule.hard:
            document.pop(rule.name, None)
            reasons.pop(rule.name, None)
            continue
        current = {key: count for key, count in sorted(counts.get(rule.name, {}).items()) if count}
        document[rule.name] = current
        kept = {key: text for key, text in previous.get("growth_reasons", {}).get(rule.name, {}).items()
                if key in current}
        for key, count in current.items():
            if reason and rule.name in previous and count > previous[rule.name].get(key, 0):
                earlier = kept.get(key)
                kept[key] = growth_reason(earlier, reason)
        if kept:
            reasons[rule.name] = dict(sorted(kept.items()))
    if reasons:
        document["growth_reasons"] = reasons
    return document


Measured = TypeVar("Measured")


def baseline_cli(
    argv: list[str] | None,
    *,
    prog: str,
    description: str,
    baseline_path: Path,
    self_test: Callable[[], None],
    measure: Callable[[], tuple[list[str], Measured]],
    check: Callable[[Measured, dict], list[str]],
    tighten: Callable[[Measured, dict], dict],
    write: Callable[[Measured, dict, str | None], tuple[dict, list[str]]],
    report: Callable[[Measured], str] | None = None,
    report_only: bool = False,
    is_initialization: Callable[[dict], bool] | None = None,
    stage: Callable[[], None] | None = None,
    hard_rules: Callable[[dict], list[str]] | None = None,
    decode: Callable[[str], dict] = json.loads,
    encode: Callable[[dict], str] | None = None,
    empty_baseline: Callable[[], dict] = dict,
    allow_missing_check: bool = False,
    check_message: Callable[[Measured, dict], str] | None = None,
) -> int:
    """One CLI and write boundary; each guard retains its measurement and rules.

    A tighten candidate must pass the guard's own checker: a clamp or set
    intersection alone can hide growth. Explicit writes may change recorded
    allowances, but their candidate must still pass every hard rule.
    Baseline-dependent hard admission uses this one original snapshot before
    any candidate is built; a candidate cannot grandfather its own new debt.
    Owners may supply a pure record codec and their verified absent-check
    policy; this remains the only file IO boundary. Tighter ceilings fall,
    while a coverage floor rises. JSON defaults and reason admission stay here.
    """
    parser = argparse.ArgumentParser(prog=prog, description=description, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--self-test", action="store_true", help="run the decision table")
    modes.add_argument("--check", action="store_true", help="check without writing (the default)")
    modes.add_argument("--tighten", action="store_true", help="record tighter bounds in an existing baseline")
    modes.add_argument("--write-baseline", action="store_true", help="record the current tree (growth needs --reason)")
    parser.add_argument("--reason", help="with --write-baseline: JSON review record with reason, approved_by, "
                        "date (YYYY-MM-DD), and remove_when")
    if report:
        (modes if report_only else parser).add_argument("--report", action="store_true", help="print the measurements")
    if stage:
        parser.add_argument("--stage", action="store_true", help="with --tighten: git-add the lowered baseline")
    args = parser.parse_args(argv)
    if args.reason is not None and (not args.write_baseline or not args.reason.strip()):
        print("usage: --reason must be nonempty and only applies to --write-baseline", file=sys.stderr)
        return 2
    if args.reason is not None:
        try:
            validate_growth_reason(args.reason)
        except ValueError as error:
            print(f"usage: {error}", file=sys.stderr)
            return 2
    if getattr(args, "stage", False) and not args.tighten:
        print("usage: --stage only applies to --tighten", file=sys.stderr)
        return 2
    if args.self_test and getattr(args, "report", False):
        print("usage: --report cannot accompany --self-test", file=sys.stderr)
        return 2
    if args.self_test:
        self_test()
        return 0
    exists = baseline_path.exists()
    if (not exists and not args.write_baseline and not (report_only and args.report)
            and not (allow_missing_check and not args.tighten)):
        print("ERROR: an existing baseline is required; initialize it with --write-baseline", file=sys.stderr)
        return 1
    previous = decode(baseline_path.read_text(encoding="utf-8")) if exists else empty_baseline()
    errors, measured = measure()
    if hard_rules:
        errors += hard_rules(previous)
    if not errors and report and args.report:
        print(report(measured))
        if report_only:
            return 0
    document = previous
    if not errors:
        if args.tighten:
            document = tighten(measured, previous)
            errors = check(measured, document)
        elif args.write_baseline:
            document, grown = write(measured, previous, args.reason)
            errors = check(measured, document)
            # A guard may already have an explicit initial-state rule (shape:
            # crates={} counts as its first write). It affects only the reason
            # gate; measurement and candidate hard rules still have to pass.
            initializing = not exists or (is_initialization is not None and is_initialization(previous))
            if grown and not initializing and not args.reason:
                errors += [f"baseline would grow: {item}" for item in grown]
                errors.append("Use --write-baseline --reason '<JSON review: reason, approved_by, date, "
                              "remove_when>' to record the growth.")
        else:
            errors = check(measured, previous)
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if errors:
        return 1
    if args.tighten or args.write_baseline:
        if document != previous or not exists:
            text = encode(document) if encode else json.dumps(document, indent=2) + "\n"
            # The codecs produce canonical LF, including the TXT coverage floor.
            # Preserve held bytes on no-op; write the same format on every host.
            baseline_path.write_text(text, encoding="utf-8", newline="\n")
            print(f"wrote {baseline_path.name}")
            if args.tighten and getattr(args, "stage", False):
                stage()
        else:
            print(f"{baseline_path.name} is already tight")
    else:
        print(check_message(measured, previous) if check_message else f"OK - {prog} ratchets hold.")
    return 0


def main(
    argv: list[str] | None,
    *,
    prog: str,
    description: str,
    rules: tuple[Rule, ...],
    measure: Callable[[], tuple[Counts, Places]],
    baseline_path: Path,
    self_test: Callable[[], None],
    hard_rules: Callable[[dict], list[str]] | None = None,
) -> int:
    """Count-shaped guards adapt their existing comparison to the shared CLI."""
    comment = (f"Shrink-only ratchet of scripts/ci/{prog} (ADR-0091). Numbers may only fall; write it with "
               f"`python3 scripts/ci/{prog} --write-baseline`; raising one needs --reason, kept in growth_reasons.")

    def measured():
        counts, places = measure()
        return [], (counts, places)

    def lower(current, previous):
        counts, _ = current
        # A newly measured key/rule holds zero in shrink-only mode. Keep its
        # old allowance if it grows, so the original checker refuses it.
        held = {rule.name: previous.get(rule.name, {}) for rule in rules}
        limited = {rule.name: {key: min(value, held[rule.name].get(key, 0))
                              for key, value in counts.get(rule.name, {}).items()} for rule in rules}
        return render(rules, limited, previous, None, previous.get("$comment", comment))

    return baseline_cli(argv, prog=prog, description=description, baseline_path=baseline_path,
                        self_test=self_test, measure=measured, hard_rules=hard_rules,
                        check=lambda current, previous: compare(
                            rules, *current, previous, baseline_path.name,
                            lower_advice=f"python3 scripts/ci/{prog} --tighten to lower {baseline_path.name} in this commit"),
                        tighten=lower,
                        write=lambda current, previous, reason: (
                            render(rules, current[0], previous, reason, comment), growth(rules, current[0], previous)))


def _test_growth_reason(reason: str = "reviewed") -> str:
    """Fictional review record shared by the real CLI decision-table fixtures."""
    return json.dumps({"reason": reason, "approved_by": "fixture-reviewer",
                       "date": "2026-10-20", "remove_when": "fixture completes"})


def _cli_fixture(invoke, argv, baseline, measurement_name, measurement, **overrides):
    """Exercise a guard's real main against temporary bytes, never the workspace baseline."""
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch

    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / "baseline.json"
        if baseline is not None:
            path.write_text(json.dumps(baseline) + "\n", encoding="utf-8")
        before = path.read_bytes() if path.exists() else None
        observed = []

        def measure(*_args):
            observed.append("measured")
            return measurement

        stdout = io.StringIO()
        with patch.dict(invoke.__globals__, {"BASELINE": path, measurement_name: measure, **overrides}), \
                contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(io.StringIO()):
            try:
                status = invoke(argv)
            except SystemExit as error:
                status = error.code
        return status, before, path.read_bytes() if path.exists() else None, observed, stdout.getvalue()


def _selftest_cli() -> None:
    """CLI cause/effect table over the existing comparison and writer.

    Causes: mode, baseline presence, equal/lower/higher or newly measured
    counts, hard-rule failure, and whether a growth reason was supplied.
    Constraints: modes are exclusive; reasons belong only to explicit writes.
    Effects: exit status, whether measurement ran, and exact baseline bytes.
    C1 default/check -> read-only; C2 tighten lower -> shrink; C3 equal -> no
    write; C4 growth/new key/new rule -> refuse without changing bytes; C5
    missing baseline -> refuse before measuring or creating it; C6 hard failure -> refuse;
    C7 explicit first write/reasoned growth -> existing writer; C8 invalid
    modes/reasons -> usage before measurement; C9 self-test -> no measurement.
    """
    import contextlib
    import io
    import tempfile

    rules = (Rule("r", "fix"), Rule("new", "fix"))
    held = {"$comment": "fixture", "r": {"a": 2}, "new": {}}

    def invoke(argv, counts, baseline=held, hard=()):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "baseline.json"
            if baseline is not None:
                path.write_text(json.dumps(baseline) + "\n", encoding="utf-8")
            before = path.read_bytes() if path.exists() else None
            observed = []

            def measure():
                observed.append("measured")
                return counts, {}

            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                try:
                    status = main(argv, prog="fixture.py", description="fixture", rules=rules,
                                  measure=measure, baseline_path=path, self_test=lambda: None,
                                  hard_rules=lambda _previous: list(hard))
                except SystemExit as error:
                    status = error.code
            after = path.read_bytes() if path.exists() else None
            return status, before, after, observed

    equal = {"r": {"a": 2}, "new": {}}
    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, observed = invoke(argv, equal)
        assert status == 0 and before == after and observed, f"C1/C3 {argv}"
    status, _, after, _ = invoke(["--tighten"], {"r": {"a": 1}})
    assert status == 0 and json.loads(after)["r"] == {"a": 1}, "C2 shrink"
    for counts, baseline in (({"r": {"a": 3}}, held), ({"r": {"b": 1}}, held),
                             ({"r": {"a": 2}, "new": {"k": 1}}, {"r": {"a": 2}})):
        status, before, after, _ = invoke(["--tighten"], counts, baseline)
        assert status == 1 and before == after, "C4 no growth, new key or first rule"
    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, observed = invoke(argv, equal, None)
        assert status == 1 and before is None and after is None and not observed, "C5 no initialization/measurement"
    for argv in (["--tighten"], ["--write-baseline", "--reason", _test_growth_reason("reviewed")]):
        status, before, after, _ = invoke(argv, {"r": {"a": 1}}, hard=("hard failure",))
        assert status == 1 and before == after, "C6 hard rules also forbid reasoned writes"
    status, _, after, _ = invoke(["--write-baseline"], equal, None)
    assert status == 0 and json.loads(after)["r"] == {"a": 2}, "C7 first write retained"
    status, before, after, _ = invoke(["--write-baseline"], {"r": {"a": 3}})
    assert status == 1 and before == after, "C7 reason required for growth"
    status, _, after, _ = invoke(["--write-baseline", "--reason", _test_growth_reason("necessary")], {"r": {"a": 3}})
    assert status == 0 and json.loads(after)["growth_reasons"]["r"]["a"] == _test_growth_reason("necessary"), "C7 reason"
    for argv in (["--check", "--tighten"], ["--self-test", "--write-baseline"],
                 ["--tighten", "--reason", "growth"], ["--check", "--reason", "growth"],
                 ["--tighten", "--reason", ""], ["--write-baseline", "--reason", ""],
                 ["--write-baseline", "--reason", "   "]):
        status, before, after, observed = invoke(argv, equal)
        assert status == 2 and before == after and not observed, f"C8 {argv}"
    status, before, after, observed = invoke(["--self-test"], equal)
    assert status == 0 and before == after and not observed, "C9 self-test only"

    # C10 cause/effect graph: explicit write + valid four-field review permits
    # the original numeric/hard-rule decision; missing/blank/nonstring fields,
    # duplicate keys, malformed JSON or a noncanonical/invalid date -> usage2
    # before measurement or baseline writes. These fixtures name a fictional
    # reviewer; formatting is not authentication or permission to waive a rule.
    valid = json.loads(_test_growth_reason())
    invalid = ["reviewed", "{", "null", "[]", '"text"',
               _test_growth_reason()[:-1] + ', "approved_by": "other"}',
               json.dumps({**valid, "extra": "not a second record"})]
    for key in valid:
        invalid.append(json.dumps({field: value for field, value in valid.items() if field != key}))
        invalid.extend(json.dumps({**valid, key: value}) for value in ("", " \n ", None, 1, [], {}))
    invalid.extend(json.dumps({**valid, "date": value}) for value in
                   ("2026-02-29", "2026-13-01", "20261020", "2026-W43-2", "2026-10-20T00:00:00Z"))
    for reason in invalid:
        for baseline in (held, None):
            status, before, after, observed = invoke(["--write-baseline", "--reason", reason],
                                                     {"r": {"a": 3}}, baseline)
            assert status == 2 and before == after and not observed, "C10 review admission before IO"
    # C11 valid JSON keeps supplied text and existing historical reasons. A
    # second growth appends through growth_reason; repeats retain that history.
    # Date validation permits a real leap day, and does not consult wall time.
    for record in (valid, {**valid, "reason": "兼容边界迁移", "date": "2024-02-29"}):
        reason = json.dumps(record, ensure_ascii=False, indent=2)
        previous = {**held, "growth_reasons": {"r": {"a": "legacy review text"}}}
        status, _, after, observed = invoke(["--write-baseline", "--reason", reason], {"r": {"a": 3}}, previous)
        expected = "legacy review text; then: " + reason
        document = json.loads(after)
        assert status == 0 and observed and document["growth_reasons"]["r"]["a"] == expected, "C11 exact history"
        status, _, after, _ = invoke(["--write-baseline", "--reason", reason], {"r": {"a": 4}}, document)
        assert status == 0 and json.loads(after)["growth_reasons"]["r"]["a"] == expected, "C11 repeat retained"
        status, before, after, _ = invoke([], {"r": {"a": 4}}, json.loads(after))
        assert status == 0 and before == after, "C11 check does not reclassify historical records"


def _selftest_registered() -> None:
    """A1/A2 registered-count cause/effect graph and decision table.

    Exact key present -> exempt all its occurrences; missing registered key ->
    hard stale error. Independently, actual live debt < / = / > held -> fall
    error / accept / growth error through compare(). Hard errors never consume
    debt budget or cancel a decrease. Registry values are preserved decisions,
    not a new reason/expiry policy. Effects: errors, complete original finding
    details, exact-key scope and unchanged input data. No CLI or writer exists.
    """
    from unittest.mock import patch

    rule = Rule("fixture", "own the effect or review the exact decision")
    found = [("A", "a.rs:1: original advice"), ("A", "a.rs:2: original advice"), ("B", "b.rs:3: original advice")]
    before = list(found)
    # A1 every count relation uses the canonical compare; A2 valid/stale/other
    # keys are crossed with that relation, so a stale decision cannot be hidden
    # at equality or absorbed into an allowance after a real debt disappears.
    for exemptions in ({}, {"A": "why"}, {"missing": "why"}):
        held_inputs = dict(exemptions)
        actual = sum(key not in exemptions for key, _ in found)
        for held in (actual - 1, actual, actual + 1):
            if held < 0:
                continue
            errors = compare_registered(found, exemptions, held, "REG", rule=rule, baseline_name="fixture.py::HELD")
            assert bool(errors) == (held != actual or "missing" in exemptions), "A1/A2 separate hard and debt"
            if "missing" in exemptions:
                assert any("registered -> missing" in error for error in errors), "A2 stale always hard"
            assert found == before and exemptions == held_inputs, "A2 decisions/findings never modified"
            assert not any("--write-baseline" in error for error in errors), "A1 inline owner has no writer"
            if held < actual:
                assert all(message in errors for key, message in found if key not in exemptions), "A1 full advice retained"
            elif held > actual:
                assert any("fixture.py::HELD" in error for error in errors), "A1 real inline source named"
    # A1 prove delegation, not a second comparison hidden in the adapter.
    with patch.dict(compare_registered.__globals__, {"compare": lambda *args, **kwargs: ["canonical compare called"]}):
        assert compare_registered(found, {}, 3, "REG", rule=rule, baseline_name="fixture.py::HELD") == [
            "canonical compare called"
        ], "A1 canonical comparison owns the numeric decision"


def selftest() -> None:
    """Decision table.

    | Rule | cause                                                     | effect |
    | B1   | count above the baseline, or a key the baseline lacks     | error naming where and the advice |
    | B2   | count below the baseline (no slack)                       | error: lower the baseline |
    | B3   | equal                                                     | accept |
    | B4   | slack rule falls less than / at least `slack` below       | accept / error (lower) |
    | B5   | render: growth without a reason / with one / repeated reason / a fall | reported / history kept / history retained / no reason needed |
    | B6   | a rule the baseline does not hold yet (new rule)          | first write: no growth, no reason kept |
    | B7   | hard rule: an occurrence / none / a baseline record for it | error / accept / error; render drops the record |
    """
    rules = (Rule("r", "fix it"), Rule("lines", "move it", slack=10))
    held = {"r": {"a": 2}, "lines": {"c": 100}}
    places: Places = {"r": {"b": ["x.rs:3"]}}
    assert compare(rules, {"r": {"a": 2}, "lines": {"c": 100}}, places, held, "b.json") == [], "B3"
    grown = compare(rules, {"r": {"a": 2, "b": 1}, "lines": {"c": 100}}, places, held, "b.json")
    assert grown == ["r: b 0 -> 1 (x.rs:3); fix: fix it"], f"B1 {grown}"
    assert "r: a 2 -> 3; fix: fix it" in compare(rules, {"r": {"a": 3}, "lines": {"c": 100}}, {}, held, "b.json")[0], "B1"
    assert "lower b.json" in compare(rules, {"r": {"a": 1}, "lines": {"c": 100}}, {}, held, "b.json")[0], "B2"
    assert "lower b.json" in compare(rules, {"lines": {"c": 100}}, {}, held, "b.json")[0], "B2 gone"
    # B2a graph: count relation + zero/nonzero slack -> numeric refusal. The
    # documented no-slack rule rejects every decrease, including parser-readable
    # fractional held values; an integer minimum step used to admit 1 under1.5.
    # The original threshold remains for every slack, including negative slack
    # and parser-readable special numbers; only zero-slack strict decreases are
    # added. No type schema, rounding or alternative comparison is introduced.
    #
    # | Row | slack        | relation                 | effect |
    # | B2a | zero         | lower/equal/higher        | reject/admit/reject |
    # | B2b | positive     | fall below/at threshold   | admit/reject |
    # | B2c | negative     | fall below/at step1       | admit/reject |
    # | B2d | key absent   | original implicit count0  | same zero-key admission |
    # | B2e | special      | equal infinities/NaN      | original reject/admit |
    for slack, was, current, failed in (
        (0, 2, 1, True), (0, 2, 2, False), (0, 2, 3, True),
        (0, 1.5, 1, True), (0, 1.5, 1.5, False), (0, 1.5, 2, True), (0, 2, 1.75, True),
        (2, 2, 1, False), (2, 2, 0, True), (2, 2, 2, False), (2, 2, 3, True),
        (2, 3.5, 2, False), (2, 3.5, 1.5, True), (2, 3.5, 3.5, False), (2, 3.5, 4, True),
        (-2, 2, 1, True), (-2, 2, 2, False), (-2, 2, 3, True),
        (-2, 1.5, 1, False), (-2, 1.5, 0.5, True), (-2, 1.5, 1.5, False), (-2, 1.5, 2, True),
        (0.5, 1.5, 1, False), (0.5, 1.5, 0.5, True),
    ):
        errors = compare((Rule("fraction", "fix", slack=slack),), {"fraction": {"key": current}}, {},
                         {"fraction": {"key": was}}, "fixture", lower_advice="lower the original record")
        assert bool(errors) == failed, ("B2a-B2c", slack, was, current, errors)
    for counts, previous, failed in (({}, {"r": {"key": 0.5}}, True),
                                     ({"r": {"key": 1}}, {}, True),
                                     ({"r": {"key": 0}}, {}, False)):
        assert bool(compare((Rule("r", "fix"),), counts, {}, previous, "fixture")) == failed, "B2d absence"
    # B2e compatibility: json.loads already reads Infinity/-Infinity/NaN and
    # compare has no numeric schema. Equal infinities retain the original
    # threshold refusal; NaN retains its original unordered admission. These
    # cases preserve the old boundary, not a claim that special counts are valid.
    # finite against infinity follows the original growth/fall decisions.
    for was, current, failed in (
        (float("inf"), float("inf"), True),
        (float("-inf"), float("-inf"), True),
        (float("inf"), 1, True), (float("-inf"), 1, True),
        (1, float("inf"), True), (1, float("-inf"), True),
        (float("nan"), 1, False), (1, float("nan"), False),
        (float("nan"), float("nan"), False),
    ):
        assert bool(compare((Rule("special", "fix"),), {"special": {"key": current}}, {},
                            {"special": {"key": was}}, "fixture")) == failed, "B2e original special-number boundary"
    assert compare(rules, {"r": {"a": 2}, "lines": {"c": 91}}, {}, held, "b.json") == [], "B4 float"
    assert "lower" in compare(rules, {"r": {"a": 2}, "lines": {"c": 90}}, {}, held, "b.json")[0], "B4 fall"
    assert "lines: c 100 -> 101" in compare(rules, {"r": {"a": 2}, "lines": {"c": 101}}, {}, held, "b.json")[0], "B4 grow"
    assert growth(rules, {"r": {"a": 3}}, held) == ["r: a 2 -> 3"], "B5 growth"
    document = render(rules, {"r": {"a": 3}, "lines": {"c": 0}}, held, "split pending", "c")
    assert document == {"$comment": "c", "r": {"a": 3}, "lines": {}, "growth_reasons": {"r": {"a": "split pending"}}}, \
        f"B5 reason {document}"
    again = render(rules, {"r": {"a": 4}}, document, "second", "c")
    assert again["growth_reasons"] == {"r": {"a": "split pending; then: second"}}, "B5 history"
    repeated = render(rules, {"r": {"a": 5}}, again, "second", "c")
    assert repeated["growth_reasons"] == again["growth_reasons"], "B5 repeating the reason does not erase history"
    assert "growth_reasons" not in render(rules, {"r": {"a": 1}}, held, None, "c"), "B5 fall"
    assert growth(rules, {"r": {"a": 2}, "new": {"k": 5}}, held) == [], "B6 first write"
    # B5 foreign rule + its counter/reason: both shrink and reasoned growth
    # retain the foreign data. Owning the current rules is not authority to
    # retire a future metric or erase the reason next to it.
    metadata = {**held, "owner": "guard", "expiry": "after-extraction", "future-counter": {"k": 9},
                "growth_reasons": {"future-counter": {"k": "  foreign review; expiry unchanged  "}}}
    assert render(rules, {"r": {"a": 1}}, metadata, None, "c")["owner"] == "guard", "B5 owner retained"
    assert render(rules, {"r": {"a": 1}}, metadata, None, "c")["expiry"] == "after-extraction", "B5 expiry retained"
    for reason, count in ((None, 1), ("reviewed", 3)):
        preserved = render(rules, {"r": {"a": count}}, metadata, reason, "c")
        assert preserved["future-counter"] == metadata["future-counter"], "B5 unknown counter retained"
        assert preserved["growth_reasons"]["future-counter"] == metadata["growth_reasons"]["future-counter"], \
            "B5 unknown reason data stays with its counter, for shrink and explicit writes"
    added = rules + (Rule("new", "fix"),)
    first = render(added, {"r": {"a": 2}, "new": {"k": 5}}, held, "unused", "c")
    assert first["new"] == {"k": 5} and "growth_reasons" not in first, f"B6 render {first}"
    assert "new: k 0 -> 5" in compare(added, {"r": {"a": 2}, "lines": {"c": 100}, "new": {"k": 5}}, {}, held,
                                   "b.json")[0], "B6 check before the first write"
    deny = (Rule("deny", "remove it", hard=True),)
    assert compare(deny, {"deny": {"k": 1}}, {"deny": {"k": ["x.rs:1"]}}, {}, "b.json") == \
        ["deny: k 0 -> 1 (x.rs:1); fix: remove it"], "B7 occurrence"
    assert compare(deny, {"deny": {}}, {}, {}, "b.json") == [], "B7 none"
    assert compare(deny, {}, {}, {"deny": {"k": 1}}, "b.json")[0].startswith("deny: b.json holds a record"), \
        "B7 record refused"
    assert compare(deny, {"deny": {"k": 1}}, {}, {"deny": {"k": 1}}, "b.json")[1].startswith("deny: k 0 -> 1"), \
        "B7 a record grants nothing"
    assert "deny" not in render(deny, {"deny": {"k": 1}}, {"deny": {}}, "why", "c"), "B7 render drops it"
    assert growth(deny, {"deny": {"k": 1}}, {"deny": {}}) == [], "B7 no growth to approve"
    _selftest_cli()
    _selftest_registered()
    _selftest_diagnostics()
    print("_architecture_ratchet selftest: the decision table holds")


def _selftest_diagnostics() -> None:
    """D1/D2: original decision -> formatter, with no edge back to the rule.

    Rule | causes                                      | effects
    D1   | held/current -1/= /+1, key absence, slack    | same failure count, explicit held -> current + fix
    D2   | >5 places / full details / inline owner     | first five / all raw findings; truthful fix, no writer
    Existing B1-B6 and A1/A2 cases own the comparison and hard-stale policy.
    These cases check its output boundary, not a second comparison algorithm.
    """
    rule = Rule("fixture", "remove the offending operation", slack=2)
    held = {"fixture": {"key": 5}}
    locations = [f"src.rs:{line}" for line in range(1, 8)]
    places = {"fixture": {"key": locations}}
    for current, failed in ((6, True), (5, False), (4, False), (3, True), (0, True)):
        errors = compare((rule,), {"fixture": {"key": current}}, places, held, "fixture.json")
        assert bool(errors) == failed, "D1 original slack boundary"
        if errors:
            assert f"fixture: key 5 -> {current}" in errors[0] and "; fix: " in errors[0], "D1 explicit transition"
    errors = compare((rule,), {"fixture": {"key": 6}}, places, held, "fixture.json")
    assert all(where in errors[0] for where in locations[:5]) and all(where not in errors[0] for where in locations[5:]), \
        "D2 first-five locator limit remains"
    errors = compare((rule,), {"fixture": {"key": 6}}, places, held, "fixture.py::HELD",
                     full_details=True, lower_advice="lower fixture.py::HELD")
    assert errors[:-1] == locations and errors[-1] == "fixture: key 5 -> 6; fix: remove the offending operation", \
        "D2 full raw details and summary stay separate"
    errors = compare((rule,), {}, {}, held, "fixture.py::HELD", lower_advice="lower fixture.py::HELD")
    assert errors == ["fixture: key 5 -> 0; fix: lower fixture.py::HELD"], "D2 real inline lower owner"
    assert not any("--write-baseline" in error or "--tighten" in error for error in errors), "D2 NoCLI has no writer"

    # D9 real check_all consumers -> their own lower command. A baseline
    # filename is input data, never a registry from which to guess an owner.
    # These bounded fixtures exercise the actual callbacks used by the
    # boundary gate; the original owners' selftests retain their hard rules.
    import tempfile
    from unittest.mock import patch
    import _naming_fitness
    import _single_owner_fitness
    import _contract_map_fitness

    with tempfile.TemporaryDirectory() as temporary:
        for owner in (_naming_fitness, _single_owner_fitness, _contract_map_fitness):
            rule = owner.RULES[0]
            path = Path(temporary) / "arbitrary-baseline-name.json"
            path.write_text(json.dumps({rule.name: {"key": 2}}))
            current = ({rule.name: {"key": 1}}, {})
            if owner is _contract_map_fitness:
                contracts = set(owner.TARGET_MAP) | {name for _, pending in owner.TARGET_MAP.values() for name in pending}
                callback, result = "_workspace", (*current, contracts, {})
            else:
                callback, result = "_measure_workspace", current
            with patch.object(owner, callback, lambda _crates: result):
                errors = owner.check_all(Path(temporary), path)
            assert len(errors) == 1 and f"python3 scripts/ci/{Path(owner.__file__).name} --tighten" in errors[0], \
                "D9 actual caller supplies its truthful command"
            assert "arbitrary-baseline-name.json" in errors[0], "D9 baseline location remains explicit"
