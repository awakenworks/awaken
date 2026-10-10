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


def render(rules: tuple[Rule, ...], counts: Counts, previous: dict, reason: str | None, comment: str) -> dict:
    document: dict = {"$comment": comment}
    reasons: dict[str, dict[str, str]] = {}
    for rule in rules:
        current = {key: count for key, count in sorted(counts.get(rule.name, {}).items()) if count}
        document[rule.name] = current
        kept = {key: text for key, text in previous.get("growth_reasons", {}).get(rule.name, {}).items()
                if key in current}
        for key, count in current.items():
            if reason and rule.name in previous and count > previous[rule.name].get(key, 0):
                earlier = kept.get(key)
                kept[key] = reason if not earlier or reason in earlier else f"{earlier}; then: {reason}"
        if kept:
            reasons[rule.name] = dict(sorted(kept.items()))
    if reasons:
        document["growth_reasons"] = reasons
    return document


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
    parser = argparse.ArgumentParser(prog=prog, description=description, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    parser.add_argument("--write-baseline", action="store_true", help="record the current tree (never raises a "
                        "number without --reason)")
    parser.add_argument("--reason", help="with --write-baseline: why a recorded number is allowed to grow")
    args = parser.parse_args(argv)
    if args.self_test:
        self_test()
        return 0
    if args.reason and not args.write_baseline:
        print("usage: --reason only applies to --write-baseline", file=sys.stderr)
        return 2
    counts, places = measure()
    exists = baseline_path.exists()
    previous = json.loads(baseline_path.read_text(encoding="utf-8")) if exists else {}
    if args.write_baseline:
        grown = growth(rules, counts, previous)
        if exists and grown and not args.reason:
            for item in grown:
                print(f"ERROR: baseline would grow: {item}", file=sys.stderr)
            print("Pass --reason '<why this must grow>' to record the growth.", file=sys.stderr)
            return 1
        comment = (f"Shrink-only ratchet of scripts/ci/{prog} (ADR-0091). Numbers may only fall; write it with "
                   f"`python3 scripts/ci/{prog} --write-baseline`; raising one needs --reason, kept in growth_reasons.")
        document = render(rules, counts, previous, args.reason, comment)
        baseline_path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {baseline_path.name}")
        return 0
    errors = (hard_rules() if hard_rules else []) + compare(rules, counts, places, previous, baseline_path.name)
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if not errors:
        print(f"OK - {prog} ratchets hold.")
    return 1 if errors else 0


def selftest() -> None:
    """Decision table.

    | Rule | cause                                                     | effect |
    | B1   | count above the baseline, or a key the baseline lacks     | error naming where and the advice |
    | B2   | count below the baseline (no slack)                       | error: lower the baseline |
    | B3   | equal                                                     | accept |
    | B4   | slack rule falls less than / at least `slack` below       | accept / error (lower) |
    | B5   | render: growth without a reason / with one / a fall       | reported / reason kept / no reason needed |
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
    assert "growth_reasons" not in render(rules, {"r": {"a": 1}}, held, None, "c"), "B5 fall"
    assert growth(rules, {"r": {"a": 2}, "new": {"k": 5}}, held) == [], "B6 first write"
    added = rules + (Rule("new", "fix"),)
    first = render(added, {"r": {"a": 2}, "new": {"k": 5}}, held, "unused", "c")
    assert first["new"] == {"k": 5} and "growth_reasons" not in first, f"B6 render {first}"
    assert "baseline 0" in compare(added, {"r": {"a": 2}, "lines": {"c": 100}, "new": {"k": 5}}, {}, held,
                                   "b.json")[0], "B6 check before the first write"
    print("_architecture_ratchet selftest: the decision table holds")
