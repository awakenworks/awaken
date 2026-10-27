"""Future crate boundaries inside `awaken-session-executor` hold before the crate is split.

Plan 10.2 / 10.4: the realization cluster leaves `awaken-session-executor` for its
own crate (`awaken-session-realization`) and the host keeps the rest. That split
is a pure move only if the realization modules never reach up into host
modules. This guard makes the boundary checkable now, so the slices that
reshape the host in between cannot regress it.

Every module of the crate is classified as `realization` or `host` in `LAYERS`,
next to the reason it is placed there. A module inherits the target of its
nearest classified ancestor, except below a *mixed* module (one with
descendants of both targets): each child of a mixed module, and each
top-level module, must be listed itself. An unlisted one is an error, so a new
module states which side it lands on (fail closed).

Counted from production code (comments and `cfg(test)` code are not scanned):

* `upward-edge`: a `crate::`, `super::` or `use` path in a `realization` module
  that names an item of a `host` module. Recorded per (from-module,
  to-module) pair as the set of host items named; a new pair or a new item is
  an error, a recorded item that is no longer named is an error until it is
  removed from `module_layer_baseline.json` in the same commit.
* `shared-host-impl`: methods in `impl ... ExecutorComponents` blocks inside a
  `realization` module. `ExecutorComponents` lives in the host, so such a block is an
  upward edge by construction; it is expected to shrink when the realization
  state gets its own owner, and is held per module at or below the baseline
  instead of being reported item by item. Paths inside those blocks are not
  counted as `upward-edge`.

When an edge or a method goes, lower the baseline with
`python3 scripts/ci/_module_layer_fitness.py --tighten`; it only removes or
lowers held entries, never adds or raises one; growth is refused, not silently
left in the tree. Default/`--check` is read-only checking. `--report` preserves
the JSON measurement view; explicit `--write-baseline --reason` records reviewed
growth but cannot waive classification rules.
"""

from __future__ import annotations

import json
import re
from collections import defaultdict
from pathlib import Path

import _crate_boundary_workspace
import _architecture_ratchet
from _context_map_fitness import _flatten_path_tree

BASELINE = Path(__file__).with_name("module_layer_baseline.json")
CRATE = "awaken-session-executor"
REALIZATION = "realization"
HOST = "host"

_REALIZATION_REASON = "realization cluster: leaves the host for awaken-session-realization (plan 10.2)"
_LEAF_REASON = "leaf value types moved out of installed_session; go with the realization (plan 10.1 item 1)"
_MIXED_REASON = "mixed: its own items stay in the host, each child is classified below"

# (target, reason, modules). Module paths are `::`-joined from the crate root
# and follow `#[path]` attributes.
LAYERS: tuple[tuple[str, str, tuple[str, ...]], ...] = (
    (HOST, _MIXED_REASON, (
        "host",
        "host::session",
    )),
    (HOST, "executor component assembly, Run delivery, coordination and capabilities stay in the host (plan 10.2)", (
        "host::build",
        "host::capability",
        "host::completion",
        "host::coordination",
        "host::credential_capabilities",
        "host::dependencies",
        "host::dispatch_execution",
        "host::claimed_run",
        "host::boundary_run",
        "host::run_admission",
        "host::run_control",
        "host::run",
        "host::session_ctx",
        "host::types",
    )),
    (HOST, "Session run path outside the environment lifecycle stays in the host", (
        "host::session::quiescence",
        "host::session::child_substrate",
        "host::session::content_delivery",
        "host::session::context",
        "host::session::input_projection",
        "host::session::dispatch_worker",
    )),
    (HOST, "run execution, agent runs and auxiliary runs consume the whole host", (
        "agent_runner",
        "background_task",
        "compact",
        "delegate",
        "judge",
        "outcome_controller",
        "outcome_report",
        "outcomes",
        "child_run_coordination",
        "child_run_launcher",
        "run_assembly",
        "run_capabilities",
        "run_exec",
        "session_runs",
        "step_projection",
        "tool_output_spill",
    )),
    (HOST, "session state composition, authority and committed-truth ingestion are the host's", (
        "application",
        "authority",
        "coordination",
        "durable_operations",
        "installed_session",
        "thread_reads",
    )),
    (HOST, "protocol-neutral host adapters and their composition", (
        "process_roles",
        "session_effects",
        "session_execution_error",
        "live_inbox",
        "managed_input_projection",
        "managed_outcome",
        "mcp_host",
    )),
    (HOST, "capability, tool, model and catalog assembly for a run", (
        "acp_backend",
        "acp_capability_probe",
        "acp_tool_export",
        "agent_catalog",
        "config",
        "inference_routing",
        "model_content_materializer",
        "no_model",
        "redact",
        "session_tools",
        "skill_catalog",
        "web_search",
    )),
    (HOST, "memory and skill behaviour stay; their resource projection moves later (plan 10.3 item 3)", (
        "memory",
        "session_memory_tools",
        "skills",
    )),
    (HOST, "worker and sandbox stand-ins the host installs when nothing else is configured", (
        "auxiliary_sandbox",
        "lazy_sandbox",
        "unavailable_worker",
        "uninstalled_environment",
        "worker_services",
    )),
)

_DECLARATION = re.compile(
    r"(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*([;{])"
)
_PATH_ATTRIBUTE = re.compile(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]')
_VISIBILITY_PATH = re.compile(r"\bpub\s*\(\s*in\s+[^)]*\)")
_PATH_ROOT = re.compile(r"(?<![\w$])(crate|super)\s*::")
_SHARED_HOST_IMPL = re.compile(
    r"^\s*impl(?:<[^>{]*>)?\s+(?:[A-Za-z_][\w:]*(?:<[^>{]*>)?\s+for\s+)?(?:crate::(?:host::)?)?ExecutorComponents\b[^{;]*\{", re.M
)
_FN = re.compile(r"\bfn\s+[A-Za-z_]\w*")

Module = tuple[str, ...]


def layer_table(layers=LAYERS) -> dict[Module, str]:
    table: dict[Module, str] = {}
    for target, _reason, modules in layers:
        for module in modules:
            key = tuple(module.split("::"))
            if key in table:
                raise ValueError(f"module-layer: {module} is classified twice")
            table[key] = target
    return table


def module_sources(manifest_path: Path) -> dict[Module, str]:
    """Production source per module path, following `mod` declarations and `#[path]`."""
    root = manifest_path.parent / "src" / "lib.rs"
    found: dict[Module, str] = {}

    def visit(path: Path, module: Module, module_dir: Path) -> None:
        if module in found or not path.is_file():
            return
        source = _crate_boundary_workspace.production_rust(path.read_text(encoding="utf-8"))
        found[module] = source
        syntax = _crate_boundary_workspace.rust_syntax(source)
        for match in _DECLARATION.finditer(syntax):
            name, delimiter = match.group(1, 2)
            if delimiter == "{":
                raise ValueError(f"module-layer: inline module {'::'.join((*module, name))} is not supported; "
                                 "give it its own file")
            explicit = _PATH_ATTRIBUTE.search(source[match.start():match.end()])
            child = (*module, name)
            if explicit:
                child_path = path.parent / explicit.group(1)
                visit(child_path, child, child_path.with_suffix(""))
            else:
                for candidate in (module_dir / f"{name}.rs", module_dir / name / "mod.rs"):
                    if candidate.is_file():
                        visit(candidate, child, module_dir / name)
                        break

    visit(root, (), root.parent)
    return found


def _owner(path: Module, table: dict[Module, str]) -> Module:
    for length in range(len(path), 0, -1):
        if path[:length] in table:
            return path[:length]
    return ()


def target_of(path: Module, table: dict[Module, str]) -> str:
    return table.get(_owner(path, table), HOST)


def classification_errors(modules: set[Module], table: dict[Module, str]) -> list[str]:
    mixed = {
        key[:length]
        for key in table
        for length in range(1, len(key))
        if table[key] != target_of(key[:length], table)
    }
    errors = [
        _architecture_ratchet.diagnostic(
            "module-layer", "::".join(module), "classified", "unclassified",
            f"add it to {Path(__file__).name}::LAYERS as '{REALIZATION}' or '{HOST}' with the reason")
        for module in sorted(modules)
        if module and module not in table and (len(module) == 1 or module[:-1] in mixed)
    ]
    errors += [
        _architecture_ratchet.diagnostic(
            "module-layer", "::".join(key), "listed module", "absent",
            f"remove it from {Path(__file__).name}::LAYERS; it is not a module of {CRATE}")
        for key in sorted(set(table) - modules)
    ]
    return errors


def _block_end(source: str, opening: int) -> int:
    depth = 0
    for index in range(opening, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return index + 1
    return len(source)


def _resolve(module: Module, root: str, path: tuple[str, ...]) -> Module | None:
    if root == "crate":
        target, rest = (), list(path)
    else:
        target, rest = module[:-1], list(path)
        while rest and rest[0] == "super":
            target, rest = target[:-1], rest[1:]
    rest = [part for part in rest if part != "self"]
    return (*target, *rest)


def measure(sources: dict[Module, str], table: dict[Module, str]) -> tuple[dict[str, set[str]], dict[str, int]]:
    """(upward edges per `from -> to` pair with the host items named, ExecutorComponents impl methods per module)."""
    edges: dict[str, set[str]] = defaultdict(set)
    shared_host: dict[str, int] = defaultdict(int)
    for module, source in sources.items():
        if target_of(module, table) != REALIZATION:
            continue
        origin = "::".join(_owner(module, table))
        syntax = _VISIBILITY_PATH.sub(lambda m: " " * len(m.group(0)), _crate_boundary_workspace.rust_syntax(source))
        kept = list(syntax)
        for match in _SHARED_HOST_IMPL.finditer(syntax):
            opening = syntax.index("{", match.start())
            end = _block_end(syntax, opening)
            shared_host[origin] += len(_FN.findall(syntax, opening, end))
            kept[match.start():end] = " " * (end - match.start())
        scanned = "".join(kept)
        for match in _PATH_ROOT.finditer(scanned):
            for path in _flatten_path_tree(scanned, match.end()):
                resolved = _resolve(module, match.group(1), path)
                if resolved is None or target_of(resolved, table) != HOST:
                    continue
                owner = _owner(resolved, table)
                item = resolved[len(owner):len(owner) + 1] or ("*",)
                edges[f"{origin} -> {'::'.join(owner) or 'crate'}"].add("::".join(item))
    return dict(edges), dict(shared_host)


def layer_errors(edges: dict[str, set[str]], shared_host: dict[str, int], baseline: dict) -> list[str]:
    errors: list[str] = []
    lower = f"python3 scripts/ci/{Path(__file__).name} --tighten to lower {BASELINE.name} in this commit"
    held_edges: dict[str, list[str]] = baseline.get("upward-edges", {})
    for pair in sorted(set(edges) | set(held_edges)):
        now, was = edges.get(pair, set()), set(held_edges.get(pair, []))
        for item in sorted(now - was):
            errors.append(_architecture_ratchet.diagnostic(
                "upward-edge", f"{pair}::{item}", "absent", "named host item",
                "move the shared part down into a realization or leaf module, or take it through a port"))
        for item in sorted(was - now):
            errors.append(_architecture_ratchet.diagnostic("upward-edge", f"{pair}::{item}", "named host item", "absent", lower))
    held_impl: dict[str, int] = baseline.get("shared-host-impl", {})
    for module in sorted(set(shared_host) | set(held_impl)):
        now, was = shared_host.get(module, 0), held_impl.get(module, 0)
        if now > was:
            errors.append(_architecture_ratchet.diagnostic(
                "shared-host-impl", module, was, now, "give the realization state its own owner instead of adding host methods"))
        elif now < was:
            errors.append(_architecture_ratchet.diagnostic("shared-host-impl", module, was, now, lower))
    return [f"module-layer: {error}" for error in errors]


def workspace_measures(crates: Path) -> tuple[list[str], tuple[dict[str, set[str]], dict[str, int]]]:
    manifest = _crate_boundary_workspace.crate_manifest(CRATE, crates)
    if manifest is None:
        return [_architecture_ratchet.diagnostic(
            "module-layer", CRATE, "active crate", "absent", f"retire {Path(__file__).name}")], ({}, {})
    table = layer_table()
    sources = module_sources(manifest)
    errors = classification_errors(set(sources), table)
    return (errors, ({}, {})) if errors else ([], measure(sources, table))


def check_all(crates: Path, baseline_path: Path = BASELINE) -> list[str]:
    errors, current = workspace_measures(crates)
    return errors or layer_errors(*current, json.loads(baseline_path.read_text(encoding="utf-8")))


def tightened(edges: dict[str, set[str]], shared_host: dict[str, int], baseline: dict,
              *, record: bool = False, reason: str | None = None) -> dict:
    """One candidate writer; held sets/counts only shrink unless explicitly recording.

    An intersection alone does not prove that the tree has no new edge. The
    shared CLI always validates this document using the original layer_errors.
    Metadata stays with its owner, and reasoned growth keeps its review history.
    """
    held_edges: dict[str, list[str]] = baseline.get("upward-edges", {})
    recorded = summary(edges, shared_host) if record else {}
    kept_edges = recorded["upward-edges"] if record else {
        pair: items if set(items) <= edges.get(pair, set()) else sorted(set(items) & edges.get(pair, set()))
        for pair, items in held_edges.items()}
    held_impl: dict[str, int] = baseline.get("shared-host-impl", {})
    kept_impl = recorded["shared-host-impl"] if record else {
        module: min(was, shared_host.get(module, 0)) for module, was in held_impl.items()}
    document = {**baseline,
                "upward-edges": {pair: items for pair, items in sorted(kept_edges.items()) if items},
                "shared-host-impl": {module: count for module, count in sorted(kept_impl.items()) if count > 0}}
    reasons = {name: value for name, value in baseline.get("growth_reasons", {}).items()
               if name not in {"upward-edges", "shared-host-impl"}}
    for rule in ("upward-edges", "shared-host-impl"):
        held_reasons = baseline.get("growth_reasons", {}).get(rule, {})
        for key, value in document[rule].items():
            earlier = held_reasons.get(key)
            grown = (set(value) - set(held_edges.get(key, []))) if rule == "upward-edges" else value > held_impl.get(key, 0)
            if record and grown and reason:
                earlier = _architecture_ratchet.growth_reason(earlier, reason)
            if earlier:
                reasons.setdefault(rule, {})[key] = earlier
    document.pop("growth_reasons", None)
    if reasons:
        document["growth_reasons"] = reasons
    return document


def summary(edges: dict[str, set[str]], shared_host: dict[str, int]) -> dict:
    """The original JSON measurement shape, now requested explicitly by --report."""
    return {"upward-edges": {pair: sorted(items) for pair, items in sorted(edges.items())},
            "shared-host-impl": dict(sorted(shared_host.items()))}


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                                | effect |
    | L1   | top-level module, or child of a mixed module, not in LAYERS          | error (fail closed) |
    | L2   | child of a module that is not mixed, not in LAYERS                   | inherits its parent |
    | L3   | LAYERS names a module that does not exist                            | error |
    | L4   | realization path names a host item: recorded / new / recorded but gone  | accept / error / error (lower) |
    | L5   | realization -> realization, host -> realization, `pub(in crate::host)`        | not an edge |
    | L6   | `use super::*` in a realization child of a host module                  | edge to `*` of the parent |
    | L7   | path inside `impl ExecutorComponents` / `impl T for ExecutorComponents` in realization | counted as methods, not edges |
    | L8   | ExecutorComponents methods above / below / at the baseline           | error / error (lower) / accept |
    | L9   | `--tighten`: gone item / gone pair / lower count / new item or more  | drop / drop / lower / never added |
    """
    table = layer_table((
        (REALIZATION, "r", ("real", "mix::env")),
        (HOST, "h", ("mix", "mix::run", "top")),
    ))
    modules = {(), ("real",), ("real", "inner"), ("mix",), ("mix", "env"), ("mix", "run"), ("top",)}
    assert classification_errors(modules, table) == [], "L2"
    assert target_of(("real", "inner"), table) == REALIZATION, "L2 inherit"
    assert "classified -> unclassified" in classification_errors(modules | {("new",)}, table)[0], "L1 top-level"
    assert "classified -> unclassified" in classification_errors(modules | {("mix", "new")}, table)[0], "L1 mixed child"
    assert classification_errors(modules | {("top", "new")}, table) == [], "L2 unmixed child"
    assert "not a module" in classification_errors(modules - {("top",)}, table)[0], "L3"

    sources = {
        (): "",
        ("real",): "use crate::top::{Alpha, Beta};\nuse crate::mix::env::Gamma;\npub(in crate::top) fn f() {}\n"
                   "impl ExecutorComponents { fn a(&self) { crate::top::Hidden; } fn b(&self) {} }\n"
                   "impl Port for ExecutorComponents { fn c(&self) {} }\nimpl crate::ExecutorComponents { fn d(&self) {} }\n",
        ("mix", "env"): "use super::*;\nuse super::run::Delta;\n",
        ("top",): "use crate::real::Epsilon;\n",
    }
    edges, shared_host = measure(sources, table)
    assert edges == {"real -> top": {"Alpha", "Beta"}, "mix::env -> mix": {"*"}, "mix::env -> mix::run": {"Delta"}}, \
        f"L4-L7 measure: {edges}"
    assert shared_host == {"real": 4}, "L7"
    baseline = {"upward-edges": {pair: sorted(items) for pair, items in edges.items()}, "shared-host-impl": {"real": 4}}
    assert layer_errors(edges, shared_host, baseline) == [], "L4 recorded"
    grown = {**edges, "real -> top": {"Alpha", "Beta", "Zeta"}}
    assert "real -> top::Zeta absent -> named host item" in layer_errors(grown, shared_host, baseline)[0], "L4 new"
    shrunk = {**edges, "real -> top": {"Alpha"}}
    assert "real -> top::Beta named host item -> absent" in layer_errors(shrunk, shared_host, baseline)[0], "L4 gone"
    # D5: same-cardinality replacement has two independent set effects.
    # Member identity -> added and removed diagnostics; totals cannot cancel.
    swapped = layer_errors({"r -> h": {"new"}}, {}, {"upward-edges": {"r -> h": ["old"]}})
    assert len(swapped) == 2 and "r -> h::new absent -> named host item" in swapped[0] \
        and "r -> h::old named host item -> absent" in swapped[1], "D5 set identity, growth before removal"
    assert all("; fix: " in error for error in swapped), "D5 canonical output with the original fixes"
    assert "shared-host-impl: real 4 -> 5" in layer_errors(edges, {"real": 5}, baseline)[0], "L8 above"
    assert "shared-host-impl: real 4 -> 3" in layer_errors(edges, {"real": 3}, baseline)[0], "L8 below"
    now_edges = {"real -> top": {"Alpha", "Zeta"}, "mix::env -> mix::run": {"Delta"}}
    tight = tightened(now_edges, {"real": 2, "new": 1}, baseline)
    assert tight["upward-edges"] == {"mix::env -> mix::run": ["Delta"], "real -> top": ["Alpha"]}, "L9 edges"
    assert tight["shared-host-impl"] == {"real": 2}, "L9 counts"
    assert tightened(edges, {}, baseline)["shared-host-impl"] == {}, "L9 zero drops the entry"
    assert layer_errors({"real -> top": {"Alpha"}, "mix::env -> mix::run": {"Delta"}}, {"real": 2}, tight) == [], \
        "L9 a tightened baseline holds"
    _selftest_cli()
    print("_module_layer_fitness selftest: the decision tables hold")


def _selftest_cli() -> None:
    """CLI decision table on the authoritative set/count comparison.

    Causes: mode, held/new/removed host items and methods, classification
    failure, baseline presence and a valid/empty reason. Effects: status,
    exact bytes, report format and measurement. LCLI1 default/check reject
    growth (the old default only printed JSON); report only shows the same
    JSON. LCLI2 tighten rejects new item/pair/method even alongside removal.
    LCLI3 shrink succeeds, equal does not rewrite, including unknown metadata/reasons. LCLI4
    explicit reviewed growth retains reasons; unreasoned growth is refused.
    LCLI5 no reason overrides classification, even in report mode. LCLI6
    missing baseline and invalid mode/reason combinations precede measurement.
    D8: these same actual CLI cases keep status, bytes, summary JSON and hard
    failure authority independent from the diagnostic formatter. D5/L1-L3
    additionally exercise the real classification predicate and source fix.
    """
    foreign = {"future-counter": {"k": "  foreign review  "}}
    held = {"owner": "layer", "expiry": "extraction", "future-counter": {"k": 9}, "growth_reasons": foreign, "upward-edges": {"r -> h": ["old", "gone"]},
            "shared-host-impl": {"r": 2}}

    def invoke(argv, edges, methods, baseline=held, errors=()):
        return _architecture_ratchet._cli_fixture(main, argv, baseline, "workspace_measures",
                                                   (list(errors), (edges, methods)))

    grown = {"r -> h": {"old", "new"}}
    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, observed, _ = invoke(argv, grown, {"r": 1})
        assert status == 1 and before == after and observed, f"LCLI1/LCLI2 {argv}"
    status, before, after, _, output = invoke(["--report"], grown, {"r": 1})
    assert status == 0 and before == after and json.loads(output) == summary(grown, {"r": 1}), "LCLI1 old JSON"
    for edges, methods in (({"new -> h": {"x"}}, {"r": 1}),
                           ({"r -> h": {"old"}}, {"r": 3}), ({"r -> h": {"old"}}, {"new": 1})):
        status, before, after, _, _ = invoke(["--tighten"], edges, methods)
        assert status == 1 and before == after, "LCLI2 new pair/new or grown methods"
    status, _, after, _, _ = invoke(["--tighten"], {"r -> h": {"old"}}, {"r": 1})
    document = json.loads(after)
    assert status == 0 and document == {**held, "upward-edges": {"r -> h": ["old"]},
                                        "shared-host-impl": {"r": 1}}, "LCLI3 only shrink, owner/expiry retained"
    status, before, after, _, _ = invoke(["--tighten"], {"r -> h": {"old", "gone"}}, {"r": 2})
    assert status == 0 and before == after, "LCLI3 equal bytes"
    status, before, after, _, _ = invoke(["--write-baseline"], grown, {"r": 3})
    assert status == 1 and before == after, "LCLI4 reason needed"
    prior = {**held, "growth_reasons": {**foreign, "upward-edges": {"r -> h": "first"}}}
    status, _, after, _, _ = invoke(["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], grown, {"r": 3}, prior)
    document = json.loads(after)
    assert status == 0 and document["upward-edges"] == {"r -> h": ["new", "old"]}, "LCLI4 exact facts"
    assert document["growth_reasons"] == {**foreign, "upward-edges": {"r -> h": "first; then: " + _architecture_ratchet._test_growth_reason("reviewed")},
                                          "shared-host-impl": {"r": _architecture_ratchet._test_growth_reason("reviewed")}}, "LCLI4 reason history"
    assert document["owner"] == held["owner"] and document["expiry"] == held["expiry"], "LCLI4 metadata"
    assert document["future-counter"] == held["future-counter"], "LCLI4 foreign counter/reason retained"
    status, before, after, _, _ = invoke(["--write-baseline"], grown, {"r": 3}, {})
    assert status == 1 and before == after, "LCLI4 no borrowed shape initialization waiver"
    for argv in (["--tighten"], ["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], ["--report"]):
        status, before, after, _, output = invoke(argv, grown, {"r": 1}, errors=("unclassified module",))
        assert status == 1 and before == after and not output, "LCLI5 hard classification"
    for argv in ([], ["--check"], ["--tighten"]):
        status, before, after, observed, _ = invoke(argv, {}, {}, None)
        assert status == 1 and before is None and after is None and not observed, "LCLI6 missing baseline"
    for argv in (["--check", "--tighten"], ["--report", "--check"], ["--write-baseline", "--tighten"],
                 ["--tighten", "--reason", "x"], ["--write-baseline", "--reason", "   "], ["--stage"]):
        status, before, after, observed, _ = invoke(argv, grown, {"r": 3})
        assert status == 2 and before == after and not observed, f"LCLI6 {argv}"


def main(argv: list[str] | None = None) -> int:
    return _architecture_ratchet.baseline_cli(
        argv, prog="_module_layer_fitness.py", description=__doc__, baseline_path=BASELINE, self_test=selftest,
        measure=lambda: workspace_measures(_crate_boundary_workspace.CRATES),
        check=lambda current, previous: layer_errors(*current, previous),
        tighten=lambda current, previous: tightened(*current, previous),
        write=lambda current, previous, reason: (
            tightened(*current, previous, record=True, reason=reason),
            layer_errors(*current, tightened(*current, previous))),
        report=lambda current: json.dumps(summary(*current), indent=2), report_only=True,
    )


if __name__ == "__main__":
    raise SystemExit(main())
