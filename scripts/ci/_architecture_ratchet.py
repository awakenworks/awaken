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
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Callable
from dataclasses import dataclass
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


def compare(rules: tuple[Rule, ...], counts: Counts, places: Places, baseline: dict, baseline_name: str) -> list[str]:
    errors: list[str] = []
    for rule in rules:
        now, held = counts.get(rule.name, {}), baseline.get(rule.name, {})
        for key in sorted(set(now) | set(held)):
            current, was = now.get(key, 0), held.get(key, 0)
            if current > was:
                where = ", ".join(places.get(rule.name, {}).get(key, [])[:5])
                errors.append(f"{rule.name}: {key} is at {current}, baseline {was}" + (f" ({where})" if where else "")
                              + f"; {rule.advice}")
            elif current <= was - max(rule.slack, 1):
                errors.append(f"{rule.name}: {key} is at {current}, below the baseline {was}; lower {baseline_name} "
                              "in this commit with --write-baseline")
    return errors


def growth(rules: tuple[Rule, ...], counts: Counts, baseline: dict) -> list[str]:
    """Recorded numbers that would rise; a rule the baseline does not hold yet is its first write."""
    return [
        f"{rule.name}: {key} {baseline[rule.name].get(key, 0)} -> {count}"
        for rule in rules if rule.name in baseline
        for key, count in sorted(counts.get(rule.name, {}).items())
        if count > baseline[rule.name].get(key, 0)
    ]


def growth_reason(earlier: str | None, reason: str) -> str:
    """Keep the earlier review reason when a held allowance grows again."""
    return reason if not earlier else earlier if reason in earlier else f"{earlier}; then: {reason}"


def render(rules: tuple[Rule, ...], counts: Counts, previous: dict, reason: str | None, comment: str) -> dict:
    document: dict = {**previous, "$comment": comment}
    document.pop("growth_reasons", None)
    owned = {rule.name for rule in rules}
    reasons: dict[str, dict[str, str]] = {name: value for name, value in previous.get("growth_reasons", {}).items()
                                       if name not in owned}
    for rule in rules:
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
) -> int:
    """One CLI and write boundary; each guard retains its measurement and rules.

    A tighten candidate must pass the guard's own checker: a clamp or set
    intersection alone can hide growth. Explicit writes may change recorded
    allowances, but their candidate must still pass every hard rule.
    """
    parser = argparse.ArgumentParser(prog=prog, description=description, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--self-test", action="store_true", help="run the decision table")
    modes.add_argument("--check", action="store_true", help="check without writing (the default)")
    modes.add_argument("--tighten", action="store_true", help="record decreases only in an existing baseline")
    modes.add_argument("--write-baseline", action="store_true", help="record the current tree (growth needs --reason)")
    parser.add_argument("--reason", help="with --write-baseline: why a recorded allowance may grow")
    if report:
        (modes if report_only else parser).add_argument("--report", action="store_true", help="print the measurements")
    if stage:
        parser.add_argument("--stage", action="store_true", help="with --tighten: git-add the lowered baseline")
    args = parser.parse_args(argv)
    if args.reason is not None and (not args.write_baseline or not args.reason.strip()):
        print("usage: --reason must be nonempty and only applies to --write-baseline", file=sys.stderr)
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
    if not exists and not args.write_baseline and not (report_only and args.report):
        print("ERROR: an existing baseline is required; initialize it with --write-baseline", file=sys.stderr)
        return 1
    previous = json.loads(baseline_path.read_text(encoding="utf-8")) if exists else {}
    errors, measured = measure()
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
                errors.append("Use --write-baseline --reason '<why this must grow>' to record the growth.")
        else:
            errors = check(measured, previous)
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if errors:
        return 1
    if args.tighten or args.write_baseline:
        if document != previous or not exists:
            baseline_path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
            print(f"wrote {baseline_path.name}")
            if args.tighten and getattr(args, "stage", False):
                stage()
        else:
            print(f"{baseline_path.name} is already tight")
    else:
        print(f"OK - {prog} ratchets hold.")
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
    hard_rules: Callable[[], list[str]] | None = None,
) -> int:
    """Count-shaped guards adapt their existing comparison to the shared CLI."""
    comment = (f"Shrink-only ratchet of scripts/ci/{prog} (ADR-0091). Numbers may only fall; write it with "
               f"`python3 scripts/ci/{prog} --write-baseline`; raising one needs --reason, kept in growth_reasons.")

    def measured():
        counts, places = measure()
        return (hard_rules() if hard_rules else []), (counts, places)

    def lower(current, previous):
        counts, _ = current
        # A newly measured key/rule holds zero in shrink-only mode. Keep its
        # old allowance if it grows, so the original checker refuses it.
        held = {rule.name: previous.get(rule.name, {}) for rule in rules}
        limited = {rule.name: {key: min(value, held[rule.name].get(key, 0))
                              for key, value in counts.get(rule.name, {}).items()} for rule in rules}
        return render(rules, limited, previous, None, previous.get("$comment", comment))

    return baseline_cli(argv, prog=prog, description=description, baseline_path=baseline_path,
                        self_test=self_test, measure=measured,
                        check=lambda current, previous: compare(rules, *current, previous, baseline_path.name),
                        tighten=lower,
                        write=lambda current, previous, reason: (
                            render(rules, current[0], previous, reason, comment), growth(rules, current[0], previous)))


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
                                  hard_rules=lambda: list(hard))
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
    for argv in (["--tighten"], ["--write-baseline", "--reason", "reviewed"]):
        status, before, after, _ = invoke(argv, {"r": {"a": 1}}, hard=("hard failure",))
        assert status == 1 and before == after, "C6 hard rules also forbid reasoned writes"
    status, _, after, _ = invoke(["--write-baseline"], equal, None)
    assert status == 0 and json.loads(after)["r"] == {"a": 2}, "C7 first write retained"
    status, before, after, _ = invoke(["--write-baseline"], {"r": {"a": 3}})
    assert status == 1 and before == after, "C7 reason required for growth"
    status, _, after, _ = invoke(["--write-baseline", "--reason", "necessary"], {"r": {"a": 3}})
    assert status == 0 and json.loads(after)["growth_reasons"]["r"]["a"] == "necessary", "C7 reason"
    for argv in (["--check", "--tighten"], ["--self-test", "--write-baseline"],
                 ["--tighten", "--reason", "growth"], ["--check", "--reason", "growth"],
                 ["--tighten", "--reason", ""], ["--write-baseline", "--reason", ""],
                 ["--write-baseline", "--reason", "   "]):
        status, before, after, observed = invoke(argv, equal)
        assert status == 2 and before == after and not observed, f"C8 {argv}"
    status, before, after, observed = invoke(["--self-test"], equal)
    assert status == 0 and before == after and not observed, "C9 self-test only"


def selftest() -> None:
    """Decision table.

    | Rule | cause                                                     | effect |
    | B1   | count above the baseline, or a key the baseline lacks     | error naming where and the advice |
    | B2   | count below the baseline (no slack)                       | error: lower the baseline |
    | B3   | equal                                                     | accept |
    | B4   | slack rule falls less than / at least `slack` below       | accept / error (lower) |
    | B5   | render: growth without a reason / with one / repeated reason / a fall | reported / history kept / history retained / no reason needed |
    | B6   | a rule the baseline does not hold yet (new rule)          | first write: no growth, no reason kept |
    """
    rules = (Rule("r", "fix it"), Rule("lines", "move it", slack=10))
    held = {"r": {"a": 2}, "lines": {"c": 100}}
    places: Places = {"r": {"b": ["x.rs:3"]}}
    assert compare(rules, {"r": {"a": 2}, "lines": {"c": 100}}, places, held, "b.json") == [], "B3"
    grown = compare(rules, {"r": {"a": 2, "b": 1}, "lines": {"c": 100}}, places, held, "b.json")
    assert len(grown) == 1 and "b is at 1, baseline 0 (x.rs:3); fix it" in grown[0], f"B1 {grown}"
    assert "baseline 2; fix it" in compare(rules, {"r": {"a": 3}, "lines": {"c": 100}}, {}, held, "b.json")[0], "B1"
    assert "lower b.json" in compare(rules, {"r": {"a": 1}, "lines": {"c": 100}}, {}, held, "b.json")[0], "B2"
    assert "lower b.json" in compare(rules, {"lines": {"c": 100}}, {}, held, "b.json")[0], "B2 gone"
    assert compare(rules, {"r": {"a": 2}, "lines": {"c": 91}}, {}, held, "b.json") == [], "B4 float"
    assert "lower" in compare(rules, {"r": {"a": 2}, "lines": {"c": 90}}, {}, held, "b.json")[0], "B4 fall"
    assert "baseline 100" in compare(rules, {"r": {"a": 2}, "lines": {"c": 101}}, {}, held, "b.json")[0], "B4 grow"
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
    assert "baseline 0" in compare(added, {"r": {"a": 2}, "lines": {"c": 100}, "new": {"k": 5}}, {}, held,
                                   "b.json")[0], "B6 check before the first write"
    _selftest_cli()
    print("_architecture_ratchet selftest: the decision table holds")
