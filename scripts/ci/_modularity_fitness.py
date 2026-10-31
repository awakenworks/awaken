"""Module ownership and dependency ratchets, with approved line ceilings.

Plan 9.12 / W22. A module is a boundary when it owns its state and callers
reach it through a narrow surface. Production measurements exclude comments
and provably test-only code. Structural and dependency ratchets stay hard;
line counts remain visible WARN diagnostics and never block a check
(ADR-0091, 2026-10-28). Line ceilings are approved allowances that a review
record may raise (ADR-0091, 2026-10-31).

* `impl-spread`: one type's inherent impls exceed MAX_IMPL_FILES or
  MAX_IMPL_METHODS. Recorded files/methods may only fall and retain their review.
* `module-cycle`: top-level crate/super dependency cycles may only shrink.
* `workspace-lints`: every crate inherits `[workspace.lints]` (or, where it
  must loosen `unsafe_code`, declares every workspace lint itself), and no
  production source `allow`s a lint the compiler now holds for a retired guard
  (`unreachable_pub`, `clippy::wildcard_imports`, `clippy::disallowed_methods`,
  `clippy::disallowed_types`). A pending site states `#[expect(..)]`, which the
  compiler fails once it is clean. Hard, no baseline.
* `crate-line-ceiling`: every crate's approved functional-line ceiling, its
  lines plus headroom when recorded. Silent at or below it; a crate above its
  ceiling, a crate without one and the ceiling of an absent crate each WARN.
* `long-function`: a function above MAX_FUNCTION_LINES holds its approved
  length, without headroom. Growth, a new long function, a shorter one and a
  disappeared one WARN.
* `crate-budget`: the review registry of crates above CRATE_REVIEW_LINES: a
  mandatory review reason and an exact workspace-dependency count, both hard,
  even after the crate falls below the review threshold.

Default/--check is read-only. --tighten only lowers/removes held values, never
adds or raises an allowance. --write-baseline records the tree; any allowance
it adds or raises, including a line ceiling or a long function, needs a
--reason review record, which is appended to the allowance's history. A reason
cannot waive structural, metadata or dependency checks. No other architecture
guard changes its policy through this module.
"""

from __future__ import annotations

import json
import math
import re
import sys
from collections import Counter, defaultdict
from collections.abc import Iterable
from fractions import Fraction
from pathlib import Path

import _crate_boundary_workspace
import _architecture_ratchet
from _context_map_fitness import _flatten_path_tree

BASELINE = Path(__file__).with_name("modularity_baseline.json")
MAX_IMPL_FILES = 5
MAX_IMPL_METHODS = 60
MAX_FUNCTION_LINES = 150
CRATE_REVIEW_LINES = 15000
# A crate's approved line ceiling is its functional lines plus
# max(ratio * lines, floor), rounded up (user decision 2026-10-31).
LINE_HEADROOM_RATIO = Fraction(1, 10)
LINE_HEADROOM_FLOOR = 1000
# crate -> (ratio, floor). The SDK engine stays minimal, so its ceiling sits
# 5% above its size with no fixed floor.
LINE_HEADROOM_OVERRIDES: dict[str, tuple[Fraction, int]] = {"awaken-runtime": (Fraction(1, 20), 0)}
_RECORD = (f"python3 scripts/ci/{Path(__file__).name} --write-baseline --reason "
           "'<JSON review: reason, approved_by, date, remove_when>'")

# An inherent impl may name its type through a crate path (`impl crate::host::ExecutorComponents`);
# that block belongs to the same type, so the path prefix is part of the match.
_IMPL = re.compile(
    r"^\s*impl(?:<[^>{]*>)?\s+(?:crate::(?:[a-z_][a-z0-9_]*::)*)?([A-Z][A-Za-z0-9_]*)(?:<[^>{]*>)?\s*(?:where[^{]*)?\{",
    re.M,
)
_FN = re.compile(r"\bfn\s+[A-Za-z_]\w*")
_FN_ITEM = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", re.M)
_PATH_ROOT = re.compile(r"\b(crate|super)\s*::")
# The lints that replaced Python rules (docs/design/guard-consolidation.md R8,
# R9, R10-R13, R18). Silencing one with `allow` would retire the rule quietly;
# a pending site uses `expect`, which fails once the site is clean.
COMPILER_HELD_LINTS = ("unreachable_pub", "clippy::wildcard_imports", "clippy::disallowed_methods",
                       "clippy::disallowed_types")
_LINT_ALLOW = re.compile(
    r"#!?\[\s*(?:cfg_attr\s*\([^\]]*?,\s*)?allow\s*\(([^)]*)\)", re.S)
WORKSPACE_MANIFEST = _crate_boundary_workspace.REPO_ROOT / "Cargo.toml"


def _block_end(source: str, opening: int) -> int:
    """Index of the brace closing ``source[opening]``, or ``len(source)``."""
    end = _crate_boundary_workspace._balanced_end(source, opening, "{", "}")
    return len(source) if end is None else end - 1


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


def lint_allows(files: dict[str, str]) -> list[str]:
    """`allow` attributes naming a compiler-held lint, as `file:line`."""
    found = []
    for relative, source in sorted(files.items()):
        for match in _LINT_ALLOW.finditer(source):
            named = {part.strip().replace(" ", "") for part in match.group(1).split(",")}
            if named & set(COMPILER_HELD_LINTS):
                found.append(f"{relative}:{source.count(chr(10), 0, match.start()) + 1}")
    return found


def inherits_workspace_lints(manifest: dict, workspace_lints: dict) -> bool:
    """`[lints] workspace = true`, or every workspace lint declared locally.

    A crate that must loosen one lint (`unsafe_code` for an FFI boundary) cannot
    combine `workspace = true` with its own table, so it restates the rest.
    """
    lints = manifest.get("lints", {})
    if lints.get("workspace") is True:
        return True
    return all(set(table) <= set(lints.get(tool, {})) for tool, table in workspace_lints.items())


def workspace_lint_tables(root_manifest: Path = WORKSPACE_MANIFEST) -> dict:
    return _crate_boundary_workspace.load_manifest(root_manifest).get("workspace", {}).get("lints", {})


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


def line_ceiling(crate: str, lines: int) -> int:
    """The ceiling a review approves for `lines` functional lines of `crate`."""
    ratio, floor = LINE_HEADROOM_OVERRIDES.get(crate, (LINE_HEADROOM_RATIO, LINE_HEADROOM_FLOOR))
    return lines + max(math.ceil(ratio * lines), floor)


def _record_or_move(subject: str, owner: str) -> str:
    return (f"if the growth is this {subject}'s own responsibility, record it with {_RECORD}; "
            f"otherwise move the code to the {owner} that owns it")


def crate_measures(manifest_path: Path) -> dict:
    root = manifest_path.parent / "src"
    files: dict[str, str] = {}
    for path in _crate_boundary_workspace.production_module_files([manifest_path]):
        path = Path(path)
        if root in path.parents:
            relative = path.relative_to(root).as_posix()
            files[relative] = _crate_boundary_workspace.production_rust(path.read_text(encoding="utf-8"))
    top = {_module_of(relative)[0] for relative in files if _module_of(relative)}
    manifest = _crate_boundary_workspace.load_manifest(manifest_path)
    layer = manifest.get("package", {}).get("metadata", {}).get("awaken", {}).get("layer", "")
    return {
        "lines": sum(1 for source in files.values() for line in source.splitlines() if line.strip()),
        "deps": _crate_boundary_workspace.normal_dependency_names(manifest),
        "long-function": {} if layer == "tooling" else long_functions(files),
        "impl-spread": impl_spread(files),
        "module-cycle": largest_module_cycle(files, top),
        "workspace-lints": inherits_workspace_lints(manifest, workspace_lint_tables()),
        "lint-allows": lint_allows(files),
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
    held_cycles: dict[str, int] = baseline.get("module-cycle", {})
    for crate in sorted(set(measures) | set(held_cycles)):
        now, was = measures.get(crate, {}).get("module-cycle", 0), held_cycles.get(crate, 0)
        if now > was:
            errors.append(_architecture_ratchet.diagnostic(
                "module-cycle", crate, was, now, "reach the other module through a port, or move the shared part down"))
        elif now < was:
            errors.append(_architecture_ratchet.diagnostic("module-cycle", crate, was, now, lower))
    for crate, measure in sorted(measures.items()):
        if not measure["workspace-lints"]:
            errors.append(_architecture_ratchet.diagnostic(
                "workspace-lints", crate, "[lints] workspace = true", "missing",
                "inherit the workspace lints in its Cargo.toml"))
        for site in measure["lint-allows"]:
            errors.append(_architecture_ratchet.diagnostic(
                "workspace-lints", f"{crate}::{site}", "expect", "allow",
                "state a pending site with #[expect(..)] so the compiler fails it once clean"))
    long_held: dict[str, int] = baseline.get("long-function", {})
    function_growth = _record_or_move("function", "component")
    seen_long: set[str] = set()
    for crate, measure in sorted(measures.items()):
        for function, lines in sorted(measure.get("long-function", {}).items()):
            key = f"{crate}::{function}"
            held = long_held.get(key)
            if held is None:
                line_warnings.append(_architecture_ratchet.diagnostic(
                    "long-function", key, MAX_FUNCTION_LINES, lines, function_growth))
                continue
            seen_long.add(key)
            if lines > held:
                line_warnings.append(_architecture_ratchet.diagnostic("long-function", key, held, lines, function_growth))
            elif lines < held:
                line_warnings.append(_architecture_ratchet.diagnostic("long-function", key, held, lines, line_lower))
    for key in sorted(set(long_held) - seen_long):
        line_warnings.append(_architecture_ratchet.diagnostic(
            "long-function", key, long_held[key], f"absent or <= {MAX_FUNCTION_LINES}", line_lower))
    ceilings: dict[str, int] = baseline.get("crate-line-ceiling", {})
    for crate, measure in sorted(measures.items()):
        lines, ceiling = measure.get("lines", 0), ceilings.get(crate)
        if ceiling is None:
            line_warnings.append(_architecture_ratchet.diagnostic(
                "crate-line-ceiling", crate, "unrecorded", lines,
                f"record the new crate's ceiling ({line_ceiling(crate, lines)}) with {_RECORD}"))
        elif lines > ceiling:
            line_warnings.append(_architecture_ratchet.diagnostic(
                "crate-line-ceiling", f"{crate}.lines", ceiling, lines, _record_or_move("crate", "crate")))
    for crate in sorted(set(ceilings) - set(measures)):
        line_warnings.append(_architecture_ratchet.diagnostic(
            "crate-line-ceiling", crate, "recorded crate", "absent", f"{Path(__file__).name} --tighten removes it"))
    budgeted = baseline.get("crate-budget", {})
    for crate, measure in sorted(measures.items()):
        lines = measure.get("lines", 0)
        if lines > CRATE_REVIEW_LINES and crate not in budgeted:
            line_warnings.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.lines", CRATE_REVIEW_LINES, lines,
                f"register the crate's review reason and exact dependencies with {_RECORD}"))
    for crate, held in sorted(budgeted.items()):
        measure = measures.get(crate)
        if measure is None:
            errors.append(_architecture_ratchet.diagnostic("crate-budget", crate, "recorded crate", "absent", lower))
            continue
        if not str(held.get("reason", "")).strip():
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.reason", "reviewed split", "missing",
                f"restore the owning reviewed split in {BASELINE.name} crate-budget[{crate}].reason"))
        deps = len(measure.get("workspace-deps", ()))
        if deps > held["deps"]:
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.deps", held["deps"], deps, "put the new dependency in the crate it is moving to"))
        elif deps < held["deps"]:
            errors.append(_architecture_ratchet.diagnostic("crate-budget", f"{crate}.deps", held["deps"], deps, lower))
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


def recorded_line_growth(measures: dict[str, dict], baseline: dict) -> list[str]:
    """Line allowances `--write-baseline` would add or raise; each needs a review record."""
    ceilings, long_held = baseline.get("crate-line-ceiling", {}), baseline.get("long-function", {})
    found = [f"crate-line-ceiling: {crate} {ceilings.get(crate, 'unrecorded')} -> {line_ceiling(crate, measure.get('lines', 0))}"
             for crate, measure in sorted(measures.items())
             if crate not in ceilings or measure.get("lines", 0) > ceilings[crate]]
    for crate, measure in sorted(measures.items()):
        for function, lines in sorted(measure.get("long-function", {}).items()):
            key = f"{crate}::{function}"
            held = long_held.get(key)
            if held is None or lines > held:
                found.append(f"long-function: {key} {MAX_FUNCTION_LINES if held is None else held} -> {lines}")
    return found


def tightened(measures: dict[str, dict], baseline: dict, *, record: bool = False, reason: str | None = None) -> dict:
    """The candidate writer, shrink-only unless explicitly recording.

    Shrink mode never adds/raises an allowance; it lowers a line ceiling to
    lines plus headroom and drops absent crates. Recording also raises an
    over-ceiling crate and adds a new one at lines plus headroom, and appends
    the reason to every allowance it adds or raises. The CLI checks its
    candidate with the same decision owner, so a clamp cannot hide structural
    or dependency growth; recording cannot add lint exemptions or waive
    required review metadata.
    """
    out = json.loads(json.dumps(baseline))
    reasons = out.pop("growth_reasons", {})
    owned = {"impl-spread", "module-cycle", "long-function", "crate-line-ceiling", "crate-budget"}
    kept_reasons: dict[str, dict[str, str]] = {name: value for name, value in reasons.items() if name not in owned}

    def count(rule: str, key: str, now: int, was: int) -> int:
        earlier = kept_reasons.get(rule, {}).get(key, reasons.get(rule, {}).get(key))
        if record and now > was and reason:
            earlier = _architecture_ratchet.growth_reason(earlier, reason)
        if earlier:
            kept_reasons.setdefault(rule, {})[key] = earlier
        return now if record else min(now, was)

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
    held = baseline.get("module-cycle", {})
    keys = set(held) | (set(measures) if record else set())
    out["module-cycle"] = {
        crate: count("module-cycle", crate, measures[crate].get("module-cycle", 0), held.get(crate, 0))
        for crate in sorted(keys) if crate in measures and measures[crate].get("module-cycle", 0) > 0}
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
    held_ceilings = baseline.get("crate-line-ceiling", {})
    ceilings = {}
    for crate in sorted(set(measures) if record else set(held_ceilings) & set(measures)):
        lines, held = measures[crate].get("lines", 0), held_ceilings.get(crate)
        approved = line_ceiling(crate, lines)
        raised = record and (held is None or lines > held)
        ceilings[crate] = approved if raised else min(held, approved)
        earlier = reasons.get("crate-line-ceiling", {}).get(crate)
        if raised and reason:
            earlier = _architecture_ratchet.growth_reason(earlier, reason)
        if earlier:
            kept_reasons.setdefault("crate-line-ceiling", {})[crate] = earlier
    out["crate-line-ceiling"] = ceilings
    held_budget = baseline.get("crate-budget", {})
    keys = set(held_budget) | (set(measures) if record else set())
    budget = {}
    for crate in sorted(keys):
        measure = measures.get(crate)
        if measure is None or (measure.get("lines", 0) <= CRATE_REVIEW_LINES and crate not in held_budget):
            continue
        # A line-threshold crossing must not delete the held dependency ceiling.
        held = held_budget.get(crate, {"reason": reason or "", "deps": 0})
        budget[crate] = {"deps": count("crate-budget", crate, len(measure.get("workspace-deps", ())), held["deps"]),
                         "reason": held.get("reason", "")}
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
    | M4   | (retired: `use super::*` is clippy::wildcard_imports, R9)      | - |
    | M5   | `crate::a` <-> `super::b` paths close a cycle of top modules | cycle size counted |
    | M6   | crate inherits workspace lints / restates them all / misses one | accept / accept / error |
    | M6   | production `allow` of a compiler-held lint / `expect` / other lint | error / accept / accept |
    | M7   | function over the limit: unrecorded / grew / shorter / equal / gone or under | WARN record-or-move / WARN record-or-move / WARN lower / accept / WARN lower |
    | M8   | crate lines below / at / one above its ceiling                | silent / silent / WARN record-or-move |
    | M8   | measured crate without a ceiling / ceiling of an absent crate | WARN record new / WARN tighten removes, never an error |
    | H1   | default crate: 10% of lines above / below / at the floor       | ceiling = lines + ceil(10%) / lines + 1000 / lines + 1000 |
    | H2   | awaken-runtime: 5%, no floor; fractional percent               | ceiling = lines + ceil(5%); rounded up |
    | M9   | --tighten: ceiling above / at / below lines + headroom; unrecorded; absent | lowered / kept / kept (never raised) / not added / dropped |
    | M10  | registry: unregistered crate over / at review threshold         | WARN register / silent |
    | M10  | registered crate: deps grow / shrink / equal; reason missing; absent | error / error (lower) / accept; error; error |
    | M13  | lines under / at / over ceiling or threshold x deps grow / shrink / equal | deps error / error / accept, independent of lines |
    | W1   | record: crate over ceiling / new crate / under ceiling          | raised to lines + headroom / added at lines + headroom / lowered like tighten |
    | W2   | record: long function grew / new / shorter                      | raised to length / added / lowered (no headroom) |
    | W3   | record with reason: raised or added allowance / unchanged one   | reason appended to its history / history kept unchanged |
    | W4   | line allowance a record would add or raise / none               | listed as growth needing --reason / empty |

    D3/D4 graph: existing threshold/held/dimension/metadata condition -> the
    same structural hard result or line-only warning -> owning diagnostic.
    M1-M7 retain spread dimensions, exact counts, workspace lints and long keys;
    M8/H1/H2 decide a crate's approved ceiling; M10/M13 keep the review
    registry and its exact dependencies hard whatever the lines; W1-W4 make
    every raised line allowance carry a review record. MCLI3 retains hard
    admission for structural changes and metadata; no line ceiling becomes
    authorization to grow deps.
    """
    spread = impl_spread({"a.rs": "impl Big { fn x(&self) {} fn y(&self) {} }", "b.rs": "impl<T> Big { fn z() {} }"})
    assert spread == {"Big": (2, 3)}, "impl-spread counting"
    pathed = impl_spread({"a.rs": "impl crate::host::Big { fn x(&self) {} }", "b.rs": "impl crate::Big for X { fn y() {} }"})
    assert pathed == {"Big": (1, 1)}, "impl-spread counts a crate-path inherent impl, not a trait impl"

    def measure(spread=None, cycle=0, lints=True, allows=None, long=None):
        return {"impl-spread": spread or {}, "module-cycle": cycle, "workspace-lints": lints,
                "lint-allows": allows or [], "long-function": long or {}}

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
    cycle = largest_module_cycle(
        {"a.rs": "use crate::b::X;", "b/mod.rs": "use super::a::Y;", "c.rs": "use crate::a::Z;"}, {"a", "b", "c"}
    )
    assert cycle == 2, "M5"
    workspace = {"rust": {"unsafe_code": "forbid", "unreachable_pub": "warn"}, "clippy": {"wildcard_imports": "warn"}}
    assert inherits_workspace_lints({"lints": {"workspace": True}}, workspace), "M6 inherits"
    assert inherits_workspace_lints({"lints": {"rust": {"unsafe_code": "allow", "unreachable_pub": "warn"},
                                               "clippy": {"wildcard_imports": "warn"}}}, workspace), "M6 restates"
    assert not inherits_workspace_lints({"lints": {"rust": {"unsafe_code": "allow"}}}, workspace), "M6 misses one"
    assert not inherits_workspace_lints({}, workspace), "M6 no lints"
    assert "workspace-lints: c [lints] workspace = true -> missing" in modularity_errors(
        {"c": measure(lints=False)}, {})[0], "M6 missing"
    allowed = lint_allows({
        "a.rs": "#![allow(unreachable_pub)]\n#[allow(dead_code, clippy::wildcard_imports)]\nuse super::*;\n",
        "b.rs": "#[cfg_attr(not(test), allow(clippy::disallowed_methods))]\nfn f() {}\n",
        "c.rs": "#[cfg_attr(not(test), expect(clippy::wildcard_imports))]\nuse super::*;\n"
                "#![expect(unreachable_pub)]\n#[allow(dead_code)]\nfn g() {}\n",
    })
    assert allowed == ["a.rs:1", "a.rs:2", "b.rs:1"], ("M6 allow / expect / other lint", allowed)
    assert "c::a.rs:1 expect -> allow" in modularity_errors({"c": measure(allows=["a.rs:1"])}, {})[0], "M6 allow"
    body = "fn big() {\n" + "    let x = \"{\";\n" * (MAX_FUNCTION_LINES + 1) + "}\nfn small() {\n}\n"
    assert long_functions({"a.rs": body}) == {"a.rs::big": MAX_FUNCTION_LINES + 2}, "M7 measure (braces in literals)"
    held = {"long-function": {"c::a.rs::big": 200}, "crate-line-ceiling": {"c": 1000}}
    unrecorded = line_findings({"c": measure(long={"a.rs::big": 200})}, {})[0]
    assert "c::a.rs::big 150 -> 200" in unrecorded and "--write-baseline --reason" in unrecorded \
        and "move the code" in unrecorded, "M7 unrecorded WARN record-or-move"
    grew = line_findings({"c": measure(long={"a.rs::big": 201})}, held)[0]
    assert "c::a.rs::big 200 -> 201" in grew and "--write-baseline --reason" in grew, "M7 grew WARN record-or-move"
    assert "--tighten" in line_findings({"c": measure(long={"a.rs::big": 190})}, held)[0], "M7 shorter WARN lower"
    assert line_findings({"c": measure(long={"a.rs::big": 200})}, held) == [], "M7 equal"
    assert "c::a.rs::big 200 -> absent or <= 150" in line_findings({"c": measure()}, held)[0], "M7 gone WARN"

    # H1/H2: ceiling = lines + max(ratio * lines, floor), rounded up.
    for crate, lines, ceiling in (
            ("c", 20000, 22000), ("c", 20001, 22002), ("c", 5000, 6000), ("c", 10000, 11000), ("c", 0, 1000),
            ("awaken-runtime", 9256, 9719), ("awaken-runtime", 100, 105), ("awaken-runtime", 21, 23),
            ("awaken-runtime", 0, 0)):
        assert line_ceiling(crate, lines) == ceiling, f"H1/H2 {crate} {lines} -> {ceiling}"
    assert set(LINE_HEADROOM_OVERRIDES) == {"awaken-runtime"}, "H2 one override table, one minimal crate"

    def sized(lines, deps=3, crate="c"):
        return {crate: {**measure(), "lines": lines, "workspace-deps": frozenset(f"d{i}" for i in range(deps))}}

    # M8 crate line ceilings decide only warnings, never errors.
    ceiling = {"crate-line-ceiling": {"c": 12000}}
    assert line_findings(sized(11999), ceiling) == [], "M8 below ceiling silent"
    assert line_findings(sized(12000), ceiling) == [], "M8 at ceiling silent"
    over = line_findings(sized(12001), ceiling)
    assert len(over) == 1 and "crate-line-ceiling: c.lines 12000 -> 12001" in over[0] \
        and "--write-baseline --reason" in over[0] and "move the code to the crate that owns it" in over[0], \
        "M8 over ceiling WARN record-or-move"
    new = line_findings(sized(500), {})
    assert len(new) == 1 and "crate-line-ceiling: c unrecorded -> 500" in new[0] and "(1500)" in new[0] \
        and "--write-baseline --reason" in new[0], "M8 new crate WARN record"
    gone = line_findings({}, ceiling)
    assert len(gone) == 1 and "c recorded crate -> absent" in gone[0] and "--tighten" in gone[0], \
        "M8 absent crate WARN, not an error"

    # M10 the crate-budget review registry: reason and exact deps stay hard.
    budget = {"crate-line-ceiling": {"c": 30000}, "crate-budget": {"c": {"deps": 3, "reason": "split"}}}
    unregistered = line_findings(sized(CRATE_REVIEW_LINES + 1), {"crate-line-ceiling": {"c": 30000}})
    assert len(unregistered) == 1 and "crate-budget: c.lines 15000 -> 15001" in unregistered[0], "M10 register WARN"
    assert line_findings(sized(CRATE_REVIEW_LINES), {"crate-line-ceiling": {"c": 30000}}) == [], "M10 at threshold"
    assert line_findings(sized(20000), budget) == [], "M10 registered equal deps"
    assert "c.deps 3 -> 4" in modularity_errors(sized(20000, 4), budget)[0], "M10 deps grow"
    assert "c.deps 3 -> 2" in modularity_errors(sized(20000, 2), budget)[0], "M10 deps shrink"
    assert "recorded crate -> absent" in modularity_errors({}, budget)[0], "M10 absent registered crate stays hard"
    no_reason = {**budget, "crate-budget": {"c": {"deps": 3, "reason": ""}}}
    assert any("c.reason" in error for error in modularity_errors(sized(CRATE_REVIEW_LINES - 1), no_reason)), \
        "M10 missing review reason stays hard below the threshold"
    # M13 the deps decision is independent of every line row: within, at and
    # over the ceiling and below the review threshold. No check mutates the baseline.
    for lines in (30001, 30000, 20000, CRATE_REVIEW_LINES, CRATE_REVIEW_LINES - 1):
        for deps in (2, 3, 4):
            warnings: list[str] = []
            before = json.dumps(budget, sort_keys=True)
            errors = modularity_errors(sized(lines, deps), budget, warnings=warnings)
            assert bool(warnings) == (lines > 30000), f"M13 lines warn only above the ceiling {lines}"
            assert bool(errors) == (deps != 3), f"M13 independent deps {lines}/{deps}"
            if deps != 3:
                assert any(f"c.deps 3 -> {deps}" in error for error in errors), "M13 exact deps stay hard"
            assert json.dumps(budget, sort_keys=True) == before, "M13 pure decision is read-only"

    # M9 `--tighten`: lowers or removes held entries, never adds or raises one.
    held = {"impl-spread": {"c::T": {"files": 10, "methods": 100, "reason": "r"}, "c::Gone": {"files": 9,
            "methods": 90, "reason": "r"}}, "module-cycle": {"c": 2},
            "long-function": {"c::a.rs::big": 200, "c::a.rs::gone": 300},
            "crate-line-ceiling": {"c": 25000, "at": 6000, "over": 6000, "under": 1000, "gone": 7000},
            "crate-budget": {"c": {"deps": 3, "reason": "split"}},
            "growth_reasons": {"crate-line-ceiling": {"c": "first", "gone": "old"}}}
    now = {"c": {**measure({"T": (8, 120)}, cycle=4, long={"a.rs::big": 180, "a.rs::new": 170}),
                 "lines": 20000, "workspace-deps": frozenset({"d0", "d1"})},
           "at": {**measure(), "lines": 5000, "workspace-deps": frozenset()},
           "over": {**measure(), "lines": 9000, "workspace-deps": frozenset()},
           "under": {**measure(), "lines": 100, "workspace-deps": frozenset()},
           "new": {**measure(), "lines": 100, "workspace-deps": frozenset()}}
    tight = tightened(now, held)
    assert tight["impl-spread"] == {"c::T": {"files": 8, "methods": 100, "reason": "r"}}, "M9 spread"
    assert tight["module-cycle"] == {"c": 2}, "M9 counts never rise"
    assert tight["long-function"] == {"c::a.rs::big": 180}, "M9 long-function lowers, never adds"
    assert tight["crate-line-ceiling"] == {"c": 22000, "at": 6000, "over": 6000, "under": 1000}, \
        "M9 ceiling lowered to lines + headroom / kept at it / never raised / never added / absent dropped"
    assert tight["growth_reasons"] == {"crate-line-ceiling": {"c": "first"}}, "M9 absent crate's history dropped"
    assert tight["crate-budget"]["c"] == {"deps": 2, "reason": "split"}, "M9 budget"
    assert modularity_errors({**now, "c": {**now["c"], "impl-spread": {"T": (8, 100)}, "module-cycle": 2}}, tight) == [], \
        "M9 a tightened baseline holds"
    # W1-W4 explicit recording raises or adds a line allowance only with its review.
    review = _architecture_ratchet._test_growth_reason("reviewed")
    growth = recorded_line_growth(now, held)
    assert growth == ["crate-line-ceiling: new unrecorded -> 1100", "crate-line-ceiling: over 6000 -> 10000",
                      "long-function: c::a.rs::new 150 -> 170"], "W4 listed growth"
    assert recorded_line_growth({"c": now["c"]}, tight | {"long-function": {"c::a.rs::big": 180, "c::a.rs::new": 170}}) \
        == [], "W4 nothing to raise"
    written = tightened(now, held, record=True, reason=review)
    assert written["crate-line-ceiling"] == {"c": 22000, "at": 6000, "over": 10000, "under": 1000, "new": 1100}, \
        "W1 raised / added at lines + headroom, under-ceiling lowered like tighten"
    assert written["long-function"] == {"c::a.rs::big": 180, "c::a.rs::new": 170}, "W2 exact lengths, no headroom"
    raised = {**held, "long-function": {"c::a.rs::big": 170}}
    assert tightened(now, raised, record=True, reason=review)["long-function"]["c::a.rs::big"] == 180, "W2 raised"
    assert tightened(now, raised, record=True, reason=review)["growth_reasons"]["long-function"]["c::a.rs::big"] == review, \
        "W3 long-function reason recorded"
    assert written["growth_reasons"]["crate-line-ceiling"] == {"c": "first", "over": review, "new": review}, \
        "W3 reason for raised/added only; unchanged history kept"
    again = tightened({**now, "over": {**now["over"], "lines": 10001}}, written, record=True, reason=review.replace("reviewed", "again"))
    assert again["growth_reasons"]["crate-line-ceiling"]["over"] == review + "; then: " + review.replace("reviewed", "again"), \
        "W3 reason history appended"
    assert again["crate-line-ceiling"]["over"] == 10001 + 1001, "W1 raised again"
    _selftest_cli()
    print("_modularity_fitness selftest: the decision tables hold")


def _selftest_cli() -> None:
    """CLI causes/effects over the M1-M13 and W1-W4 decision tables.

    MCLI1 default/check are read-only; tighten validates its clamped candidate
    and rejects structural/dependency growth, including mixed shrink/growth;
    line-only growth WARNs and accepts without raising a ceiling. MCLI2 pure
    shrink writes, equality preserves bytes, owner/expiry/foreign counters and
    their unknown reasons survive.
    MCLI3 explicit growth needs a reason and keeps its history, including a
    raised crate ceiling or long function, but a reason cannot waive lint or
    missing review metadata.
    MCLI4 invalid modes/blank reasons/missing baseline precede measurement.
    MCLI5 over-ceiling and long-function rows display WARN in the CLI and the
    aggregate check_all; within-ceiling rows are silent; baseline bytes stay
    read-only and no line row waives deps.
    """
    review = _architecture_ratchet._test_growth_reason("reviewed")
    measure = {"impl-spread": {"T": (8, 80)}, "module-cycle": 2, "workspace-lints": True, "lint-allows": [],
               "long-function": {"a.rs::big": 200}, "lines": 20000, "workspace-deps": frozenset({"a", "b"})}
    foreign = {"future-counter": {"k": "  foreign review  "}}
    held = {"owner": "modularity", "expiry": "split", "future-counter": {"k": 9}, "growth_reasons": foreign, "impl-spread": {"c::T": {"files": 8, "methods": 80,
            "reason": "extract component"}}, "module-cycle": {"c": 2},
            "long-function": {"c::a.rs::big": 200},
            "crate-line-ceiling": {"c": 22000}, "crate-budget": {"c": {"deps": 2, "reason": "split c"}}}

    def invoke(argv, current=measure, baseline=held):
        return _architecture_ratchet._cli_fixture(main, argv, baseline, "workspace_measures", {"c": current})

    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, observed, output = invoke(argv)
        assert status == 0 and before == after and observed and "WARN" not in output, f"MCLI1/MCLI2 equal {argv}"
    rows = ({"impl-spread": {"T": (8, 81)}}, {"impl-spread": {"T": (8, 80), "New": (6, 10)}},
            {"module-cycle": 3}, {"long-function": {"a.rs::big": 201}},
            {"long-function": {"new.rs::big": 180}}, {"lines": 22001},
            {"workspace-deps": frozenset({"a", "b", "new"})})
    for change in rows:
        # One growing field while module-cycle shrinks: a successful min() must
        # not be mistaken for a successful check of the actual tree.
        current = {**measure, "module-cycle": 1, **change}
        status, before, after, _, output = invoke(["--tighten"], current)
        if set(change) <= {"long-function", "lines"}:
            assert status == 0 and "WARN: modularity:" in output, f"MCLI1 line-only WARN {change}"
            document = json.loads(after)
            assert document["module-cycle"] == {"c": 1}, "MCLI1 structural shrink still records"
            assert document["crate-line-ceiling"] == held["crate-line-ceiling"], "MCLI1 tighten never raises a ceiling"
            assert all(key in held["long-function"] and value <= held["long-function"][key]
                       for key, value in document["long-function"].items()), "MCLI1 tighten never adds/raises long keys"
        else:
            assert status == 1 and before == after, f"MCLI1 growth {change}"
    smaller = {**measure, "module-cycle": 1, "lines": 19000}
    status, _, after, _, _ = invoke(["--tighten"], smaller)
    document = json.loads(after)
    assert status == 0 and document["module-cycle"] == {"c": 1}, "MCLI2 shrink"
    assert document["crate-line-ceiling"] == {"c": 20900}, "MCLI2 ceiling lowered to lines + headroom"
    assert document["crate-budget"] == held["crate-budget"], "MCLI2 held review reason and deps"
    assert document["owner"] == held["owner"] and document["expiry"] == held["expiry"], "MCLI2 metadata"
    assert document["future-counter"] == held["future-counter"] and document["growth_reasons"] == foreign, \
        "MCLI2 foreign metadata/reason retained"
    current = {**measure, "module-cycle": 3, "impl-spread": {"T": (9, 81)}}
    status, before, after, _, _ = invoke(["--write-baseline"], current)
    assert status == 1 and before == after, "MCLI3 unreasoned growth"
    prior = {**held, "growth_reasons": {**foreign, "impl-spread": {"c::T": "first"}}}
    status, _, after, _, _ = invoke(["--write-baseline", "--reason", review], current, prior)
    document = json.loads(after)
    assert status == 0 and document["impl-spread"]["c::T"] == {"files": 9, "methods": 81,
                                                              "reason": "extract component"}, "MCLI3 exact facts"
    assert document["growth_reasons"] == {**foreign, "impl-spread": {"c::T": "first; then: " + review},
                                          "module-cycle": {"c": review}}, "MCLI3 reason history"
    assert document["future-counter"] == held["future-counter"], "MCLI3 foreign counter retained"
    # MCLI3 line allowances: a raise or a new one needs a reason, which joins its history.
    for change, rule, key, value in (({"lines": 22001}, "crate-line-ceiling", "c", 24202),
                                     ({"long-function": {"a.rs::big": 201}}, "long-function", "c::a.rs::big", 201),
                                     ({"long-function": {"a.rs::big": 200, "new.rs::big": 180}}, "long-function",
                                      "c::new.rs::big", 180)):
        status, before, after, _, output = invoke(["--write-baseline"], {**measure, **change})
        assert status == 1 and before == after, f"MCLI3 unreasoned line growth {change}"
        status, _, after, _, _ = invoke(["--write-baseline", "--reason", review], {**measure, **change})
        document = json.loads(after)
        assert status == 0 and document[rule][key] == value, f"MCLI3 reasoned line growth {change}"
        assert document["growth_reasons"] == {**foreign, rule: {key: review}}, f"MCLI3 line reason {change}"
    status, _, after, _, _ = invoke(["--write-baseline"], {**measure, "lines": 19000})
    assert status == 0 and json.loads(after)["crate-line-ceiling"] == {"c": 20900}, "MCLI3 lowering needs no reason"
    # MCLI5: line rows stay read-only in default and --check; over-ceiling and
    # long-function rows WARN, rows within the ceiling are silent.
    line_rows = (
        ({"long-function": {"a.rs::big": 201}}, True), ({"long-function": {"a.rs::big": 190}}, True),
        ({"long-function": {"new.rs::big": 180, "a.rs::big": 200}}, True), ({"long-function": {}}, True),
        ({"lines": 22001}, True), ({"lines": 22000}, False), ({"lines": 19000}, False),
        ({"lines": CRATE_REVIEW_LINES}, False),
    )
    for change, warns in line_rows:
        for argv in ([], ["--check"]):
            status, before, after, observed, output = invoke(argv, {**measure, **change})
            assert status == 0 and before == after and observed, f"MCLI5 read-only line row {change}/{argv}"
            assert ("WARN: modularity:" in output) == warns, f"MCLI5 CLI diagnostic {change}"
    for lines in (22001, 19000, CRATE_REVIEW_LINES, CRATE_REVIEW_LINES - 1):
        for argv in ([], ["--check"], ["--tighten"]):
            current = {**measure, "lines": lines, "workspace-deps": frozenset({"a", "b", "new"})}
            status, before, after, _, output = invoke(argv, current)
            assert status == 1 and before == after, f"MCLI5 line+dependency growth stays hard {lines}/{argv}"
            assert ("WARN: modularity:" in output) == (lines > 22000), f"MCLI5 line diagnostic {lines}/{argv}"
    below = {**measure, "lines": CRATE_REVIEW_LINES - 1}
    status, _, after, _, output = invoke(["--tighten"], below)
    assert status == 0
    document = json.loads(after)
    assert document["crate-budget"]["c"] == {"deps": 2, "reason": "split c"}, \
        "MCLI5 threshold shrink preserves original dependency review"
    assert document["crate-line-ceiling"] == {"c": line_ceiling("c", below["lines"])}, "MCLI5 explicit tighten only lowers"

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
        for lines, deps, warns in ((22001, {"a", "b"}, True), (CRATE_REVIEW_LINES - 1, {"a", "b", "new"}, False),
                                   (22001, {"a", "b", "new"}, True)):
            output = io.StringIO()
            current = {**measure, "lines": lines, "workspace-deps": frozenset(deps)}
            with patch.dict(check_all.__globals__, {"workspace_measures": lambda _root: {"c": current}}), \
                    contextlib.redirect_stdout(output):
                errors = check_all(Path(temporary) / "crates", baseline_path)
            assert ("WARN: modularity:" in output.getvalue()) == warns, f"MCLI5 aggregate WARN {lines}"
            assert bool(errors) == (len(deps) != 2), "MCLI5 aggregate line row cannot hide dependency growth"
            if errors:
                assert any("c.deps 2 -> 3" in error for error in errors)
            assert baseline_path.read_bytes() == before, "MCLI5 aggregate does not rewrite baseline"

    # Unlike shape's explicit crates={} initial state, an existing empty
    # modularity file is not a global authorization to grow a held metric.
    ordinary = {**measure, "impl-spread": {}, "long-function": {}, "lines": 0}
    status, before, after, _, _ = invoke(["--write-baseline"], ordinary, {})
    assert status == 1 and before == after, "MCLI3 no borrowed initialization waiver"
    status, before, after, _, _ = invoke(["--write-baseline", "--reason", review], {**measure, "workspace-lints": False})
    assert status == 1 and before == after, "MCLI3 hard rule cannot be reasoned away"
    no_reason = {**held, "crate-budget": {"c": {**held["crate-budget"]["c"], "reason": ""}}}
    status, before, after, _, _ = invoke(["--write-baseline", "--reason", review], baseline=no_reason)
    assert status == 1 and before == after, "MCLI3 original review metadata remains mandatory"
    # A new recorded exception must itself satisfy the original metadata
    # rule. First-write mode does not silently invent a review or lint waiver.
    for argv, expected in ((["--write-baseline"], 1), (["--write-baseline", "--reason", review], 0)):
        status, _, after, _, _ = invoke(argv, baseline=None)
        assert status == expected and ((after is not None) == (expected == 0)), "MCLI3 first review"
        if after is not None:
            assert json.loads(after)["crate-line-ceiling"] == {"c": 22000}, "MCLI3 first review records the ceiling"
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
            modularity_errors(measured, tightened(measured, previous)) + recorded_line_growth(measured, previous),
        )

    return _architecture_ratchet.baseline_cli(
        argv, prog="_modularity_fitness.py", description=__doc__, baseline_path=BASELINE, self_test=selftest,
        measure=lambda: ([], workspace_measures(_crate_boundary_workspace.CRATES)), check=check,
        tighten=tighten_candidate, write=write_candidate,
    )


if __name__ == "__main__":
    raise SystemExit(main())
