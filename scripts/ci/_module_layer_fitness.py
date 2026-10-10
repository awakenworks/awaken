"""Future crate boundaries inside `awaken-session-executor` hold before the crate is split.

Plan 10.2 / 10.4: the realization cluster leaves `awaken-session-executor` for its
own crate (`awaken-session-realizer`) and the host keeps the rest. That split
is a pure move only if the realization modules never reach up into host
modules. This guard makes the boundary checkable now, so the slices that
reshape the host in between cannot regress it.

Every module of the crate is classified as `realizer` or `host` in `LAYERS`,
next to the reason it is placed there. A module inherits the target of its
nearest classified ancestor, except below a *mixed* module (one with
descendants of both targets): each child of a mixed module, and each
top-level module, must be listed itself. An unlisted one is an error, so a new
module states which side it lands on (fail closed).

Counted from production code (comments and `cfg(test)` code are not scanned):

* `upward-edge`: a `crate::`, `super::` or `use` path in a `realizer` module
  that names an item of a `host` module. Recorded per (from-module,
  to-module) pair as the set of host items named; a new pair or a new item is
  an error, a recorded item that is no longer named is an error until it is
  removed from `module_layer_baseline.json` in the same commit.
* `shared-host-impl`: methods in `impl ... SharedHost` blocks inside a
  `realizer` module. `SharedHost` lives in the host, so such a block is an
  upward edge by construction; it is expected to shrink when the realization
  state gets its own owner, and is held per module at or below the baseline
  instead of being reported item by item. Paths inside those blocks are not
  counted as `upward-edge`.

When an edge or a method goes, lower the baseline with
`python3 scripts/ci/_module_layer_fitness.py --tighten`; it only removes or
lowers held entries, never adds or raises one.
"""

from __future__ import annotations

import json
import re
from collections import defaultdict
from pathlib import Path

import _crate_boundary_workspace
from _context_map_fitness import _flatten_path_tree

BASELINE = Path(__file__).with_name("module_layer_baseline.json")
CRATE = "awaken-session-executor"
REALIZER = "realizer"
HOST = "host"

_REALIZER_REASON = "realization cluster: leaves the host for awaken-session-realizer (plan 10.2)"
_LEAF_REASON = "leaf value types moved out of session_slot; go with the realizer (plan 10.1 item 1)"
_MIXED_REASON = "mixed: its own items stay in the host, each child is classified below"

# (target, reason, modules). Module paths are `::`-joined from the crate root
# and follow `#[path]` attributes.
LAYERS: tuple[tuple[str, str, tuple[str, ...]], ...] = (
    (REALIZER, _REALIZER_REASON, (
        "host::session::environment_lifecycle",
        "host::session::environment_realization",
        "host::worker_resolver::session_realization",
        "host::terminal_reconciliation",
        "provisioning",
        "managed_resource_projection",
        "cache_volume",
        "environment_continuation",
        "terminal_repository_publication",
        "session_environment",
        "environment_owner",
        "session_realizer",
        "environment_capacity",
        "environment_providers",
    )),
    (REALIZER, _LEAF_REASON, (
        "staged_resources",
        "managed_input_layout",
        "frozen_inputs",
    )),
    (REALIZER, "Skill resource projection: the delivered-bundle installer and its prompt (plan 10.3 item 3)", (
        "skill_delivery",
    )),
    (REALIZER, "the error vocabulary realization and the run path both return; the host re-exports it", (
        "host_error",
    )),
    (HOST, _MIXED_REASON, (
        "host",
        "host::session",
        "host::worker_resolver",
    )),
    (HOST, "SharedHost run, coordination and capability assembly stay in the host (plan 10.2)", (
        "host::build",
        "host::capability",
        "host::completion",
        "host::coordination",
        "host::credential_capabilities",
        "host::dependencies",
        "host::dispatch_reconciliation",
        "host::dispatch_resolution",
        "host::durable_control",
        "host::placement",
        "host::run",
        "host::session_ctx",
        "host::types",
    )),
    (HOST, "Session run path outside the environment lifecycle stays in the host", (
        "host::session::child_substrate",
        "host::session::content_delivery",
        "host::session::context",
        "host::session::input_projection",
        "host::session::run_ingress",
    )),
    (HOST, "claimed dispatch routing to the host stays with the host (plan 10.2)", (
        "host::worker_resolver::claimed_dispatch",
        "host::worker_resolver::claimed_session",
        "host::worker_resolver::realization_failure",
        "host::worker_resolver::resolver",
    )),
    (HOST, "run execution, agent runs and auxiliary runs consume the whole host", (
        "agent_runner",
        "background",
        "background_task",
        "compact",
        "delegate",
        "judge",
        "outcome_controller",
        "run_exec",
        "step_projection",
        "tool_output_spill",
    )),
    (HOST, "session state composition, authority and committed-truth ingestion are the host's", (
        "application",
        "authority",
        "commit_ingest",
        "coordination",
        "durable_operations",
        "session_slot",
        "store",
    )),
    (HOST, "protocol-neutral host adapters and their composition", (
        "dispatch_session_runtime",
        "hub",
        "live_inbox",
        "managed_adapter_error",
        "managed_host_composition",
        "managed_input_projection",
        "managed_model_capability",
        "managed_outcome",
        "mcp_host",
        "run_application_host",
    )),
    (HOST, "capability, tool, model and catalog assembly for a run", (
        "acp_backend",
        "acp_capability_probe",
        "acp_tool_export",
        "agent_catalog",
        "capabilities",
        "config",
        "deployment_config",
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
        "memory_stores",
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
    r"^\s*impl(?:<[^>{]*>)?\s+(?:[A-Za-z_][\w:]*(?:<[^>{]*>)?\s+for\s+)?(?:crate::(?:host::)?)?SharedHost\b[^{;]*\{", re.M
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
        f"module-layer: {'::'.join(module)} is not classified; add it to LAYERS in {Path(__file__).name} as "
        f"'{REALIZER}' or '{HOST}' with the reason"
        for module in sorted(modules)
        if module and module not in table and (len(module) == 1 or module[:-1] in mixed)
    ]
    errors += [
        f"module-layer: LAYERS lists {'::'.join(key)}, which is not a module of {CRATE}; remove it"
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
    """(upward edges per `from -> to` pair with the host items named, SharedHost impl methods per module)."""
    edges: dict[str, set[str]] = defaultdict(set)
    shared_host: dict[str, int] = defaultdict(int)
    for module, source in sources.items():
        if target_of(module, table) != REALIZER:
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
    held_edges: dict[str, list[str]] = baseline.get("upward-edges", {})
    for pair in sorted(set(edges) | set(held_edges)):
        now, was = edges.get(pair, set()), set(held_edges.get(pair, []))
        for item in sorted(now - was):
            errors.append(
                f"upward-edge: {pair} names host item {item}; a realizer module may not reach up into the host. "
                "Move the shared part down into a realizer or leaf module, or take it through a port"
            )
        for item in sorted(was - now):
            errors.append(f"upward-edge: {pair} no longer names {item}; lower {BASELINE.name} in this commit "
                          f"(`{Path(__file__).name} --tighten`)")
    held_impl: dict[str, int] = baseline.get("shared-host-impl", {})
    for module in sorted(set(shared_host) | set(held_impl)):
        now, was = shared_host.get(module, 0), held_impl.get(module, 0)
        if now > was:
            errors.append(f"shared-host-impl: {module} has {now} SharedHost methods, baseline {was}; give the "
                          "realization state its own owner instead of adding host methods")
        elif now < was:
            errors.append(f"shared-host-impl: {module} has {now} SharedHost methods, below the baseline {was}; "
                          f"lower {BASELINE.name} in this commit (`{Path(__file__).name} --tighten`)")
    return [f"module-layer: {error}" for error in errors]


def check_all(crates: Path, baseline_path: Path = BASELINE) -> list[str]:
    manifest = _crate_boundary_workspace.crate_manifest(CRATE, crates)
    if manifest is None:
        return [f"module-layer: {CRATE} is not a crate any more; retire {Path(__file__).name}"]
    table = layer_table()
    sources = module_sources(manifest)
    errors = classification_errors(set(sources), table)
    if errors:
        return errors
    edges, shared_host = measure(sources, table)
    return layer_errors(edges, shared_host, json.loads(baseline_path.read_text(encoding="utf-8")))


def tightened(edges: dict[str, set[str]], shared_host: dict[str, int], baseline: dict) -> dict:
    """The baseline lowered to today's measures: entries only shrink or disappear.

    A held pair keeps only the items still named, and goes when none is; a held
    SharedHost count drops to today's and goes at zero. Nothing is ever added or
    raised, so `--tighten` cannot admit a new edge or a new method.
    """
    held_edges: dict[str, list[str]] = baseline.get("upward-edges", {})
    kept_edges = {
        pair: sorted(set(items) & edges.get(pair, set()))
        for pair, items in held_edges.items()
    }
    held_impl: dict[str, int] = baseline.get("shared-host-impl", {})
    kept_impl = {module: min(was, shared_host.get(module, 0)) for module, was in held_impl.items()}
    return {
        **baseline,
        "upward-edges": {pair: items for pair, items in sorted(kept_edges.items()) if items},
        "shared-host-impl": {module: count for module, count in sorted(kept_impl.items()) if count > 0},
    }


def summary(crates: Path) -> dict:
    """The current measures in baseline shape (used to write and review the baseline)."""
    manifest = _crate_boundary_workspace.crate_manifest(CRATE, crates)
    edges, shared_host = measure(module_sources(manifest), layer_table())
    return {
        "upward-edges": {pair: sorted(items) for pair, items in sorted(edges.items())},
        "shared-host-impl": dict(sorted(shared_host.items())),
    }


def selftest() -> None:
    """Decision tables.

    | Rule | cause                                                                | effect |
    | L1   | top-level module, or child of a mixed module, not in LAYERS          | error (fail closed) |
    | L2   | child of a module that is not mixed, not in LAYERS                   | inherits its parent |
    | L3   | LAYERS names a module that does not exist                            | error |
    | L4   | realizer path names a host item: recorded / new / recorded but gone  | accept / error / error (lower) |
    | L5   | realizer -> realizer, host -> realizer, `pub(in crate::host)`        | not an edge |
    | L6   | `use super::*` in a realizer child of a host module                  | edge to `*` of the parent |
    | L7   | path inside `impl SharedHost` / `impl T for SharedHost` in realizer  | counted as methods, not edges |
    | L8   | SharedHost methods above / below / at the baseline                   | error / error (lower) / accept |
    | L9   | `--tighten`: gone item / gone pair / lower count / new item or more  | drop / drop / lower / never added |
    """
    table = layer_table((
        (REALIZER, "r", ("real", "mix::env")),
        (HOST, "h", ("mix", "mix::run", "top")),
    ))
    modules = {(), ("real",), ("real", "inner"), ("mix",), ("mix", "env"), ("mix", "run"), ("top",)}
    assert classification_errors(modules, table) == [], "L2"
    assert target_of(("real", "inner"), table) == REALIZER, "L2 inherit"
    assert "not classified" in classification_errors(modules | {("new",)}, table)[0], "L1 top-level"
    assert "not classified" in classification_errors(modules | {("mix", "new")}, table)[0], "L1 mixed child"
    assert classification_errors(modules | {("top", "new")}, table) == [], "L2 unmixed child"
    assert "not a module" in classification_errors(modules - {("top",)}, table)[0], "L3"

    sources = {
        (): "",
        ("real",): "use crate::top::{Alpha, Beta};\nuse crate::mix::env::Gamma;\npub(in crate::top) fn f() {}\n"
                   "impl SharedHost { fn a(&self) { crate::top::Hidden; } fn b(&self) {} }\n"
                   "impl Port for SharedHost { fn c(&self) {} }\nimpl crate::SharedHost { fn d(&self) {} }\n",
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
    assert "names host item Zeta" in layer_errors(grown, shared_host, baseline)[0], "L4 new"
    shrunk = {**edges, "real -> top": {"Alpha"}}
    assert "no longer names Beta" in layer_errors(shrunk, shared_host, baseline)[0], "L4 gone"
    assert "baseline 4" in layer_errors(edges, {"real": 5}, baseline)[0], "L8 above"
    assert "below the baseline" in layer_errors(edges, {"real": 3}, baseline)[0], "L8 below"
    now_edges = {"real -> top": {"Alpha", "Zeta"}, "mix::env -> mix::run": {"Delta"}}
    tight = tightened(now_edges, {"real": 2, "new": 1}, baseline)
    assert tight["upward-edges"] == {"mix::env -> mix::run": ["Delta"], "real -> top": ["Alpha"]}, "L9 edges"
    assert tight["shared-host-impl"] == {"real": 2}, "L9 counts"
    assert tightened(edges, {}, baseline)["shared-host-impl"] == {}, "L9 zero drops the entry"
    assert layer_errors({"real -> top": {"Alpha"}, "mix::env -> mix::run": {"Delta"}}, {"real": 2}, tight) == [], \
        "L9 a tightened baseline holds"
    print("_module_layer_fitness selftest: the decision tables hold")


def parse(argv: list[str]):
    import argparse
    parser = argparse.ArgumentParser(prog="_module_layer_fitness.py", description=__doc__, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="run the decision tables")
    parser.add_argument("--tighten", action="store_true",
                        help=f"lower {BASELINE.name} to today's measures (never raises or adds an entry)")
    return parser.parse_args(argv)


if __name__ == "__main__":
    import sys
    args = parse(sys.argv[1:])
    selftest()
    if args.self_test:
        raise SystemExit(0)
    if args.tighten:
        manifest = _crate_boundary_workspace.crate_manifest(CRATE, _crate_boundary_workspace.CRATES)
        measured_edges, measured_impl = measure(module_sources(manifest), layer_table())
        held = json.loads(BASELINE.read_text(encoding="utf-8"))
        BASELINE.write_text(json.dumps(tightened(measured_edges, measured_impl, held), indent=2) + "\n",
                            encoding="utf-8")
        print(f"tightened {BASELINE.name}")
        raise SystemExit(0)
    print(json.dumps(summary(_crate_boundary_workspace.CRATES), indent=2))
