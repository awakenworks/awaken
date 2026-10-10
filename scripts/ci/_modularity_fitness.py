"""Modules inside a crate stay real boundaries; six ratchets that only shrink.

Plan 9.12 / W22. A module is a boundary only when it owns its state and the
other modules reach it through a narrow surface. Six ways to lose that, each
counted from production code (comments and `cfg(test)` code are not scanned)
and held at or below `modularity_baseline.json` (lower it after a shrink with
`python3 scripts/ci/_modularity_fitness.py --tighten`, which never raises or
adds an entry):

* `impl-spread`: one type's inherent `impl` blocks spread over more than
  `MAX_IMPL_FILES` files, or more than `MAX_IMPL_METHODS` methods in total.
  Such a type is one object cut into files: every file reaches every field and
  method through `self`. Recorded types may only lose files and methods.
* `super-glob`: `use super::*;` hands a child module everything of its parent,
  private items included. Counted per crate.
* `module-cycle`: the largest cycle among a crate's top-level modules, resolved
  through `crate::` and `super::` paths. Counted per crate; a crate without a
  recorded cycle may not grow one. A cycle carried by method calls on a spread
  type is not seen here; `impl-spread` holds that one.
* `unreachable-pub`: a crate root declares `#![warn(unreachable_pub)]` (or
  `deny`), so the compiler marks every `pub` item the crate does not export.
  Crates that have not adopted it yet are listed; the list only shrinks.
* `long-function`: a production function longer than `MAX_FUNCTION_LINES`
  lines (crates of layer `tooling` are not scanned). Today's are recorded by
  `crate::file::function` with their length and may only get shorter; a moved or
  renamed one renames its key in the same commit.
* `crate-budget`: a crate over `CRATE_REVIEW_LINES` production code lines (non-blank,
  comments and `cfg(test)` code excluded, production-compiled files only) must be
  listed with a reason -- the review that it is still one boundary (ADR-0091 D2) or
  the work item that shrinks it. A listed crate may not grow in code lines or in
  workspace dependencies, so new behavior lands in its future home instead; once it
  falls to the threshold or below, its entry is removed. Dependencies are exact (a lower
  count is recorded in the same commit); lines may float below the budget and
  must be re-recorded once they fall `BUDGET_LINE_SLACK` or more below it.

A count above its baseline is an error; a count below it is an error until the
baseline is lowered in the same commit, so the file records the real state.
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter, defaultdict
from collections.abc import Iterable
from pathlib import Path

import _crate_boundary_workspace
from _context_map_fitness import _flatten_path_tree

BASELINE = Path(__file__).with_name("modularity_baseline.json")
MAX_IMPL_FILES = 5
MAX_IMPL_METHODS = 60
MAX_FUNCTION_LINES = 150
BUDGET_LINE_SLACK = 1000
CRATE_REVIEW_LINES = 15000

# An inherent impl may name its type through a crate path (`impl crate::host::SharedHost`);
# that block belongs to the same type, so the path prefix is part of the match.
_IMPL = re.compile(
    r"^\s*impl(?:<[^>{]*>)?\s+(?:crate::(?:[a-z_][a-z0-9_]*::)*)?([A-Z][A-Za-z0-9_]*)(?:<[^>{]*>)?\s*(?:where[^{]*)?\{",
    re.M,
)
_FN = re.compile(r"\bfn\s+[A-Za-z_]\w*")
_FN_ITEM = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", re.M)
_SUPER_GLOB = re.compile(r"^\s*use\s+super::\*\s*;", re.M)
_PATH_ROOT = re.compile(r"\b(crate|super)\s*::")
_UNREACHABLE_PUB = re.compile(r"#!\[\s*(?:warn|deny|forbid)\s*\([^)]*\bunreachable_pub\b")


def _block_end(source: str, opening: int) -> int:
    depth = 0
    for index in range(opening, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    return len(source)


def impl_spread(files: dict[str, str]) -> dict[str, tuple[int, int]]:
    """Per type: (files holding an inherent impl, methods across those impls)."""
    holders: dict[str, set[str]] = defaultdict(set)
    methods: Counter[str] = Counter()
    for name, source in files.items():
        for match in _IMPL.finditer(source):
            opening = source.index("{", match.start())
            holders[match.group(1)].add(name)
            methods[match.group(1)] += len(_FN.findall(source, opening, _block_end(source, opening)))
    return {ty: (len(holders[ty]), methods[ty]) for ty in holders}


def long_functions(files: dict[str, str]) -> dict[str, int]:
    """Per `file::function`: its length in lines, when above `MAX_FUNCTION_LINES` (the longest of a repeated name)."""
    found: dict[str, int] = {}
    for relative, text in files.items():
        # Blank string and character literals first: a `{` inside a literal
        # would otherwise leave the body unbalanced and the function unmeasured.
        source = _crate_boundary_workspace.rust_syntax(text)
        for match in _FN_ITEM.finditer(source):
            end = _crate_boundary_workspace._rust_item_end(source, match.start())
            if end is None:
                continue
            lines = source.count("\n", match.start(), end)
            key = f"{relative}::{match.group(1)}"
            if lines > MAX_FUNCTION_LINES and lines > found.get(key, 0):
                found[key] = lines
    return found


def super_globs(files: dict[str, str]) -> int:
    return sum(len(_SUPER_GLOB.findall(source)) for source in files.values())


def _module_of(relative: str) -> list[str]:
    parts = relative.removesuffix(".rs").split("/")
    return parts[:-1] if parts[-1] in {"mod", "lib", "main"} else parts


def largest_module_cycle(files: dict[str, str], top_modules: set[str]) -> int:
    """Largest strongly connected set of top-level modules, through `crate::` / `super::` paths."""
    edges: dict[str, set[str]] = defaultdict(set)
    for relative, source in files.items():
        here = _module_of(relative)
        if not here:
            continue
        for match in _PATH_ROOT.finditer(source):
            for path in _flatten_path_tree(source, match.end()):
                if match.group(1) == "crate":
                    target = list(path)
                else:
                    base, rest = here[:-1], list(path)
                    while rest and rest[0] == "super":
                        base, rest = base[:-1], rest[1:]
                    target = base + rest
                target = [part for part in target if part not in {"self", "*"}]
                if target and target[0] in top_modules and target[0] != here[0]:
                    edges[here[0]].add(target[0])
    return max((len(component) for component in _components(top_modules, edges)), default=0)


def _components(nodes: Iterable[str], edges: dict[str, set[str]]) -> list[list[str]]:
    index: dict[str, int] = {}
    low: dict[str, int] = {}
    stack: list[str] = []
    on_stack: set[str] = set()
    found: list[list[str]] = []
    limit = sys.getrecursionlimit()
    sys.setrecursionlimit(max(limit, 10_000))

    def visit(node: str) -> None:
        index[node] = low[node] = len(index)
        stack.append(node)
        on_stack.add(node)
        for target in edges[node]:
            if target not in index:
                visit(target)
                low[node] = min(low[node], low[target])
            elif target in on_stack:
                low[node] = min(low[node], index[target])
        if low[node] == index[node]:
            component = []
            while True:
                member = stack.pop()
                on_stack.discard(member)
                component.append(member)
                if member == node:
                    break
            if len(component) > 1:
                found.append(component)

    try:
        for node in sorted(nodes):
            if node not in index:
                visit(node)
    finally:
        sys.setrecursionlimit(limit)
    return found


def crate_measures(manifest_path: Path) -> dict:
    root = manifest_path.parent / "src"
    files: dict[str, str] = {}
    for path in _crate_boundary_workspace.production_module_files([manifest_path]):
        path = Path(path)
        if root in path.parents:
            relative = path.relative_to(root).as_posix()
            files[relative] = _crate_boundary_workspace.production_rust(path.read_text(encoding="utf-8"))
    top = {_module_of(relative)[0] for relative in files if _module_of(relative)}
    crate_root = next((root / name for name in ("lib.rs", "main.rs") if (root / name).exists()), None)
    adopted = bool(crate_root and _UNREACHABLE_PUB.search(crate_root.read_text(encoding="utf-8")))
    layer = _crate_boundary_workspace.load_manifest(manifest_path).get("package", {}).get("metadata", {}).get(
        "awaken", {}).get("layer", "")
    return {
        "lines": sum(1 for source in files.values() for line in source.splitlines() if line.strip()),
        "deps": _crate_boundary_workspace.normal_dependency_names(
            _crate_boundary_workspace.load_manifest(manifest_path)),
        "long-function": {} if layer == "tooling" else long_functions(files),
        "impl-spread": impl_spread(files),
        "super-glob": super_globs(files),
        "module-cycle": largest_module_cycle(files, top),
        "unreachable-pub": adopted,
    }


def modularity_errors(measures: dict[str, dict], baseline: dict) -> list[str]:
    errors: list[str] = []
    spread_held: dict[str, dict] = baseline.get("impl-spread", {})
    seen_spread: set[str] = set()
    for crate, measure in sorted(measures.items()):
        for ty, (files, methods) in sorted(measure["impl-spread"].items()):
            key = f"{crate}::{ty}"
            held = spread_held.get(key)
            if held is None:
                if files > MAX_IMPL_FILES or methods > MAX_IMPL_METHODS:
                    errors.append(
                        f"impl-spread: {key} has inherent impls in {files} files with {methods} methods "
                        f"(limit {MAX_IMPL_FILES} files, {MAX_IMPL_METHODS} methods); split it into components "
                        "that own their state"
                    )
                continue
            seen_spread.add(key)
            for label, now, was in (("files", files, held["files"]), ("methods", methods, held["methods"])):
                if now > was:
                    errors.append(f"impl-spread: {key} grew to {now} {label}, baseline {was}")
                elif now < was:
                    errors.append(f"impl-spread: {key} has {now} {label}, below the baseline {was}; "
                                  f"lower {BASELINE.name} in this commit")
            if not str(held.get("reason", "")).strip():
                errors.append(f"impl-spread: baseline entry {key} must name the change that removes it")
    for key in sorted(set(spread_held) - seen_spread):
        errors.append(f"impl-spread: {key} no longer exists; remove it from {BASELINE.name} in this commit")
    for rule in ("super-glob", "module-cycle"):
        held: dict[str, int] = baseline.get(rule, {})
        for crate in sorted(set(measures) | set(held)):
            now, was = measures.get(crate, {}).get(rule, 0), held.get(crate, 0)
            if now > was:
                errors.append(f"{rule}: {crate} is at {now}, baseline {was}" + (
                    "; import the names the module uses" if rule == "super-glob"
                    else "; reach the other module through a port, or move the shared part down"))
            elif now < was:
                errors.append(f"{rule}: {crate} is at {now}, below the baseline {was}; lower {BASELINE.name} "
                              "in this commit")
    pending = set(baseline.get("unreachable-pub-pending", []))
    for crate, measure in sorted(measures.items()):
        if measure["unreachable-pub"] and crate in pending:
            errors.append(f"unreachable-pub: {crate} adopted the lint; remove it from the pending list in this commit")
        elif not measure["unreachable-pub"] and crate not in pending:
            errors.append(f"unreachable-pub: {crate} must declare #![warn(unreachable_pub)] in its crate root")
    for crate in sorted(pending - set(measures)):
        errors.append(f"unreachable-pub: {crate} is pending but is no longer a crate; remove it in this commit")
    long_held: dict[str, int] = baseline.get("long-function", {})
    seen_long: set[str] = set()
    for crate, measure in sorted(measures.items()):
        for function, lines in sorted(measure.get("long-function", {}).items()):
            key = f"{crate}::{function}"
            held = long_held.get(key)
            if held is None:
                errors.append(f"long-function: {key} is {lines} lines (limit {MAX_FUNCTION_LINES}); split it, or "
                              f"rename its key in {BASELINE.name} if it moved")
                continue
            seen_long.add(key)
            if lines > held:
                errors.append(f"long-function: {key} grew to {lines} lines, baseline {held}")
            elif lines < held:
                errors.append(f"long-function: {key} is {lines} lines, below the baseline {held}; lower "
                              f"{BASELINE.name} in this commit")
    budgeted = baseline.get("crate-budget", {})
    for crate, measure in sorted(measures.items()):
        lines = measure.get("lines", 0)
        if lines > CRATE_REVIEW_LINES and crate not in budgeted:
            errors.append(f"crate-budget: {crate} has {lines} production code lines, over the review threshold "
                          f"{CRATE_REVIEW_LINES}; check it is still one boundary (ADR-0091 D2) and record it in "
                          f"{BASELINE.name} with the review or the work item that shrinks it")
    for crate, held in sorted(budgeted.items()):
        measure = measures.get(crate)
        if measure is None:
            errors.append(f"crate-budget: {crate} is no longer a crate; remove it from {BASELINE.name} in this commit")
            continue
        if not str(held.get("reason", "")).strip():
            errors.append(f"crate-budget: baseline entry {crate} must name the split it guards")
        lines, deps = measure.get("lines", 0), len(measure.get("workspace-deps", ()))
        if lines <= CRATE_REVIEW_LINES:
            errors.append(f"crate-budget: {crate} is at {lines} production code lines, within the review "
                          f"threshold {CRATE_REVIEW_LINES}; remove it from {BASELINE.name} in this commit")
            continue
        if lines > held["lines"]:
            errors.append(f"crate-budget: {crate} grew to {lines} production lines, budget {held['lines']}; "
                          "put the new behavior in the crate it is moving to")
        elif lines <= held["lines"] - BUDGET_LINE_SLACK:
            errors.append(f"crate-budget: {crate} is at {lines} production lines, {BUDGET_LINE_SLACK}+ below the "
                          f"budget {held['lines']}; lower {BASELINE.name} in this commit")
        if deps > held["deps"]:
            errors.append(f"crate-budget: {crate} grew to {deps} workspace dependencies, budget {held['deps']}")
        elif deps < held["deps"]:
            errors.append(f"crate-budget: {crate} has {deps} workspace dependencies, below the budget "
                          f"{held['deps']}; lower {BASELINE.name} in this commit")
    for key in sorted(set(long_held) - seen_long):
        errors.append(f"long-function: {key} is no longer over {MAX_FUNCTION_LINES} lines or no longer exists; "
                      f"remove it from {BASELINE.name} in this commit (or rename it if it moved)")
    return [f"modularity: {error}" for error in errors]


def tightened(measures: dict[str, dict], baseline: dict) -> dict:
    """The baseline lowered to today's measures: entries only shrink or disappear.

    Nothing is ever added or raised, so `--tighten` cannot admit growth; the
    crate budget keeps its reason and drops to today's line count only when the
    guard would demand it (a fall of the slack or more).
    """
    out = json.loads(json.dumps(baseline))
    spread = {}
    for key, held in baseline.get("impl-spread", {}).items():
        crate, _, ty = key.partition("::")
        now = measures.get(crate, {}).get("impl-spread", {}).get(ty)
        if now is not None:
            spread[key] = {**held, "files": min(held["files"], now[0]), "methods": min(held["methods"], now[1])}
    out["impl-spread"] = spread
    for rule in ("super-glob", "module-cycle"):
        out[rule] = {
            crate: min(was, measures[crate].get(rule, 0))
            for crate, was in baseline.get(rule, {}).items()
            if crate in measures and min(was, measures[crate].get(rule, 0)) > 0
        }
    out["unreachable-pub-pending"] = [
        crate for crate in baseline.get("unreachable-pub-pending", [])
        if crate in measures and not measures[crate]["unreachable-pub"]
    ]
    long_held = {}
    for key, was in baseline.get("long-function", {}).items():
        crate, _, function = key.partition("::")
        now = measures.get(crate, {}).get("long-function", {}).get(function)
        if now is not None:
            long_held[key] = min(was, now)
    out["long-function"] = long_held
    budget = {}
    for crate, held in baseline.get("crate-budget", {}).items():
        measure = measures.get(crate)
        if measure is None:
            continue
        lines, deps = measure.get("lines", 0), len(measure.get("workspace-deps", ()))
        if lines <= CRATE_REVIEW_LINES:
            continue
        budget[crate] = {**held,
                         "lines": lines if lines <= held["lines"] - BUDGET_LINE_SLACK else held["lines"],
                         "deps": min(held["deps"], deps)}
    out["crate-budget"] = budget
    return out


def workspace_measures(crates: Path) -> dict[str, dict]:
    measures = {
        _crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(path)): crate_measures(path)
        for path in _crate_boundary_workspace.crate_manifests(crates)
    }
    for measure in measures.values():
        measure["workspace-deps"] = frozenset(measure.get("deps", ())) & set(measures)
    return measures


def check_all(crates: Path, baseline_path: Path = BASELINE) -> list[str]:
    return modularity_errors(workspace_measures(crates), json.loads(baseline_path.read_text(encoding="utf-8")))


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                        | effect |
    | M1   | type in <= 5 files and <= 60 methods, not recorded           | accept |
    | M2   | unrecorded type over either limit                            | error |
    | M3   | recorded type grows / shrinks / equal / gone / no reason     | error / error (lower) / accept / stale / error |
    | M4   | `use super::*` count above / below / at baseline             | error / error (lower) / accept |
    | M5   | `crate::a` <-> `super::b` paths close a cycle of top modules | cycle size counted |
    | M6   | lint adopted while pending / missing while not pending       | error / error |
    | M7   | function over the limit: unrecorded / grew / shorter / equal / gone or under | error / error / error (lower) / accept / stale |
    | M8   | budgeted crate: lines grow / float < slack / fall >= slack; deps grow / shrink / equal | error / accept / error (lower); error / error (lower) / accept |
    """
    spread = impl_spread({"a.rs": "impl Big { fn x(&self) {} fn y(&self) {} }", "b.rs": "impl<T> Big { fn z() {} }"})
    assert spread == {"Big": (2, 3)}, "impl-spread counting"
    pathed = impl_spread({"a.rs": "impl crate::host::Big { fn x(&self) {} }", "b.rs": "impl crate::Big for X { fn y() {} }"})
    assert pathed == {"Big": (1, 1)}, "impl-spread counts a crate-path inherent impl, not a trait impl"

    def measure(spread=None, globs=0, cycle=0, adopted=True, long=None):
        return {"impl-spread": spread or {}, "super-glob": globs, "module-cycle": cycle, "unreachable-pub": adopted,
                "long-function": long or {}}

    assert modularity_errors({"c": measure({"T": (5, 60)})}, {}) == [], "M1"
    assert "split it" in modularity_errors({"c": measure({"T": (6, 10)})}, {})[0], "M2 files"
    assert "split it" in modularity_errors({"c": measure({"T": (1, 61)})}, {})[0], "M2 methods"
    held = {"impl-spread": {"c::T": {"files": 10, "methods": 100, "reason": "split in H"}}}
    assert modularity_errors({"c": measure({"T": (10, 100)})}, held) == [], "M3 equal"
    assert "grew" in modularity_errors({"c": measure({"T": (11, 100)})}, held)[0], "M3 grow"
    assert "below the baseline" in modularity_errors({"c": measure({"T": (10, 90)})}, held)[0], "M3 shrink"
    assert "no longer exists" in modularity_errors({"c": measure()}, held)[0], "M3 gone"
    unreasoned = {"impl-spread": {"c::T": {"files": 10, "methods": 100, "reason": ""}}}
    assert "must name" in modularity_errors({"c": measure({"T": (10, 100)})}, unreasoned)[0], "M3 reason"
    assert "baseline 1" in modularity_errors({"c": measure(globs=2)}, {"super-glob": {"c": 1}})[0], "M4 above"
    assert "below the baseline" in modularity_errors({"c": measure(globs=0)}, {"super-glob": {"c": 1}})[0], "M4 below"
    assert modularity_errors({"c": measure(globs=1)}, {"super-glob": {"c": 1}}) == [], "M4 equal"
    cycle = largest_module_cycle(
        {"a.rs": "use crate::b::X;", "b/mod.rs": "use super::a::Y;", "c.rs": "use crate::a::Z;"}, {"a", "b", "c"}
    )
    assert cycle == 2, "M5"
    assert "remove it from the pending" in modularity_errors(
        {"c": measure(adopted=True)}, {"unreachable-pub-pending": ["c"]})[0], "M6 adopted"
    assert "must declare" in modularity_errors({"c": measure(adopted=False)}, {})[0], "M6 missing"
    body = "fn big() {\n" + "    let x = \"{\";\n" * (MAX_FUNCTION_LINES + 1) + "}\nfn small() {\n}\n"
    assert long_functions({"a.rs": body}) == {"a.rs::big": MAX_FUNCTION_LINES + 2}, "M7 measure (braces in literals)"
    held = {"long-function": {"c::a.rs::big": 200}}
    assert "split it" in modularity_errors({"c": measure(long={"a.rs::big": 200})}, {})[0], "M7 unrecorded"
    assert "grew" in modularity_errors({"c": measure(long={"a.rs::big": 201})}, held)[0], "M7 grew"
    assert "below the baseline" in modularity_errors({"c": measure(long={"a.rs::big": 190})}, held)[0], "M7 shorter"
    assert modularity_errors({"c": measure(long={"a.rs::big": 200})}, held) == [], "M7 equal"
    assert "no longer over" in modularity_errors({"c": measure()}, held)[0], "M7 gone"
    budget = {"crate-budget": {"c": {"lines": 20000, "deps": 3, "reason": "split"}}}

    def sized(lines, deps):
        return {"c": {**measure(), "lines": lines, "workspace-deps": frozenset(f"d{i}" for i in range(deps))}}

    assert modularity_errors(sized(20000, 3), budget) == [], "M8 equal"
    assert "grew to 20001" in modularity_errors(sized(20001, 3), budget)[0], "M8 lines grow"
    assert modularity_errors(sized(20000 - BUDGET_LINE_SLACK + 1, 3), budget) == [], "M8 float"
    assert "lower" in modularity_errors(sized(20000 - BUDGET_LINE_SLACK, 3), budget)[0], "M8 fall"
    assert "workspace dependencies, budget" in modularity_errors(sized(20000, 4), budget)[0], "M8 deps grow"
    assert "below the budget" in modularity_errors(sized(20000, 2), budget)[0], "M8 deps shrink"
    # M10 review threshold: an unrecorded crate over it is an error; a recorded one at or below it is removed.
    assert "review threshold" in modularity_errors(sized(CRATE_REVIEW_LINES + 1, 3), {})[0], "M10 unrecorded over"
    assert modularity_errors(sized(CRATE_REVIEW_LINES, 3), {}) == [], "M10 unrecorded at threshold"
    assert "remove it" in modularity_errors(sized(CRATE_REVIEW_LINES, 3), budget)[0], "M10 recorded fell to threshold"
    assert "c" not in tightened(sized(CRATE_REVIEW_LINES, 3), budget)["crate-budget"], "M10 tighten drops it"
    # M9 `--tighten`: lowers or removes held entries, never adds or raises one.
    held = {"impl-spread": {"c::T": {"files": 10, "methods": 100, "reason": "r"}, "c::Gone": {"files": 9,
            "methods": 90, "reason": "r"}}, "super-glob": {"c": 3}, "module-cycle": {"c": 2},
            "unreachable-pub-pending": ["c", "gone"], "long-function": {"c::a.rs::big": 200, "c::a.rs::gone": 300},
            "crate-budget": {"c": {"lines": 20000, "deps": 3, "reason": "split"}}}
    now = {"c": {**measure({"T": (8, 120)}, globs=1, cycle=4, adopted=True, long={"a.rs::big": 180}),
                 "lines": 20000 - BUDGET_LINE_SLACK, "workspace-deps": frozenset({"d0", "d1"})}}
    tight = tightened(now, held)
    assert tight["impl-spread"] == {"c::T": {"files": 8, "methods": 100, "reason": "r"}}, "M9 spread"
    assert tight["super-glob"] == {"c": 1} and tight["module-cycle"] == {"c": 2}, "M9 counts never rise"
    assert tight["unreachable-pub-pending"] == [], "M9 pending"
    assert tight["long-function"] == {"c::a.rs::big": 180}, "M9 long-function"
    assert tight["crate-budget"]["c"] == {"lines": 20000 - BUDGET_LINE_SLACK, "deps": 2, "reason": "split"}, "M9 budget"
    assert modularity_errors({"c": {**now["c"], "impl-spread": {"T": (8, 100)}, "module-cycle": 2}}, tight) == [], \
        "M9 a tightened baseline holds"
    print("_modularity_fitness selftest: the decision tables hold")


def parse(argv: list[str]):
    import argparse
    parser = argparse.ArgumentParser(prog="_modularity_fitness.py", description=__doc__, allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true", help="run the decision tables")
    parser.add_argument("--tighten", action="store_true",
                        help=f"lower {BASELINE.name} to today's measures (never raises or adds an entry)")
    return parser.parse_args(argv)


if __name__ == "__main__":
    args = parse(sys.argv[1:])
    if args.self_test:
        selftest()
        raise SystemExit(0)
    if args.tighten:
        baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
        BASELINE.write_text(json.dumps(tightened(workspace_measures(_crate_boundary_workspace.CRATES), baseline),
                                       indent=2) + "\n", encoding="utf-8")
        print(f"tightened {BASELINE.name}")
        raise SystemExit(0)
    errors = check_all(_crate_boundary_workspace.CRATES)
    print("\n".join(errors) if errors else "OK - modularity ratchets hold.")
    raise SystemExit(1 if errors else 0)
