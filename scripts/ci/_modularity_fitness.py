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
  Modules of `awaken-session-executor` that the module-layer guard classifies as
  `realization` are the realization still being extracted (plan slice R). The crate
  budget still holds the whole crate (`lines`, host plus realization), so code
  moved between the two inside the crate is not growth and nothing grows
  unseen; an entry may also hold `host_lines`, the host part alone, which may
  only fall (same slack rule), so moving code out of the host is recorded.
  The extracting realization meets the same review threshold.

A count above its baseline is an error; a count below it is an error until the
baseline is lowered in the same commit, so the file records the real state.
Default/`--check` only reads. `--tighten` validates the clamped candidate and
refuses growth; `--write-baseline --reason` records reviewed growth without
waiving lint, review-metadata or extracting-realization size requirements.
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


def modularity_errors(measures: dict[str, dict], baseline: dict) -> list[str]:
    errors: list[str] = []
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
                errors.append(_architecture_ratchet.diagnostic(
                    "long-function", key, MAX_FUNCTION_LINES, lines,
                    f"split it, or rename its key in {BASELINE.name} if it moved"))
                continue
            seen_long.add(key)
            if lines > held:
                errors.append(_architecture_ratchet.diagnostic("long-function", key, held, lines, "split the function"))
            elif lines < held:
                errors.append(_architecture_ratchet.diagnostic("long-function", key, held, lines, lower))
    budgeted = baseline.get("crate-budget", {})
    for crate, measure in sorted(measures.items()):
        lines = measure.get("lines", 0)
        if lines > CRATE_REVIEW_LINES and crate not in budgeted:
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.lines", CRATE_REVIEW_LINES, lines,
                f"review that it is still one boundary (ADR-0091 D2), then record the review or shrinking work item "
                f"with python3 scripts/ci/{Path(__file__).name} --write-baseline --reason '<review or work item>'"))
    for crate, measure in sorted(measures.items()):
        extracting = measure.get("extracting-realization-lines", 0)
        if extracting > CRATE_REVIEW_LINES:
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.extracting-realization-lines", CRATE_REVIEW_LINES, extracting,
                "review and split the realization boundary to meet the review threshold (ADR-0091 D2)"))
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
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.lines", held.get("lines", "recorded above review threshold"), lines, lower,
                details=f"within review threshold {CRATE_REVIEW_LINES}"))
            continue
        if lines > held["lines"]:
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.lines", held["lines"], lines, "put the new behavior in the crate it is moving to"))
        elif lines <= held["lines"] - BUDGET_LINE_SLACK:
            errors.append(_architecture_ratchet.diagnostic("crate-budget", f"{crate}.lines", held["lines"], lines, lower))
        if "host_lines" in held:
            host = measure.get("host-lines", lines)
            if host > held["host_lines"]:
                errors.append(_architecture_ratchet.diagnostic(
                    "crate-budget", f"{crate}.host_lines", held["host_lines"], host,
                    "put the new behavior in the crate it is moving to"))
            elif host <= held["host_lines"] - BUDGET_LINE_SLACK:
                errors.append(_architecture_ratchet.diagnostic("crate-budget", f"{crate}.host_lines", held["host_lines"], host, lower))
        if deps > held["deps"]:
            errors.append(_architecture_ratchet.diagnostic(
                "crate-budget", f"{crate}.deps", held["deps"], deps, "put the new dependency in the crate it is moving to"))
        elif deps < held["deps"]:
            errors.append(_architecture_ratchet.diagnostic("crate-budget", f"{crate}.deps", held["deps"], deps, lower))
    for key in sorted(set(long_held) - seen_long):
        errors.append(_architecture_ratchet.diagnostic(
            "long-function", key, long_held[key], f"absent or <= {MAX_FUNCTION_LINES}",
            f"{lower} (or rename the key if it moved)"))
    return [f"modularity: {error}" for error in errors]


def tightened(measures: dict[str, dict], baseline: dict, *, record: bool = False, reason: str | None = None) -> dict:
    """The existing candidate writer, shrink-only unless explicitly recording.

    Shrink mode never adds/raises an allowance. The CLI checks its candidate
    with modularity_errors, so a clamp cannot hide growth. Explicit recording
    uses the same fields and preserves metadata; it cannot add lint exemptions
    or excuse the realization-size hard rule.
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
        if lines <= CRATE_REVIEW_LINES:
            continue
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
    | M11  | runtime-host file in a realization / host module; other crate; extracting realization over / at threshold | extracting / host lines; host lines; error / accept |
    | M12  | split crate: whole grows / host grows / host falls >= slack / move host->realization at equal whole; tighten | error / error / error (lower) / accept; host_lines lowered, never raised |

    D3/D4 graph: existing threshold/held/dimension/metadata condition -> the
    same accept/growth/fall/stale/hard result -> explicit transition + owning
    fix. M1-M7 cross spread dimensions, exact counts, pending lint and long
    keys; M8/M10-M12 cross review thresholds, line slack, exact dependencies
    and separate whole/host/extracting scope. MCLI3 crosses those hard rules
    with reasoned/initial writes, so formatting cannot authorize a repair.
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
    assert "split it" in modularity_errors({"c": measure(long={"a.rs::big": 200})}, {})[0], "M7 unrecorded"
    assert "c::a.rs::big 200 -> 201" in modularity_errors({"c": measure(long={"a.rs::big": 201})}, held)[0], "M7 grew"
    assert "c::a.rs::big 200 -> 190" in modularity_errors({"c": measure(long={"a.rs::big": 190})}, held)[0], "M7 shorter"
    assert modularity_errors({"c": measure(long={"a.rs::big": 200})}, held) == [], "M7 equal"
    assert "c::a.rs::big 200 -> absent or <= 150" in modularity_errors({"c": measure()}, held)[0], "M7 gone"
    budget = {"crate-budget": {"c": {"lines": 20000, "deps": 3, "reason": "split"}}}

    def sized(lines, deps):
        return {"c": {**measure(), "lines": lines, "workspace-deps": frozenset(f"d{i}" for i in range(deps))}}

    assert modularity_errors(sized(20000, 3), budget) == [], "M8 equal"
    assert "c.lines 20000 -> 20001" in modularity_errors(sized(20001, 3), budget)[0], "M8 lines grow"
    assert modularity_errors(sized(20000 - BUDGET_LINE_SLACK + 1, 3), budget) == [], "M8 float"
    assert "lower" in modularity_errors(sized(20000 - BUDGET_LINE_SLACK, 3), budget)[0], "M8 fall"
    assert "c.deps 3 -> 4" in modularity_errors(sized(20000, 4), budget)[0], "M8 deps grow"
    assert "c.deps 3 -> 2" in modularity_errors(sized(20000, 2), budget)[0], "M8 deps shrink"
    # M10 review threshold: an unrecorded crate over it is an error; a recorded one at or below it is removed.
    assert "c.lines 15000 -> 15001" in modularity_errors(sized(CRATE_REVIEW_LINES + 1, 3), {})[0], "M10 unrecorded over"
    assert modularity_errors(sized(CRATE_REVIEW_LINES, 3), {}) == [], "M10 unrecorded at threshold"
    assert "c.lines 20000 -> 15000" in modularity_errors(sized(CRATE_REVIEW_LINES, 3), budget)[0], "M10 recorded fell to threshold"
    assert "c" not in tightened(sized(CRATE_REVIEW_LINES, 3), budget)["crate-budget"], "M10 tighten drops it"
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
    assert "c.extracting-realization-lines 15000 -> 15001" in modularity_errors(over, {})[0], "M11 extracting over threshold"
    over["c"]["extracting-realization-lines"] = CRATE_REVIEW_LINES
    assert modularity_errors(over, {}) == [], "M11 extracting at threshold"
    # M12 a crate being split holds its whole size and, separately, its host part.
    split = {"crate-budget": {"c": {"lines": 20000, "host_lines": 15000, "deps": 3, "reason": "split"}}}

    def parts(whole: int, host: int) -> dict:
        return {"c": {**measure(), "lines": whole, "host-lines": host, "extracting-realization-lines": whole - host,
                      "workspace-deps": frozenset({"d0", "d1", "d2"})}}
    assert any("c.lines 20000 -> 20001" in e for e in modularity_errors(parts(20001, 15000), split)), "M12 whole grows"
    assert any("c.host_lines 15000 -> 15001" in e for e in modularity_errors(parts(20000, 15001), split)), "M12 host grows"
    assert any("c.host_lines 15000 -> 14000" in e for e in modularity_errors(parts(20000, 14000), split)), "M12 host falls"
    assert modularity_errors(parts(20000, 14500), split) == [], "M12 move inside the crate is not growth"
    assert tightened(parts(20000, 14000), split)["crate-budget"]["c"]["host_lines"] == 14000, "M12 tighten lowers"
    assert tightened(parts(20000, 15500), split)["crate-budget"]["c"]["host_lines"] == 15000, "M12 never raises"
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
    and rejects any growing metric, including mixed shrink/growth. MCLI2 pure
    shrink writes, equality preserves bytes, owner/expiry/foreign counters and
    their unknown reasons survive.
    MCLI3 explicit growth needs a reason and keeps its history, but a reason
    cannot waive lint, missing review metadata or realization-size hard rules.
    MCLI4 invalid modes/blank reasons/missing baseline precede measurement.
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
        status, before, after, _, _ = invoke(["--tighten"], current)
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
    # Unlike shape's explicit crates={} initial state, an existing empty
    # modularity file is not a global authorization to grow a held metric.
    ordinary = {**measure, "impl-spread": {}, "long-function": {}, "lines": 0, "super-glob": 1}
    status, before, after, _, _ = invoke(["--write-baseline"], ordinary, {})
    assert status == 1 and before == after, "MCLI3 no borrowed initialization waiver"
    for change in ({"unreachable-pub": False}, {"extracting-realization-lines": CRATE_REVIEW_LINES + 1}):
        status, before, after, _, _ = invoke(["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], {**measure, **change})
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
    return _architecture_ratchet.baseline_cli(
        argv, prog="_modularity_fitness.py", description=__doc__, baseline_path=BASELINE, self_test=selftest,
        measure=lambda: ([], workspace_measures(_crate_boundary_workspace.CRATES)), check=modularity_errors,
        tighten=tightened,
        write=lambda measured, previous, reason: (
            tightened(measured, previous, record=True, reason=reason),
            modularity_errors(measured, tightened(measured, previous))),
    )


if __name__ == "__main__":
    raise SystemExit(main())
