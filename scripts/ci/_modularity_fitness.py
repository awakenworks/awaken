"""Module ownership and dependency ratchets, with diagnostic line measurements.

Plan 9.12 / W22. A module is a boundary when it owns its state and callers
reach it through a narrow surface. Production measurements exclude comments
and provably test-only code. Structural and dependency ratchets stay hard;
line counts remain visible WARN diagnostics and never require a baseline
rewrite before continuing (ADR-0091, 2026-10-28).

* `impl-spread`: one type's inherent impls exceed MAX_IMPL_FILES or
  MAX_IMPL_METHODS. Recorded files/methods may only fall and retain their review.
* `super-glob`: counted `use super::*` imports may only fall.
* `module-cycle`: top-level crate/super dependency cycles may only shrink.
* `unreachable-pub`: adoption and the pending-crate registry remain enforced.
* `long-function`: new, growing, shrinking and disappearing measured functions
  are advisory. Historical line measurements may remain without being raised.
* `crate-budget`: whole, host and extracting-realization line measurements are
  advisory, including threshold crossings. Registered crate review metadata
  and exact workspace-dependency ceilings remain hard, even after line counts
  fall below the review threshold. Line changes cannot erase dependency review.

Default/--check is read-only. --tighten only lowers/removes held measurements,
never adds or raises an allowance; historical crate dependency entries remain
registered below the line threshold. Explicit baseline recording retains the
existing review admission and cannot waive structural, metadata or dependency
checks. No other architecture guard changes its policy through this module.
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter, defaultdict
from collections.abc import Iterable
from pathlib import Path

import _crate_boundary_workspace
import _architecture_ratchet
import _module_layer_fitness
from _context_map_fitness import _flatten_path_tree

BASELINE = Path(__file__).with_name("modularity_baseline.json")
MAX_IMPL_FILES = 5
MAX_IMPL_METHODS = 60
MAX_FUNCTION_LINES = 150
BUDGET_LINE_SLACK = 1000
CRATE_REVIEW_LINES = 15000

# An inherent impl may name its type through a crate path (`impl crate::host::ExecutorComponents`);
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


def extracting_realization_files(crate: str, files: Iterable[str], table=None) -> set[str]:
    """Files of the runtime-host modules the module-layer guard classifies as `realization`."""
    if crate != _module_layer_fitness.CRATE:
        return set()
    table = _module_layer_fitness.layer_table() if table is None else table
    return {relative for relative in files if _module_layer_fitness.target_of(
        tuple(_module_of(relative)), table) == _module_layer_fitness.REALIZATION}


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
    manifest = _crate_boundary_workspace.load_manifest(manifest_path)
    layer = manifest.get("package", {}).get("metadata", {}).get("awaken", {}).get("layer", "")
    extracting = extracting_realization_files(_crate_boundary_workspace.package_name(manifest), files)

    def code_lines(selected: Iterable[str]) -> int:
        return sum(1 for relative in selected for line in files[relative].splitlines() if line.strip())

    return {
        "lines": code_lines(files),
        "host-lines": code_lines(set(files) - extracting),
        "extracting-realization-lines": code_lines(extracting),
        "deps": _crate_boundary_workspace.normal_dependency_names(
            _crate_boundary_workspace.load_manifest(manifest_path)),
        "long-function": {} if layer == "tooling" else long_functions(files),
        "impl-spread": impl_spread(files),
        "super-glob": super_globs(files),
        "module-cycle": largest_module_cycle(files, top),
        "unreachable-pub": adopted,
    }


def modularity_errors(
    measures: dict[str, dict], baseline: dict, *, warnings: list[str] | None = None,
) -> list[str]:
    """Classify every finding once; only structural/dependency errors block.

    Callers may collect line diagnostics without giving them failure authority.
    This owner performs no output or baseline mutation.
    """
    errors: list[str] = []
    line_warnings = warnings if warnings is not None else []
    line_review = "review responsibility and ownership; line counts are diagnostic, not a split requirement"
    line_lower = f"retain the historical measurement, or optionally use {Path(__file__).name} --tighten to lower it"
    lower = f"python3 scripts/ci/{Path(__file__).name} --tighten to lower {BASELINE.name} in this commit"
    spread_held: dict[str, dict] = baseline.get("impl-spread", {})
    seen_spread: set[str] = set()
    for crate, measure in sorted(measures.items()):
        for ty, (files, methods) in sorted(measure["impl-spread"].items()):
            key = f"{crate}::{ty}"
            held = spread_held.get(key)
            if held is None:
                if files > MAX_IMPL_FILES or methods > MAX_IMPL_METHODS:
                    errors.append(_architecture_ratchet.diagnostic(
                        "impl-spread", key, f"{MAX_IMPL_FILES} files/{MAX_IMPL_METHODS} methods",
                        f"{files} files/{methods} methods", "split it into components that own their state"))
                continue
            seen_spread.add(key)
            for label, now, was in (("files", files, held["files"]), ("methods", methods, held["methods"])):
                if now > was:
                    errors.append(_architecture_ratchet.diagnostic(
                        "impl-spread", f"{key}.{label}", was, now, "split the type into components that own their state"))
                elif now < was:
                    errors.append(_architecture_ratchet.diagnostic("impl-spread", f"{key}.{label}", was, now, lower))
            if not str(held.get("reason", "")).strip():
                errors.append(_architecture_ratchet.diagnostic(
                    "impl-spread", f"{key}.reason", "reviewed change", "missing",
                    f"restore the owning reviewed change in {BASELINE.name} impl-spread[{key}].reason"))
    for key in sorted(set(spread_held) - seen_spread):
        errors.append(_architecture_ratchet.diagnostic("impl-spread", key, "recorded type", "absent", lower))
    for rule in ("super-glob", "module-cycle"):
        held: dict[str, int] = baseline.get(rule, {})
        for crate in sorted(set(measures) | set(held)):
            now, was = measures.get(crate, {}).get(rule, 0), held.get(crate, 0)
            if now > was:
                errors.append(_architecture_ratchet.diagnostic(rule, crate, was, now,
                    "import the names the module uses" if rule == "super-glob"
                    else "reach the other module through a port, or move the shared part down"))
            elif now < was:
                errors.append(_architecture_ratchet.diagnostic(rule, crate, was, now, lower))
    pending = set(baseline.get("unreachable-pub-pending", []))
    for crate, measure in sorted(measures.items()):
        if measure["unreachable-pub"] and crate in pending:
            errors.append(_architecture_ratchet.diagnostic("unreachable-pub", crate, "pending", "adopted", lower))
        elif not measure["unreachable-pub"] and crate not in pending:
            errors.append(_architecture_ratchet.diagnostic(
                "unreachable-pub", crate, "required lint", "missing", "declare #![warn(unreachable_pub)] in its crate root"))
    for crate in sorted(pending - set(measures)):
        errors.append(_architecture_ratchet.diagnostic("unreachable-pub", crate, "pending crate", "absent", lower))
    long_held: dict[str, int] = baseline.get("long-function", {})
    seen_long: set[str] = set()
    for crate, measure in sorted(measures.items()):
        for function, lines in sorted(measure.get("long-function", {}).items()):
            key = f"{crate}::{function}"
            held = long_held.get(key)
            if held is None:
                line_warnings.append(_architecture_ratchet.diagnostic(
                    "long-function", key, MAX_FUNCTION_LINES, lines,
                    line_review))
                continue
            seen_long.add(key)
            if lines > held:
                line_warnings.append(_architecture_ratchet.diagnostic("long-function", key, held, lines, line_review))
            elif lines < held:
                line_warnings.append(_architecture_ratchet.diagnostic("long-function", key, held, lines, line_lower))
    budgeted = baseline.get("crate-budget", {})
    for crate, measure in sorted(measures.items()):
        lines = measure.get("lines", 0)
        if lines > CRATE_REVIEW_LINES and crate not in budgeted:
            line_warnings.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.lines", CRATE_REVIEW_LINES, lines,
                line_review))
    for crate, measure in sorted(measures.items()):
        extracting = measure.get("extracting-realization-lines", 0)
        if extracting > CRATE_REVIEW_LINES:
            line_warnings.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.extracting-realization-lines", CRATE_REVIEW_LINES, extracting,
                line_review))
    for crate, held in sorted(budgeted.items()):
        measure = measures.get(crate)
        if measure is None:
            errors.append(_architecture_ratchet.diagnostic("crate-budget", crate, "recorded crate", "absent", lower))
            continue
        if not str(held.get("reason", "")).strip():
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.reason", "reviewed split", "missing",
                f"restore the owning reviewed split in {BASELINE.name} crate-budget[{crate}].reason"))
        lines, deps = measure.get("lines", 0), len(measure.get("workspace-deps", ()))
        if lines <= CRATE_REVIEW_LINES:
            line_warnings.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.lines", held.get("lines", "recorded above review threshold"), lines, line_lower,
                details=f"within review threshold {CRATE_REVIEW_LINES}"))
        elif lines > held["lines"]:
            line_warnings.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.lines", held["lines"], lines, line_review))
        elif lines <= held["lines"] - BUDGET_LINE_SLACK:
            line_warnings.append(_architecture_ratchet.diagnostic("crate-budget", f"{crate}.lines", held["lines"], lines, line_lower))
        if "host_lines" in held:
            host = measure.get("host-lines", lines)
            if host > held["host_lines"]:
                line_warnings.append(_architecture_ratchet.diagnostic(
                    "crate-budget", f"{crate}.host_lines", held["host_lines"], host,
                    line_review))
            elif host <= held["host_lines"] - BUDGET_LINE_SLACK:
                line_warnings.append(_architecture_ratchet.diagnostic("crate-budget", f"{crate}.host_lines", held["host_lines"], host, line_lower))
        if deps > held["deps"]:
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.deps", held["deps"], deps, "put the new dependency in the crate it is moving to"))
        elif deps < held["deps"]:
            errors.append(_architecture_ratchet.diagnostic("crate-budget", f"{crate}.deps", held["deps"], deps, lower))
    for key in sorted(set(long_held) - seen_long):
        line_warnings.append(_architecture_ratchet.diagnostic(
            "long-function", key, long_held[key], f"absent or <= {MAX_FUNCTION_LINES}",
            line_lower))
    return [f"modularity: {error}" for error in errors]


def _reported_modularity_errors(
    measures: dict[str, dict], baseline: dict, *, seen: set[str] | None = None,
) -> list[str]:
    """The CLI and aggregate entrypoint display the same owner's diagnostics."""
    warnings: list[str] = []
    errors = modularity_errors(measures, baseline, warnings=warnings)
    for warning in warnings:
        if seen is not None:
            if warning in seen:
                continue
            seen.add(warning)
        print(f"WARN: modularity: {warning}")
    return errors


def tightened(measures: dict[str, dict], baseline: dict, *, record: bool = False, reason: str | None = None) -> dict:
    """The existing candidate writer, shrink-only unless explicitly recording.

    Shrink mode never adds/raises an allowance. The CLI checks its candidate
    with the same decision owner, so a clamp cannot hide structural or
    dependency growth. Line changes are advisory; explicit recording cannot
    add lint exemptions or waive required review metadata.
    """
    out = json.loads(json.dumps(baseline))
    reasons = out.pop("growth_reasons", {})
    owned = {"impl-spread", "super-glob", "module-cycle", "long-function", "crate-budget"}
    kept_reasons: dict[str, dict[str, str]] = {name: value for name, value in reasons.items() if name not in owned}

    def count(rule: str, key: str, now: int, was: int, slack: int = 1) -> int:
        earlier = kept_reasons.get(rule, {}).get(key, reasons.get(rule, {}).get(key))
        if record and now > was and reason:
            earlier = _architecture_ratchet.growth_reason(earlier, reason)
        if earlier:
            kept_reasons.setdefault(rule, {})[key] = earlier
        return now if record or now <= was - slack else was

    spread = {}
    held_spread = baseline.get("impl-spread", {})
    keys = set(held_spread)
    if record:
        keys |= {f"{crate}::{ty}" for crate, measure in measures.items()
                 for ty, (files, methods) in measure["impl-spread"].items()
                 if files > MAX_IMPL_FILES or methods > MAX_IMPL_METHODS}
    for key in sorted(keys):
        crate, _, ty = key.partition("::")
        now = measures.get(crate, {}).get("impl-spread", {}).get(ty)
        if now is not None:
            held = held_spread.get(key, {"files": MAX_IMPL_FILES, "methods": MAX_IMPL_METHODS, "reason": reason or ""})
            spread[key] = {**held, "files": count("impl-spread", key, now[0], held["files"]),
                           "methods": count("impl-spread", key, now[1], held["methods"])}
    out["impl-spread"] = spread
    for rule in ("super-glob", "module-cycle"):
        held = baseline.get(rule, {})
        keys = set(held) | (set(measures) if record else set())
        out[rule] = {crate: count(rule, crate, measures[crate].get(rule, 0), held.get(crate, 0))
                     for crate in sorted(keys) if crate in measures and measures[crate].get(rule, 0) > 0}
    out["unreachable-pub-pending"] = [
        crate for crate in baseline.get("unreachable-pub-pending", [])
        if crate in measures and not measures[crate]["unreachable-pub"]
    ]
    held_long = baseline.get("long-function", {})
    keys = set(held_long)
    if record:
        keys |= {f"{crate}::{function}" for crate, measure in measures.items()
                 for function in measure.get("long-function", {})}
    long_held = {}
    for key in sorted(keys):
        crate, _, function = key.partition("::")
        now = measures.get(crate, {}).get("long-function", {}).get(function)
        if now is not None:
            long_held[key] = count("long-function", key, now, held_long.get(key, MAX_FUNCTION_LINES))
    out["long-function"] = long_held
    held_budget = baseline.get("crate-budget", {})
    keys = set(held_budget) | (set(measures) if record else set())
    budget = {}
    for crate in sorted(keys):
        measure = measures.get(crate)
        if measure is None:
            continue
        lines, deps = measure.get("lines", 0), len(measure.get("workspace-deps", ()))
        if lines <= CRATE_REVIEW_LINES and crate not in held_budget:
            continue
        # A line-threshold crossing must not delete the held dependency ceiling.
        held = held_budget.get(crate, {"lines": CRATE_REVIEW_LINES, "deps": 0, "reason": reason or ""})
        budget[crate] = {**held, "lines": count("crate-budget", crate, lines, held["lines"], BUDGET_LINE_SLACK),
                         "deps": count("crate-budget", crate, deps, held["deps"])}
        if "host_lines" in held:
            host = measure.get("host-lines", lines)
            budget[crate]["host_lines"] = count("crate-budget", crate, host, held["host_lines"], BUDGET_LINE_SLACK)
    out["crate-budget"] = budget
    if kept_reasons:
        out["growth_reasons"] = kept_reasons
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
    return _reported_modularity_errors(workspace_measures(crates), json.loads(baseline_path.read_text(encoding="utf-8")))


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                        | effect |
    | M1   | type in <= 5 files and <= 60 methods, not recorded           | accept |
    | M2   | unrecorded type over either limit                            | error |
    | M3   | recorded type grows / shrinks / equal / gone / no reason     | error / error (lower) / accept / stale / error |
    | M4   | `use super::*` count above / below / at baseline             | error / error (lower) / accept |
    | M5   | `crate::a` <-> `super::b` paths close a cycle of top modules | cycle size counted |
    | M6   | lint adopted while pending / missing while not pending       | error / error |
    | M7   | function over the limit: unrecorded / grew / shorter / equal / gone or under | WARN / WARN / WARN / accept / WARN |
    | M8   | budgeted crate: lines grow / float < slack / fall >= slack; deps grow / shrink / equal | WARN / accept / WARN; error / error (lower) / accept |
    | M10  | new large / threshold crossing / below threshold with held deps | WARN / WARN / dependency checks remain hard |
    | M11  | runtime-host file in a realization / host module; other crate; extracting realization over / at threshold | extracting / host lines; host lines; WARN / accept |
    | M12  | split crate: whole grows / host grows / host falls >= slack / move host->realization at equal whole; tighten | WARN / WARN / WARN / accept; host_lines lowered, never raised |
    | M13  | any line finding + deps grow/shrink/equal, including below threshold | WARN + error/error/accept, no dependency escape |

    D3/D4 graph: existing threshold/held/dimension/metadata condition -> the
    same structural hard result or line-only warning -> owning diagnostic.
    M1-M7 retain spread dimensions, exact counts, pending lint and long keys;
    M8/M10-M13 cross review thresholds, line slack, exact dependencies and
    whole/host/extracting scope. MCLI3 retains hard admission for structural
    changes and metadata; no line warning becomes authorization to grow deps.
    """
    spread = impl_spread({"a.rs": "impl Big { fn x(&self) {} fn y(&self) {} }", "b.rs": "impl<T> Big { fn z() {} }"})
    assert spread == {"Big": (2, 3)}, "impl-spread counting"
    pathed = impl_spread({"a.rs": "impl crate::host::Big { fn x(&self) {} }", "b.rs": "impl crate::Big for X { fn y() {} }"})
    assert pathed == {"Big": (1, 1)}, "impl-spread counts a crate-path inherent impl, not a trait impl"

    def measure(spread=None, globs=0, cycle=0, adopted=True, long=None):
        return {"impl-spread": spread or {}, "super-glob": globs, "module-cycle": cycle, "unreachable-pub": adopted,
                "long-function": long or {}}

    def line_findings(current, previous):
        # Pure-line rows accept without mutating their historical baseline.
        before = json.dumps(previous, sort_keys=True)
        warnings: list[str] = []
        assert modularity_errors(current, previous, warnings=warnings) == []
        assert json.dumps(previous, sort_keys=True) == before
        return warnings

    assert modularity_errors({"c": measure({"T": (5, 60)})}, {}) == [], "M1"
    assert "split it" in modularity_errors({"c": measure({"T": (6, 10)})}, {})[0], "M2 files"
    assert "split it" in modularity_errors({"c": measure({"T": (1, 61)})}, {})[0], "M2 methods"
    held = {"impl-spread": {"c::T": {"files": 10, "methods": 100, "reason": "split in H"}}}
    assert modularity_errors({"c": measure({"T": (10, 100)})}, held) == [], "M3 equal"
    assert "c::T.files 10 -> 11" in modularity_errors({"c": measure({"T": (11, 100)})}, held)[0], "M3 grow"
    assert "c::T.methods 100 -> 90" in modularity_errors({"c": measure({"T": (10, 90)})}, held)[0], "M3 shrink"
    assert "c::T recorded type -> absent" in modularity_errors({"c": measure()}, held)[0], "M3 gone"
    unreasoned = {"impl-spread": {"c::T": {"files": 10, "methods": 100, "reason": ""}}}
    assert "c::T.reason reviewed change -> missing" in modularity_errors({"c": measure({"T": (10, 100)})}, unreasoned)[0], "M3 reason"
    assert "impl-spread[c::T].reason" in modularity_errors({"c": measure({"T": (10, 100)})}, unreasoned)[0], \
        "D3 existing review metadata is fixed at its owner, not by a CLI reason"
    assert "super-glob: c 1 -> 2" in modularity_errors({"c": measure(globs=2)}, {"super-glob": {"c": 1}})[0], "M4 above"
    assert "super-glob: c 1 -> 0" in modularity_errors({"c": measure(globs=0)}, {"super-glob": {"c": 1}})[0], "M4 below"
    assert modularity_errors({"c": measure(globs=1)}, {"super-glob": {"c": 1}}) == [], "M4 equal"
    cycle = largest_module_cycle(
        {"a.rs": "use crate::b::X;", "b/mod.rs": "use super::a::Y;", "c.rs": "use crate::a::Z;"}, {"a", "b", "c"}
    )
    assert cycle == 2, "M5"
    assert "unreachable-pub: c pending -> adopted" in modularity_errors(
        {"c": measure(adopted=True)}, {"unreachable-pub-pending": ["c"]})[0], "M6 adopted"
    assert "required lint -> missing" in modularity_errors({"c": measure(adopted=False)}, {})[0], "M6 missing"
    body = "fn big() {\n" + "    let x = \"{\";\n" * (MAX_FUNCTION_LINES + 1) + "}\nfn small() {\n}\n"
    assert long_functions({"a.rs": body}) == {"a.rs::big": MAX_FUNCTION_LINES + 2}, "M7 measure (braces in literals)"
    held = {"long-function": {"c::a.rs::big": 200}}
    assert "c::a.rs::big 150 -> 200" in line_findings({"c": measure(long={"a.rs::big": 200})}, {})[0], "M7 unrecorded WARN"
    assert "c::a.rs::big 200 -> 201" in line_findings({"c": measure(long={"a.rs::big": 201})}, held)[0], "M7 grew WARN"
    assert "c::a.rs::big 200 -> 190" in line_findings({"c": measure(long={"a.rs::big": 190})}, held)[0], "M7 shorter WARN"
    assert modularity_errors({"c": measure(long={"a.rs::big": 200})}, held) == [], "M7 equal"
    assert "c::a.rs::big 200 -> absent or <= 150" in line_findings({"c": measure()}, held)[0], "M7 gone WARN"
    budget = {"crate-budget": {"c": {"lines": 20000, "deps": 3, "reason": "split"}}}

    def sized(lines, deps):
        return {"c": {**measure(), "lines": lines, "workspace-deps": frozenset(f"d{i}" for i in range(deps))}}

    assert modularity_errors(sized(20000, 3), budget) == [], "M8 equal"
    assert "c.lines 20000 -> 20001" in line_findings(sized(20001, 3), budget)[0], "M8 lines grow WARN"
    assert modularity_errors(sized(20000 - BUDGET_LINE_SLACK + 1, 3), budget) == [], "M8 float"
    assert "lower" in line_findings(sized(20000 - BUDGET_LINE_SLACK, 3), budget)[0], "M8 fall WARN"
    assert "c.deps 3 -> 4" in modularity_errors(sized(20000, 4), budget)[0], "M8 deps grow"
    assert "c.deps 3 -> 2" in modularity_errors(sized(20000, 2), budget)[0], "M8 deps shrink"
    # M10: line thresholds are advisory; a historical dependency ceiling
    # remains enforceable after crossing the threshold in either direction.
    assert "c.lines 15000 -> 15001" in line_findings(sized(CRATE_REVIEW_LINES + 1, 3), {})[0], "M10 unrecorded over WARN"
    assert modularity_errors(sized(CRATE_REVIEW_LINES, 3), {}) == [], "M10 unrecorded at threshold"
    assert "c.lines 20000 -> 15000" in line_findings(sized(CRATE_REVIEW_LINES, 3), budget)[0], "M10 recorded fell to threshold WARN"
    assert tightened(sized(CRATE_REVIEW_LINES, 3), budget)["crate-budget"]["c"] == {
        "lines": CRATE_REVIEW_LINES, "deps": 3, "reason": "split"}, "M10 tighten retains dependency metadata"
    # M11 the realization being extracted from runtime-host is measured as its future crate.
    layers = _module_layer_fitness.layer_table((
        (_module_layer_fitness.REALIZATION, "r", ("host::session::environment_lifecycle", "provisioning")),
        (_module_layer_fitness.HOST, "h", ("host", "host::session", "host::run"))))
    host_files = ["host/session/environment_lifecycle/renewal.rs", "provisioning/mod.rs", "host/run.rs", "lib.rs"]
    assert extracting_realization_files("awaken-session-executor", host_files, layers) == {
        "host/session/environment_lifecycle/renewal.rs", "provisioning/mod.rs"}, "M11 realization vs host modules"
    assert extracting_realization_files("awaken-session-application", ["provisioning.rs"], layers) == set(), \
        "M11 other crates are measured whole"
    over = {"c": {**measure(), "lines": 10, "extracting-realization-lines": CRATE_REVIEW_LINES + 1,
                  "workspace-deps": frozenset()}}
    assert "c.extracting-realization-lines 15000 -> 15001" in line_findings(over, {})[0], "M11 extracting over threshold WARN"
    over["c"]["extracting-realization-lines"] = CRATE_REVIEW_LINES
    assert modularity_errors(over, {}) == [], "M11 extracting at threshold"
    # M12 a crate being split holds its whole size and, separately, its host part.
    split = {"crate-budget": {"c": {"lines": 20000, "host_lines": 15000, "deps": 3, "reason": "split"}}}

    def parts(whole: int, host: int) -> dict:
        return {"c": {**measure(), "lines": whole, "host-lines": host, "extracting-realization-lines": whole - host,
                      "workspace-deps": frozenset({"d0", "d1", "d2"})}}
    assert any("c.lines 20000 -> 20001" in e for e in line_findings(parts(20001, 15000), split)), "M12 whole grows WARN"
    assert any("c.host_lines 15000 -> 15001" in e for e in line_findings(parts(20000, 15001), split)), "M12 host grows WARN"
    assert any("c.host_lines 15000 -> 14000" in e for e in line_findings(parts(20000, 14000), split)), "M12 host falls WARN"
    assert modularity_errors(parts(20000, 14500), split) == [], "M12 move inside the crate is not growth"
    assert tightened(parts(20000, 14000), split)["crate-budget"]["c"]["host_lines"] == 14000, "M12 tighten lowers"
    assert tightened(parts(20000, 15500), split)["crate-budget"]["c"]["host_lines"] == 15000, "M12 never raises"
    # M13 cause/effect table: line growth, slack shrink and threshold shrink
    # cannot hide either direction of exact dependency drift. Equal deps accept
    # with WARN; grow/shrink retain their original hard error, even below the
    # old size threshold. No check mutates the baseline to make that true.
    for lines in (20001, 20000 - BUDGET_LINE_SLACK, CRATE_REVIEW_LINES, CRATE_REVIEW_LINES - 1):
        for deps in (2, 3, 4):
            warnings: list[str] = []
            before = json.dumps(budget, sort_keys=True)
            errors = modularity_errors(sized(lines, deps), budget, warnings=warnings)
            assert warnings and all(".lines" not in error for error in errors), "M13 lines only diagnose"
            assert bool(errors) == (deps != 3), f"M13 independent deps {lines}/{deps}"
            if deps != 3:
                assert any(f"c.deps 3 -> {deps}" in error for error in errors), "M13 exact deps stay hard"
            assert json.dumps(budget, sort_keys=True) == before, "M13 pure decision is read-only"
    assert "recorded crate -> absent" in modularity_errors({}, budget)[0], "M13 absent crate metadata remains hard"
    no_reason = {"crate-budget": {"c": {**budget["crate-budget"]["c"], "reason": ""}}}
    assert any("c.reason" in error for error in modularity_errors(sized(CRATE_REVIEW_LINES - 1, 3), no_reason)), \
        "M13 missing metadata stays hard below a line threshold"
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
    _selftest_cli()
    print("_modularity_fitness selftest: the decision tables hold")


def _selftest_cli() -> None:
    """CLI causes/effects, retaining the original M1-M12 comparison table.

    MCLI1 default/check are read-only; tighten validates its clamped candidate
    and rejects structural/dependency growth, including mixed shrink/growth;
    line-only conditions WARN and accept without raising ceilings. MCLI2 pure
    shrink writes, equality preserves bytes, owner/expiry/foreign counters and
    their unknown reasons survive.
    MCLI3 explicit growth needs a reason and keeps its history, but a reason
    cannot waive lint or missing review metadata; realization size only warns.
    MCLI4 invalid modes/blank reasons/missing baseline precede measurement.
    MCLI5 line-only checks and aggregate check_all both display WARN; historical
    baseline bytes stay read-only and threshold shrink cannot waive deps.
    """
    measure = {"impl-spread": {"T": (8, 80)}, "super-glob": 2, "module-cycle": 2, "unreachable-pub": True,
               "long-function": {"a.rs::big": 200}, "lines": 20000, "host-lines": 18000,
               "workspace-deps": frozenset({"a", "b"})}
    foreign = {"future-counter": {"k": "  foreign review  "}}
    held = {"owner": "modularity", "expiry": "split", "future-counter": {"k": 9}, "growth_reasons": foreign, "impl-spread": {"c::T": {"files": 8, "methods": 80,
            "reason": "extract component"}}, "super-glob": {"c": 2}, "module-cycle": {"c": 2},
            "unreachable-pub-pending": [], "long-function": {"c::a.rs::big": 200},
            "crate-budget": {"c": {"lines": 20000, "host_lines": 18000, "deps": 2, "reason": "split c"}}}

    def invoke(argv, current=measure, baseline=held):
        return _architecture_ratchet._cli_fixture(main, argv, baseline, "workspace_measures", {"c": current})

    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, observed, _ = invoke(argv)
        assert status == 0 and before == after and observed, f"MCLI1/MCLI2 equal {argv}"
    rows = ({"impl-spread": {"T": (8, 81)}}, {"impl-spread": {"T": (8, 80), "New": (6, 10)}},
            {"super-glob": 3}, {"module-cycle": 3}, {"long-function": {"a.rs::big": 201}},
            {"long-function": {"new.rs::big": 180}}, {"lines": 20001}, {"host-lines": 18001},
            {"workspace-deps": frozenset({"a", "b", "new"})})
    for change in rows:
        # One growing field while super-glob shrinks: a successful min() must
        # not be mistaken for a successful check of the actual tree.
        current = {**measure, "super-glob": 1, **change}
        status, before, after, _, output = invoke(["--tighten"], current)
        if set(change) <= {"long-function", "lines", "host-lines"}:
            assert status == 0 and "WARN: modularity:" in output, f"MCLI1 line-only WARN {change}"
            document = json.loads(after)
            assert document["super-glob"] == {"c": 1}, "MCLI1 structural shrink still records"
            assert document["crate-budget"]["c"]["lines"] <= held["crate-budget"]["c"]["lines"], "MCLI1 tighten never raises lines"
            assert document["crate-budget"]["c"]["host_lines"] <= held["crate-budget"]["c"]["host_lines"], "MCLI1 tighten never raises host lines"
            assert all(key in held["long-function"] and value <= held["long-function"][key]
                       for key, value in document["long-function"].items()), "MCLI1 tighten never adds/raises long keys"
        else:
            assert status == 1 and before == after, f"MCLI1 growth {change}"
    smaller = {**measure, "super-glob": 1, "lines": 19000, "host-lines": 17000}
    status, _, after, _, _ = invoke(["--tighten"], smaller)
    document = json.loads(after)
    assert status == 0 and document["super-glob"] == {"c": 1}, "MCLI2 shrink"
    assert document["crate-budget"]["c"] == {**held["crate-budget"]["c"], "lines": 19000, "host_lines": 17000}, \
        "MCLI2 slack and held review reason"
    assert document["owner"] == held["owner"] and document["expiry"] == held["expiry"], "MCLI2 metadata"
    assert document["future-counter"] == held["future-counter"] and document["growth_reasons"] == foreign, \
        "MCLI2 foreign metadata/reason retained"
    current = {**measure, "super-glob": 3, "impl-spread": {"T": (9, 81)}}
    status, before, after, _, _ = invoke(["--write-baseline"], current)
    assert status == 1 and before == after, "MCLI3 unreasoned growth"
    prior = {**held, "growth_reasons": {**foreign, "impl-spread": {"c::T": "first"}}}
    status, _, after, _, _ = invoke(["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], current, prior)
    document = json.loads(after)
    assert status == 0 and document["impl-spread"]["c::T"] == {"files": 9, "methods": 81,
                                                              "reason": "extract component"}, "MCLI3 exact facts"
    assert document["growth_reasons"] == {**foreign, "impl-spread": {"c::T": "first; then: " + _architecture_ratchet._test_growth_reason("reviewed")},
                                          "super-glob": {"c": _architecture_ratchet._test_growth_reason("reviewed")}}, "MCLI3 reason history"
    assert document["future-counter"] == held["future-counter"], "MCLI3 foreign counter retained"
    # MCLI5: every original line partition stays a read-only WARN in default
    # and --check. Optional --tighten may lower/drop historical measurements,
    # but never raises them or bypasses the exact dependency comparator.
    line_rows = (
        {"long-function": {"a.rs::big": 201}}, {"long-function": {"a.rs::big": 190}},
        {"long-function": {"new.rs::big": 180}}, {"long-function": {}},
        {"lines": 20001}, {"lines": 19000}, {"lines": CRATE_REVIEW_LINES},
        {"host-lines": 18001}, {"host-lines": 17000},
        {"extracting-realization-lines": CRATE_REVIEW_LINES + 1},
    )
    for change in line_rows:
        for argv in ([], ["--check"]):
            status, before, after, observed, output = invoke(argv, {**measure, **change})
            assert status == 0 and before == after and observed, f"MCLI5 read-only line row {change}/{argv}"
            assert "WARN: modularity:" in output, f"MCLI5 visible CLI diagnostic {change}"
    for lines in (20001, 19000, CRATE_REVIEW_LINES, CRATE_REVIEW_LINES - 1):
        for argv in ([], ["--check"], ["--tighten"]):
            current = {**measure, "lines": lines, "workspace-deps": frozenset({"a", "b", "new"})}
            status, before, after, _, output = invoke(argv, current)
            assert status == 1 and before == after and "WARN: modularity:" in output, \
                f"MCLI5 line+dependency growth stays hard {lines}/{argv}"
    below = {**measure, "lines": CRATE_REVIEW_LINES - 1}
    status, _, after, _, output = invoke(["--tighten"], below)
    assert status == 0 and "WARN: modularity:" in output
    document = json.loads(after)
    assert document["crate-budget"]["c"]["deps"] == 2 and document["crate-budget"]["c"]["reason"] == "split c", \
        "MCLI5 threshold shrink preserves original dependency review"
    assert document["crate-budget"]["c"]["lines"] == below["lines"], "MCLI5 explicit tighten only lowers"

    # The real aggregate entrypoint uses the same classifier/reporter; patch
    # its scanner and read only a temporary baseline, never the live workspace.
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch
    with tempfile.TemporaryDirectory() as temporary:
        baseline_path = Path(temporary) / "baseline.json"
        baseline_path.write_text(json.dumps(held) + "\n", encoding="utf-8")
        before = baseline_path.read_bytes()
        output = io.StringIO()
        with patch.dict(check_all.__globals__, {"workspace_measures": lambda _root: {"c": {**measure, "lines": 20001}}}), contextlib.redirect_stdout(output):
            errors = check_all(Path(temporary) / "crates", baseline_path)
        assert errors == [] and "WARN: modularity:" in output.getvalue(), "MCLI5 aggregate WARN+accept"
        assert baseline_path.read_bytes() == before, "MCLI5 aggregate does not rewrite baseline"
        output = io.StringIO()
        current = {**measure, "lines": CRATE_REVIEW_LINES - 1, "workspace-deps": frozenset({"a", "b", "new"})}
        with patch.dict(check_all.__globals__, {"workspace_measures": lambda _root: {"c": current}}), contextlib.redirect_stdout(output):
            errors = check_all(Path(temporary) / "crates", baseline_path)
        assert any("c.deps 2 -> 3" in error for error in errors) and "WARN: modularity:" in output.getvalue(), \
            "MCLI5 aggregate threshold WARN cannot hide dependency growth"
        assert baseline_path.read_bytes() == before

    # Unlike shape's explicit crates={} initial state, an existing empty
    # modularity file is not a global authorization to grow a held metric.
    ordinary = {**measure, "impl-spread": {}, "long-function": {}, "lines": 0, "super-glob": 1}
    status, before, after, _, _ = invoke(["--write-baseline"], ordinary, {})
    assert status == 1 and before == after, "MCLI3 no borrowed initialization waiver"
    for change in ({"unreachable-pub": False}, {"extracting-realization-lines": CRATE_REVIEW_LINES + 1}):
        status, before, after, _, output = invoke(["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], {**measure, **change})
        if "extracting-realization-lines" in change:
            assert status == 0 and before == after and "WARN: modularity:" in output, "MCLI3 realization lines only warn"
        else:
            assert status == 1 and before == after, "MCLI3 hard rule cannot be reasoned away"
    no_reason = {**held, "crate-budget": {"c": {**held["crate-budget"]["c"], "reason": ""}}}
    status, before, after, _, _ = invoke(["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], baseline=no_reason)
    assert status == 1 and before == after, "MCLI3 original review metadata remains mandatory"
    # A new recorded exception must itself satisfy the original metadata
    # rule. First-write mode does not silently invent a review or lint waiver.
    for argv, expected in ((["--write-baseline"], 1), (["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], 0)):
        status, _, after, _, _ = invoke(argv, baseline=None)
        assert status == expected and ((after is not None) == (expected == 0)), "MCLI3 first review"
    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, observed, _ = invoke(argv, baseline=None)
        assert status == 1 and before is None and after is None and not observed, "MCLI4 missing baseline"
    for argv in (["--check", "--tighten"], ["--self-test", "--write-baseline"], ["--check", "--reason", "x"],
                 ["--write-baseline", "--reason", ""], ["--write-baseline", "--reason", "   "]):
        status, before, after, observed, _ = invoke(argv)
        assert status == 2 and before == after and not observed, f"MCLI4 {argv}"


def main(argv: list[str] | None = None) -> int:
    seen_warnings: set[str] = set()

    def check(measured, previous):
        return _reported_modularity_errors(measured, previous, seen=seen_warnings)

    def tighten_candidate(measured, previous):
        # Show the original line changes before clamping. baseline_cli still
        # validates the candidate through the same hard decision owner.
        check(measured, previous)
        return tightened(measured, previous)

    def write_candidate(measured, previous, reason):
        check(measured, previous)
        return (
            tightened(measured, previous, record=True, reason=reason),
            modularity_errors(measured, tightened(measured, previous)),
        )

    return _architecture_ratchet.baseline_cli(
        argv, prog="_modularity_fitness.py", description=__doc__, baseline_path=BASELINE, self_test=selftest,
        measure=lambda: ([], workspace_measures(_crate_boundary_workspace.CRATES)), check=check,
        tighten=tighten_candidate, write=write_candidate,
    )


if __name__ == "__main__":
    raise SystemExit(main())
