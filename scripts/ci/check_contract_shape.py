#!/usr/bin/env python3
"""Ratchet structural contract rules and report module size for review.

A crate whose Cargo metadata declares ``layer = "contract"`` is the published
seam between bounded contexts (ADR-0059, ADR-0081). Five shapes make such a
seam harder to consume and evolve, and every one of them predates this gate:

1. **Graph** -- contract crates must form a DAG, and a crate's depth (longest
   chain of contract dependencies below it) may not grow. Every level of depth
   is another crate a consumer compiles and another place vocabulary can hide.
2. **Size** -- a contract module that grows without bound stops being readable.
   Size is measured per module, one source file (ADR-0091 D3, amending ADR-0081
   §7): a module whose production code declares a trait carrying methods holds
   ports (budget ``PORT_LINE_BUDGET``); any other module is vocabulary and its
   laws (budget ``TYPES_LINE_BUDGET``). Growth above the budget and its recorded
   line count is advisory. A crate's total size or a module's line count alone
   is not a reason to split an ownership boundary.
3. **Backend nouns** -- a type, trait or variant name spelling a backend or
   topology (``Docker``, ``Postgres``, ...) is adapter vocabulary living in the
   neutral core (ADR-0059 §1). Comments and string literals do not count.
4. **Catch-all errors** -- ``Storage(String)``-style variants of an ``*Error``
   enum (or an ``*Error(String)`` newtype) erase the failure a caller must
   classify, so callers end up matching on message text.
5. **Capability defaults** -- a trait method default that returns ``Err`` whose
   wording says the implementation lacks the operation ("not implemented",
   "does not support", "unsupported", ... see ``CAPABILITY_WORDING``) or names an
   ``Unsupported`` variant encodes an optional capability as a runtime failure. It is fail-closed, so
   ``check_degrading_defaults.py`` rightly admits it as *not a downgrade*; this
   gate counts it as a capability that should be a supertrait instead. A default
   that refuses on purpose (any other ``Err``) is neither.

Every count is compared with ``scripts/ci/contract_shape_baseline.json``.
Structural growth fails and names the instances; module-line growth warns.
A fall is admitted;
``--tighten`` records it so the gained ground is held (line counts only once
they shrink by ``LINE_SLACK`` or fall under budget, so ordinary edits do not
churn the file). Tightening never adds or raises a module-line record.
Default/``--check`` reads without writing. ``--write-baseline`` records the
current tree; structural growth needs ``--reason``, kept next to the number
for review. Recording advisory line measurements needs no reason.

Production source is the structural module graph from each Cargo target root,
with ``cfg(test)`` and ``cfg(kani)`` items and all comments removed -- the same
projection every crate-boundary fitness check uses.
"""

from __future__ import annotations

import subprocess
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

import _crate_boundary_workspace as workspace
import _architecture_ratchet

# Every Cargo manifest and production Rust file can add, remove or reshape a
# contract crate; the baseline and the shared lexer define the comparison.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "scripts/ci/_crate_boundary_workspace.py",
    "scripts/ci/_architecture_ratchet.py",
    "scripts/ci/contract_shape_baseline.json",
)

ROOT = Path(__file__).resolve().parents[2]
BASELINE = ROOT / "scripts/ci/contract_shape_baseline.json"

TYPES_LINE_BUDGET = 8000
PORT_LINE_BUDGET = 2000
LINE_SLACK = 200

BACKEND_NOUNS = ("Bubblewrap", "Seatbelt", "Kubernetes", "Docker", "Podman", "Postgres", "Sqlite")
CATCH_ALL_VARIANTS = frozenset(
    {
        "Backend", "Execution", "Failed", "Failure", "Internal", "Invalid", "Io", "Other",
        "Persistence", "Rejected", "Setup", "Storage", "Transport", "Unavailable",
        "Unexpected", "Unknown",
    }
)
# Wording that says an implementation *lacks* the operation, as opposed to a
# deliberate refusal of a request it understands (a PermissionDenied, an
# inactive inbox, a validation failure).
CAPABILITY_WORDING = re.compile(
    r"not implemented|unsupported|not supported|not configured|not installed|is unavailable"
    r"|does not (?:implement|support|provide|expose|persist|reconcile)",
    re.IGNORECASE,
)
CAPABILITY_VARIANTS = re.compile(r"\b(?:Unsupported|NotImplemented|NotSupported)\b")
STRING_PAYLOAD = re.compile(r"^\(\s*(?:pub\s+)?(?:(?:std|alloc)::string::)?String\s*,?\s*\)$")
STRING_FIELD = re.compile(r"^\{\s*(?:pub\s+)?\w+\s*:\s*(?:(?:std|alloc)::string::)?String\s*,?\s*\}$")

RULES = ("depth", "module_lines", "backend_nouns", "catch_all_errors", "capability_defaults")
INTERFACE_KINDS = ("types", "spi", "api", "remote", "ext")
PORT_KINDS = frozenset(INTERFACE_KINDS[1:])


@dataclass(frozen=True)
class ContractCrate:
    name: str
    contract_deps: frozenset[str]
    # relative path -> production projection (comments, cfg(test) and cfg(kani) blanked)
    sources: dict[str, str] = field(default_factory=dict)
    # ADR-0081 §1 interface kinds declared in [package.metadata.awaken]; None when absent
    kinds: frozenset[str] | None = None


def budget_of(role: str) -> int:
    return TYPES_LINE_BUDGET if role == "types" else PORT_LINE_BUDGET


@dataclass
class Measurement:
    depth: int = 0
    production_lines: int = 0
    role: str = "types"
    # relative path -> (production lines, role) of every module of the crate
    modules: dict[str, tuple[int, str]] = field(default_factory=dict)
    backend_nouns: list[str] = field(default_factory=list)
    catch_all_errors: list[str] = field(default_factory=list)
    capability_defaults: list[str] = field(default_factory=list)

    def count(self, rule: str) -> int:
        value = getattr(self, rule)
        return value if isinstance(value, int) else len(value)

    def over_budget(self) -> dict[str, int]:
        """Production lines of every module above the budget of its role."""
        return {rel: lines for rel, (lines, role) in sorted(self.modules.items()) if lines > budget_of(role)}


# --------------------------------------------------------------------------- lexing


def production_projection(source: str) -> str:
    """Production Rust with verification-only items removed.

    ``cfg(kani)`` has the same width as ``cfg(test)``, so rewriting the predicate
    reuses the shared, offset-preserving test-item projection unchanged.
    """
    return workspace.production_rust(source.replace("cfg(kani)", "cfg(test)"))


def _balanced(text: str, open_index: int, opening: str = "{", closing: str = "}") -> int:
    """Index one past the delimiter matching ``text[open_index]`` (or len(text))."""
    end = workspace._balanced_end(text, open_index, opening, closing)
    return len(text) if end is None else end


def _top_level_items(body: str) -> list[tuple[int, str]]:
    """Comma-separated items of an enum body, with their offsets."""
    items: list[tuple[int, str]] = []
    depth = 0
    start = 0
    for index, character in enumerate(body):
        if character in "([{<":
            depth += 1
        elif character in ")]}" or (character == ">" and body[index - 1 : index] != "-"):
            depth -= 1
        elif character == "," and depth == 0:
            items.append((start, body[start:index]))
            start = index + 1
    items.append((start, body[start:]))
    return [(offset, item) for offset, item in items if item.strip()]


def _strip_attributes(item: str) -> str:
    item = item.strip()
    while item.startswith("#"):
        close = _balanced(item, item.find("["), "[", "]")
        item = item[close:].strip()
    return item


def _line(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def measure_sources(sources: dict[str, str]) -> Measurement:
    measurement = Measurement()
    # Follow local scalar wrappers to a fixed point. Giving an opaque string a
    # name does not give callers an error category. Structured payloads retain
    # their own semantics and are not collapsed by this syntactic ratchet.
    carriers = {"String"}
    declarations = "\n".join(workspace.rust_syntax(text) for text in sources.values())
    wrappers = re.findall(
        r"\bstruct\s+([A-Z]\w*)\s*\(\s*(?:pub\s+)?([\w:]+)\s*,?\s*\)\s*;",
        declarations,
    ) + re.findall(r"\btype\s+([A-Z]\w*)\s*=\s*([\w:]+)\s*;", declarations)
    while True:
        found = {name for name, wrapped in wrappers if wrapped.split("::")[-1] in carriers}
        if found <= carriers:
            break
        carriers.update(found)

    def opaque_string(payload: str) -> bool:
        if STRING_PAYLOAD.match(payload) or STRING_FIELD.match(payload):
            return True
        match = re.fullmatch(r"\(\s*(?:pub\s+)?([\w:]+)\s*,?\s*\)", payload)
        return bool(match and match.group(1).split("::")[-1] in carriers)

    for relpath, production in sorted(sources.items()):
        syntax = workspace.rust_syntax(production)
        lines = sum(1 for line in production.splitlines() if line.strip())
        measurement.production_lines += lines
        measurement.modules[relpath] = (lines, "types")

        for declaration in re.finditer(r"\b(struct|enum|trait|type|union)\s+([A-Z]\w*)", syntax):
            kind, name = declaration.groups()
            if kind == "trait":
                line_start = syntax.rfind("\n", 0, declaration.start()) + 1
                if not re.search(r"\bpub\s*$", syntax[line_start:declaration.start()]):
                    continue
                open_index = syntax.find("{", declaration.end())
                semicolon = syntax.find(";", declaration.end())
                if open_index >= 0 and (semicolon < 0 or open_index < semicolon):
                    body = syntax[open_index:_balanced(syntax, open_index)]
                    if re.search(r"\bfn\s+\w+", body):
                        measurement.role = "ports"
                        measurement.modules[relpath] = (lines, "ports")
            noun = next((token for token in BACKEND_NOUNS if token in name), None)
            if noun:
                measurement.backend_nouns.append(
                    f"{relpath}:{_line(syntax, declaration.start())}: {kind} {name}"
                )

        for enum in re.finditer(r"\benum\s+([A-Z]\w*)[^{;]*\{", syntax):
            name = enum.group(1)
            open_index = enum.end() - 1
            close = _balanced(syntax, open_index)
            body = syntax[open_index + 1 : close - 1]
            for offset, item in _top_level_items(body):
                variant = _strip_attributes(item)
                match = re.match(r"([A-Z]\w*)\s*(.*)$", variant, re.DOTALL)
                if not match:
                    continue
                variant_name, payload = match.group(1), match.group(2).split("=")[0].strip()
                location = f"{relpath}:{_line(syntax, open_index + 1 + offset)}"
                noun = next((token for token in BACKEND_NOUNS if token in variant_name), None)
                if noun:
                    measurement.backend_nouns.append(f"{location}: variant {name}::{variant_name}")
                if (
                    name.endswith("Error")
                    and variant_name in CATCH_ALL_VARIANTS
                    and opaque_string(payload)
                ):
                    measurement.catch_all_errors.append(f"{location}: {name}::{variant_name}(String)")

        for newtype in re.finditer(r"\bstruct\s+([A-Z]\w*Error)\s*(\([^()]*\))\s*;", syntax):
            if opaque_string(newtype.group(2)):
                measurement.catch_all_errors.append(
                    f"{relpath}:{_line(syntax, newtype.start())}: {newtype.group(1)}(String)"
                )

        for trait in re.finditer(r"\bpub\s+trait\s+([A-Z]\w*)[^{;]*\{", syntax):
            open_index = trait.end() - 1
            close = _balanced(syntax, open_index)
            consumed = open_index
            for method in re.finditer(r"\bfn\s+([a-z_]\w*)", syntax[open_index:close]):
                start = open_index + method.end()
                if start < consumed:
                    continue  # a function nested inside an earlier default body
                brace = syntax.find("{", start, close)
                semicolon = syntax.find(";", start, close)
                if brace < 0 or (0 <= semicolon < brace):
                    continue  # required method: no default body
                body_end = _balanced(syntax, brace)
                consumed = body_end
                if "Err(" not in syntax[brace:body_end]:
                    continue
                literal_body = production[brace:body_end]
                if CAPABILITY_WORDING.search(literal_body) or (
                    CAPABILITY_VARIANTS.search(syntax[brace:body_end])
                ):
                    measurement.capability_defaults.append(
                        f"{relpath}:{_line(syntax, brace)}: {trait.group(1)}::{method.group(1)}"
                    )
    return measurement


# --------------------------------------------------------------------------- graph


def contract_cycles(crates: dict[str, ContractCrate]) -> list[list[str]]:
    """Every strongly connected component with more than one crate (or a self edge)."""
    index: dict[str, int] = {}
    low: dict[str, int] = {}
    stack: list[str] = []
    on_stack: set[str] = set()
    components: list[list[str]] = []

    def visit(node: str) -> None:
        index[node] = low[node] = len(index)
        stack.append(node)
        on_stack.add(node)
        for dep in sorted(crates[node].contract_deps & crates.keys()):
            if dep not in index:
                visit(dep)
                low[node] = min(low[node], low[dep])
            elif dep in on_stack:
                low[node] = min(low[node], index[dep])
        if low[node] == index[node]:
            component: list[str] = []
            while True:
                member = stack.pop()
                on_stack.discard(member)
                component.append(member)
                if member == node:
                    break
            if len(component) > 1 or node in crates[node].contract_deps:
                components.append(sorted(component))

    for name in sorted(crates):
        if name not in index:
            visit(name)
    return components


def contract_depths(crates: dict[str, ContractCrate]) -> dict[str, int]:
    """Longest contract-dependency chain below each crate (leaf = 0). Requires a DAG."""
    depths: dict[str, int] = {}

    def depth(name: str) -> int:
        if name not in depths:
            deps = crates[name].contract_deps & crates.keys()
            depths[name] = 1 + max((depth(dep) for dep in deps), default=-1)
        return depths[name]

    for name in crates:
        depth(name)
    return depths


def measure(crates: dict[str, ContractCrate]) -> tuple[list[str], dict[str, Measurement]]:
    cycles = contract_cycles(crates)
    errors = [
        _architecture_ratchet.diagnostic(
            "depth", " -> ".join(cycle + cycle[:1]), "acyclic contract graph", "cycle",
            "move the shared vocabulary into the lower crate so the edge points one way")
        for cycle in cycles
    ]
    if cycles:
        return errors, {}
    depths = contract_depths(crates)
    measurements: dict[str, Measurement] = {}
    for name, crate in sorted(crates.items()):
        measurement = measure_sources(crate.sources)
        measurement.depth = depths[name]
        measurements[name] = measurement
        errors.extend(kind_errors(name, crate.kinds, measurement))
    return errors, measurements


def kind_errors(name: str, kinds: frozenset[str] | None, measurement: Measurement) -> list[str]:
    """ADR-0081 §1: a contract crate declares which interface kinds it holds.

    K1 no ``kinds`` metadata; K2 an unknown kind; K3 a types-only crate that
    holds a port trait (a trait with methods); K4 a crate that declares a port
    kind but holds no port trait. Each is a hard error, not a ratchet: the
    declaration is what lets the direction rules (spi implemented outside the
    application layer, api implemented by it) be checked at all.
    """
    if kinds is None:
        return [
            _architecture_ratchet.diagnostic(
                "kinds", name, "declared interface kinds", "missing",
                'declare the ADR-0081 interface kinds in [package.metadata.awaken], e.g. kinds = ["types", "spi"]')
        ]
    unknown = sorted(kinds - set(INTERFACE_KINDS))
    if unknown:
        return [
            _architecture_ratchet.diagnostic(
                "kinds", name, list(INTERFACE_KINDS), unknown,
                f"use only {list(INTERFACE_KINDS)} in [package.metadata.awaken] kinds (ADR-0081 §1)")
        ]
    holds_ports = measurement.role == "ports"
    if not (kinds & PORT_KINDS) and holds_ports:
        return [
            _architecture_ratchet.diagnostic(
                "kinds", name, "types-only", "trait with methods",
                "declare the port kind it holds (spi/api/remote/ext) or move the trait to the crate that owns it")
        ]
    if (kinds & PORT_KINDS) and not holds_ports:
        return [
            _architecture_ratchet.diagnostic(
                "kinds", name, sorted(kinds & PORT_KINDS), "no trait with methods",
                'declare kinds = ["types"] so the budget and direction rules apply to what is really there')
        ]
    return []


# --------------------------------------------------------------------------- ratchet

HOW_TO_FIX = {
    "depth": "depend on the lower contract that owns the vocabulary instead of one that re-exports it",
    "module_lines": "review module cohesion and ownership (ADR-0091 D3: line counts are advisory; "
    "size alone is not a reason to split a module or crate)",
    "backend_nouns": "move the backend-named type into its provider/adapter crate and keep a neutral "
    "name in the contract (ADR-0059 §1)",
    "catch_all_errors": "give the failure a typed variant the caller can classify instead of a message",
    "capability_defaults": "require the method, or express the optional capability as a supertrait "
    "the capable implementations add (ADR-0081 capability ladder)",
}


def compare(
    measurements: dict[str, Measurement], baseline: dict, *, warnings: list[str] | None = None
) -> list[str]:
    """Structural failures, with advisory size findings in the optional sink."""
    recorded: dict[str, dict] = baseline.get("crates", {})
    max_depth = max((entry.get("depth", 0) for entry in recorded.values()), default=0)
    errors: list[str] = []
    for name, current in sorted(measurements.items()):
        entry = recorded.get(name)
        entry = entry or {}
        # depth
        base_depth = entry.get("depth", max_depth)
        if current.depth > base_depth:
            errors.append(_architecture_ratchet.diagnostic("depth", name, base_depth, current.depth, HOW_TO_FIX["depth"]))
        # size, per module
        recorded_modules = entry.get("module_lines", {})
        for rel, lines in current.over_budget().items():
            role = current.modules[rel][1]
            base_lines = recorded_modules.get(rel)
            if (base_lines is None or lines > base_lines) and warnings is not None:
                limit = budget_of(role) if base_lines is None else base_lines
                warnings.append(_architecture_ratchet.diagnostic(
                    "module_lines", f"{name}::{rel}", limit, lines,
                    HOW_TO_FIX["module_lines"],
                    details=f"{role} module budget {budget_of(role)}"))
        # counted instances
        for rule in RULES[2:]:
            count = current.count(rule)
            base = entry.get(rule, 0)
            if count > base:
                instances = "\n    ".join(getattr(current, rule))
                errors.append(_architecture_ratchet.diagnostic(rule, name, base, count, HOW_TO_FIX[rule], details=instances))
    return errors


def tightenable(measurements: dict[str, Measurement], baseline: dict) -> list[str]:
    """Gains the baseline has not recorded yet.

    A fall used to fail the gate until someone hand-lowered the number, which
    made the baseline the file most often co-edited with contract code (26 of
    its 42 edits in 500 commits only lowered a number). A fall is now admitted
    here and recorded mechanically by ``--tighten``, which the pre-commit hook
    runs and stages, so the gain is still kept and no person edits the file for
    it. Only structural growth needs a human, with ``--write-baseline --reason``.
    """

    recorded: dict[str, dict] = baseline.get("crates", {})
    gains: list[str] = [
        f"{name}: recorded but no longer a contract crate"
        for name in sorted(set(recorded) - set(measurements))
    ]
    for name, current in sorted(measurements.items()):
        entry = recorded.get(name)
        if entry is None:
            continue
        if current.depth < entry.get("depth", current.depth):
            gains.append(f"{name}.depth: {entry['depth']} -> {current.depth}")
        for rel, base_lines in sorted(entry.get("module_lines", {}).items()):
            lines, role = current.modules.get(rel, (0, "types"))
            if lines <= budget_of(role) or base_lines - lines >= LINE_SLACK:
                gains.append(f"{name}.module_lines[{rel}]: {base_lines} -> {lines}")
        for rule in RULES[2:]:
            if current.count(rule) < entry.get(rule, 0):
                gains.append(f"{name}.{rule}: {entry[rule]} -> {current.count(rule)}")
    return gains


def render_baseline(
    measurements: dict[str, Measurement], previous: dict, reason: str | None, *, tighten: bool = False
) -> tuple[dict, list[str]]:
    """New baseline plus structural growths that ``reason`` must justify.

    Tightening reuses the same renderer but cannot add or raise advisory line
    allowances when another rule shrinks. Explicit writes record actual lines.
    """
    old = previous.get("crates", {})
    growth: list[str] = []
    crates: dict[str, dict] = {}
    for name, current in sorted(measurements.items()):
        prior = old.get(name, {})
        entry: dict = {key: value for key, value in prior.items()
                       if key not in (*RULES, "growth_reasons", "production_lines")}
        entry["depth"] = current.depth
        modules = current.over_budget()
        if tighten:
            held = prior.get("module_lines", {})
            modules = {rel: min(lines, held[rel]) for rel, lines in modules.items() if rel in held}
        if modules:
            entry["module_lines"] = modules
        for rule in RULES[2:]:
            if current.count(rule):
                entry[rule] = current.count(rule)
        # ADR-0091 D3 retired the known per-crate production_lines metric
        # (W5 below); reasons for unknown/foreign metrics are not ours to retire.
        reasons = {rule: text for rule, text in prior.get("growth_reasons", {}).items() if rule != "production_lines"}
        for rule in RULES:
            if rule not in entry:
                reasons.pop(rule, None)
                continue
            if rule == "depth":
                grown = [(rule, prior.get(rule), entry[rule])] if name in old else []
            elif rule == "module_lines":
                held = prior.get(rule, {})
                grown = [
                    (f"{rule}[{rel}]", held.get(rel, budget_of(current.modules[rel][1])), lines)
                    for rel, lines in entry[rule].items()
                ]
            else:
                grown = [(rule, prior.get(rule, 0), entry[rule])]
            grown = [(label, before, now) for label, before, now in grown if before is not None and now > before]
            if rule != "module_lines":
                for label, before, now in grown:
                    growth.append(f"{name}.{label}: {before} -> {now}")
            if grown and reason:
                # Each growth keeps the reasons recorded for earlier growth:
                # the baseline is the history of why a number may be this high.
                earlier = reasons.get(rule)
                reasons[rule] = _architecture_ratchet.growth_reason(earlier, reason)
        if reasons:
            entry["growth_reasons"] = reasons
        crates[name] = entry
    document = {
        **previous,
        "$comment": (
            "Contract-shape ratchet (scripts/ci/check_contract_shape.py, ADR-0081). Structural counts "
            "may only fall; raising one requires --write-baseline --reason, recorded in growth_reasons. "
            "Module lines are advisory; --tighten only lowers their records."
        ),
        "crates": crates,
    }
    return document, growth


# --------------------------------------------------------------------------- workspace


def load_contract_crates() -> dict[str, ContractCrate]:
    manifests = {
        path: workspace.load_manifest(path)
        for path in workspace.iter_crate_manifests()
    }
    contract = {
        workspace.package_name(manifest): (path, manifest)
        for path, manifest in manifests.items()
        if workspace._awaken_metadata(manifest).get("layer") == "contract"
    }
    crates: dict[str, ContractCrate] = {}
    for name, (path, manifest) in contract.items():
        sources = {
            file.relative_to(ROOT).as_posix(): production_projection(file.read_text(encoding="utf-8"))
            for file in sorted(workspace.production_module_files([path]))
        }
        deps = frozenset(workspace.normal_dependency_names(manifest)) & contract.keys()
        declared = workspace._awaken_metadata(manifest).get("kinds")
        kinds = frozenset(str(kind) for kind in declared) if isinstance(declared, list) else None
        crates[name] = ContractCrate(name, deps, sources, kinds)
    return crates


def table(measurements: dict[str, Measurement]) -> str:
    header = f"{'crate':42} {'depth':>5} {'lines':>6} {'role':>5} {'nouns':>5} {'catch':>5} {'caps':>5}"
    rows = [header]
    for name, m in sorted(measurements.items()):
        rows.append(
            f"{name:42} {m.depth:5} {m.production_lines:6} {m.role:>5} {len(m.backend_nouns):5} "
            f"{len(m.catch_all_errors):5} {len(m.capability_defaults):5}"
        )
    return "\n".join(rows)


# --------------------------------------------------------------------------- self-test


def self_test() -> None:
    """Cause/effect decision table, proven on synthetic crates.

    Causes: C1 contract edge closes a cycle; C2 depth above/below baseline;
    C3 module lines above its role's budget and above/below baseline; C4 backend noun in a
    declaration name vs only in a comment/literal/function; C5 catch-all
    String variant in an *Error enum vs a typed or identity variant or a
    non-error enum; C6 default body returns Err with capability wording or
    variant vs a deliberate refusal vs a successful default; C7 count
    above/below baseline; C8 --write-baseline raises a number.

    | Rule | Cause                                            | Effect                       |
    | G1   | C1 a -> b -> a                                   | cycle error, no measurements |
    | G2   | C2 chain a -> b -> c                             | depths 2/1/0                 |
    | D1   | C2 depth > baseline                              | reject                       |
    | D2   | C2 depth < recorded baseline                     | admit; --tighten records it  |
    | L1   | C3 ports module over budget, no baseline         | warn; admit                  |
    | L2   | C3 over budget, <= baseline                      | admit                        |
    | L3   | C3 shrinks by >= LINE_SLACK / under budget / gone | admit; --tighten records it |
    | L5   | recorded crate no longer a contract crate        | admit; --tighten drops it    |
    | L4   | C3 trait-free module under TYPES budget          | admit, role types            |
    | L6   | C3 crate over a budget in total, every module under | admit (no crate budget)   |
    | L7   | C3 module above its recorded count              | warn; admit                  |
    | L8   | C3 line growth + structural growth              | warn + structural rejection  |
    | N1   | C4 struct/enum/trait/variant names               | counted                      |
    | N2   | C4 comment, string literal, fn, cfg(test) item   | not counted                  |
    | E1   | C5 Storage(String), Backend { message: String }  | counted                      |
    | E2   | C5 NotFound(String), Storage(io::Error), non-Err | not counted                  |
    | E3   | C5 struct SandboxError(pub String)               | counted                      |
    | F1   | C6 "not implemented"/"does not support"/Unsupported | counted                   |
    | F2   | C6 Err(Forbidden)/Err(Inactive), Ok default, no body | not counted               |
    | R1   | C7 count > baseline                              | reject naming instances      |
    | R2   | C7 count < baseline                              | admit; --tighten records it  |
    | W1   | C8 raise without reason                          | growth reported              |
    | W2   | C8 raise with reason                             | reason retained in baseline  |
    | W4   | C8 module rises above its budget                 | record actual lines, no gate |
    | W5   | C8 reason recorded for a rule no longer measured | dropped                      |
    | K1   | C9 no kinds metadata                             | reject                       |
    | K2   | C9 unknown kind                                  | reject                       |
    | K3   | C9 types-only crate holds a port trait           | reject                       |
    | K4   | C9 port kind declared, no port trait             | reject                       |
    | K5   | C9 kinds match what the crate holds              | admit                        |
    """
    project = production_projection

    cyclic = {
        "a": ContractCrate("a", frozenset({"b"}), kinds=frozenset({"types"})),
        "b": ContractCrate("b", frozenset({"a"}), kinds=frozenset({"types"})),
    }
    errors, measured = measure(cyclic)
    assert errors and not measured and "a -> b -> a" in errors[0], "G1"

    chain = {
        "a": ContractCrate("a", frozenset({"b", "external"}), kinds=frozenset({"types"})),
        "b": ContractCrate("b", frozenset({"c"}), kinds=frozenset({"types"})),
        "c": ContractCrate("c", frozenset(), kinds=frozenset({"types"})),
    }
    errors, measured = measure(chain)
    assert not errors and [measured[n].depth for n in "abc"] == [2, 1, 0], "G2"
    assert compare(measured, {"crates": {"a": {"depth": 1}, "b": {"depth": 1}, "c": {"depth": 0}}}), "D1"
    deeper = {"crates": {"a": {"depth": 2}, "b": {"depth": 2}, "c": {"depth": 0}}}
    assert not compare(measured, deeper), "D2 a fall is admitted"
    assert tightenable(measured, deeper) == ["b.depth: 2 -> 1"], "D2 and reported"
    assert not compare(measured, {"crates": {"a": {"depth": 2}, "b": {"depth": 1}, "c": {"depth": 0}}}), "G2 admits"

    # D6: missing crate or depth field -> original maximum-held depth fallback;
    # fall/equality stay admitted, growth names the actual fallback transition.
    fallback = {"crates": {"held": {"depth": 3}, "missing": {}}}
    for name in ("new", "missing"):
        assert compare({name: Measurement(depth=3)}, fallback) == [], "D6 depth fallback equal"
        assert f"depth: {name} 3 -> 4" in compare({name: Measurement(depth=4)}, fallback)[0], "D6 depth fallback growth"

    port = "pub trait Port {\n    fn call(&self);\n}\n"
    big = Measurement(depth=0, role="ports", modules={"p.rs": (PORT_LINE_BUDGET + 10, "ports")})
    warnings: list[str] = []
    assert not compare({"p": big}, {"crates": {}}, warnings=warnings), "L1 lines never reject"
    assert len(warnings) == 1 and "module_lines: p::p.rs 2000 -> 2010" in warnings[0] \
        and "advisory" in warnings[0], "L1 original measurement, advisory review"
    recorded = {"crates": {"p": {"depth": 0, "module_lines": {"p.rs": PORT_LINE_BUDGET + 10}}}}
    assert not compare({"p": big}, recorded), "L2"
    shrunk = {"crates": {"p": {"depth": 0, "module_lines": {"p.rs": PORT_LINE_BUDGET + 10 + LINE_SLACK}}}}
    assert not compare({"p": big}, shrunk) and tightenable({"p": big}, shrunk), "L3 slack"
    small = Measurement(depth=0, role="ports", modules={"p.rs": (PORT_LINE_BUDGET - 1, "ports")})
    under = {"crates": {"p": {"depth": 0, "module_lines": {"p.rs": PORT_LINE_BUDGET + 5}}}}
    assert not compare({"p": small}, under) and tightenable({"p": small}, under), "L3 budget"
    moved = Measurement(depth=0, role="ports", modules={"q.rs": (10, "ports")})
    assert tightenable({"p": moved}, under) == ["p.module_lines[p.rs]: 2005 -> 0"], "L3 gone"
    # D6: a role change can retire a held ports-sized allowance at equal lines.
    # Role budget, not numeric decrease or crate total -> original gain token.
    reclassified = Measurement(modules={"p.rs": (PORT_LINE_BUDGET + 5, "types")})
    assert compare({"p": reclassified}, under) == [] and tightenable({"p": reclassified}, under) == [
        "p.module_lines[p.rs]: 2005 -> 2005"], "D6 role budget retains equal-value gain semantics"
    whole = Measurement(depth=0, role="ports", modules={"a.rs": (PORT_LINE_BUDGET, "ports"),
                                                         "b.rs": (TYPES_LINE_BUDGET, "types")})
    assert not compare({"w": whole}, {"crates": {}}), "L6 the crate total is not budgeted"
    mixed = measure_sources({"a.rs": project("pub trait Port {\n    fn call(&self);\n}\n"),
                             "b.rs": project("pub struct Value;\n")})
    assert mixed.modules == {"a.rs": (3, "ports"), "b.rs": (1, "types")} and mixed.role == "ports", "L4 per module"
    gone = {"crates": {"p": {"depth": 0}, "retired": {"depth": 1}}}
    assert tightenable({"p": small}, gone) == ["retired: recorded but no longer a contract crate"], "L5"
    assert measure_sources({"x.rs": project(port)}).role == "ports", "L4 ports"
    private_helper = "trait Members {\n    fn insert(&mut self);\n}\n"
    assert measure_sources({"x.rs": project(private_helper)}).role == "types", "L4 private helper"
    ports_measured = measure_sources({"x.rs": project(port)})
    types_measured = measure_sources({"x.rs": project("pub struct Value;\n")})
    assert kind_errors("k", None, ports_measured), "K1"
    # D7 hard metadata mismatch -> explicit state and original metadata fix;
    # the diagnostic is independent from every baseline/reason mode (SCLI3).
    missing_kinds = kind_errors("k", None, ports_measured)[0]
    assert "kinds: k declared interface kinds -> missing" in missing_kinds and "; fix: " in missing_kinds \
        and "--write-baseline" not in missing_kinds, "D7 metadata is its own authority"
    assert kind_errors("k", frozenset({"types", "backend"}), types_measured), "K2"
    assert kind_errors("k", frozenset({"types"}), ports_measured), "K3"
    assert kind_errors("k", frozenset({"types", "spi"}), types_measured), "K4"
    assert not kind_errors("k", frozenset({"types", "spi"}), ports_measured), "K5 ports"
    assert not kind_errors("k", frozenset({"types"}), types_measured), "K5 types"
    vocabulary = Measurement(depth=0, modules={"v.rs": (PORT_LINE_BUDGET + 10, "types")})
    marker = "pub trait Marker {}\npub struct Value;\n"
    assert measure_sources({"x.rs": project(marker)}).role == "types", "L4 marker trait is not a port"
    assert not compare({"t": vocabulary}, {"crates": {}}), "L4"

    # L7/L8 decision table: each role keeps its numeric budget and recorded
    # allowance. Above either -> one warning, no error. Adding any structural
    # growth -> that original error and the same line warning, never a waiver.
    for role, budget in (("types", TYPES_LINE_BUDGET), ("ports", PORT_LINE_BUDGET)):
        for held, lines in ((None, budget), (None, budget + 1), (budget + 1, budget + 1),
                            (budget + 1, budget + 2)):
            baseline = {"crates": {"p": {"depth": 0}}}
            if held is not None:
                baseline["crates"]["p"]["module_lines"] = {"p.rs": held}
            warnings = []
            current = Measurement(modules={"p.rs": (lines, role)})
            assert not compare({"p": current}, baseline, warnings=warnings), "L7 advisory for both roles"
            assert bool(warnings) == (lines > (budget if held is None else held)), "L7 exact boundary"
    for rule, value in (("depth", 1), ("backend_nouns", ["p.rs:1: struct DockerHandle"]),
                        ("catch_all_errors", ["p.rs:2: StoreError::Storage(String)"]),
                        ("capability_defaults", ["p.rs:3: Port::unsupported"])):
        warnings = []
        current = Measurement(**{rule: value}, modules=big.modules)
        errors = compare({"p": current}, {"crates": {}}, warnings=warnings)
        assert len(errors) == 1 and errors[0].startswith(rule + ":") and len(warnings) == 1, \
            f"L8 line advisory cannot waive {rule}"

    nouns = project(
        "pub struct DockerHandle;\npub enum Backend { Podman, Local }\npub trait SqliteStore {}\n"
        "// struct KubernetesComment;\nconst NAME: &str = \"struct PostgresLiteral\";\n"
        "fn docker_socket() {}\n#[cfg(test)]\nstruct SeatbeltFixture;\n"
    )
    found = measure_sources({"n.rs": nouns}).backend_nouns
    assert sorted(entry.split(": ", 1)[1] for entry in found) == [
        "struct DockerHandle", "trait SqliteStore", "variant Backend::Podman",
    ], f"N1/N2 {found}"

    errors_src = project(
        "pub enum StoreError {\n    #[error(\"storage: {0}\")]\n    Storage(String),\n"
        "    Backend { message: String },\n    NotFound(String),\n    Io(std::io::Error),\n"
        "    Conflict { expected: u64, actual: u64 },\n}\n"
        "pub enum Outcome { Invalid(String) }\n"
        "pub struct SandboxError(pub String);\npub struct Typed(pub String);\n"
    )
    catch_all = measure_sources({"e.rs": errors_src}).catch_all_errors
    assert sorted(entry.split(": ", 1)[1] for entry in catch_all) == [
        "SandboxError(String)", "StoreError::Backend(String)", "StoreError::Storage(String)",
    ], f"E1/E2/E3 {catch_all}"

    # E4/E5: direct, wrapped, and alias-of-wrapper strings have the same debt;
    # E6: a structured failure with an independent kind must not be counted as
    # string-only. This catches the old ResourceErrorDetail metric loophole.
    wrapped = project(
        "struct Detail(String); type Alias = Detail; struct Twice(Alias);"
        "struct Structured { kind: u8, detail: String }"
        "enum WrappedError { Storage(Twice), Invalid(Structured) }"
        "struct BoundaryError(Detail);"
    )
    assert len(measure_sources({"wrapped.rs": wrapped}).catch_all_errors) == 2, "E4-E6"

    defaults = project(
        "pub trait Capable {\n"
        "    fn a(&self) -> Result<(), E> { Err(E::new(\"snapshot not implemented by this backend\")) }\n"
        "    async fn b(&self) -> Result<(), E> {\n        Err(E::Unsupported)\n    }\n"
        "    fn c(&self) -> Result<(), E> { Err(E::Forbidden(\"refused\".into())) }\n"
        "    fn f(&self) -> Result<(), E> { Err(E::Rejected(\"backend does not support claims\".into())) }\n"
        "    fn g(&self) -> Result<(), E> { Err(E::Inactive) }\n"
        "    fn d(&self) -> Result<(), E> { Ok(()) }\n"
        "    fn e(&self) -> Result<(), E>;\n"
        "}\n"
    )
    capability = measure_sources({"d.rs": defaults}).capability_defaults
    assert [entry.split(": ", 1)[1] for entry in capability] == ["Capable::a", "Capable::b", "Capable::f"], f"F1/F2 {capability}"

    counted = Measurement(depth=0, catch_all_errors=["x", "y"])
    rejected = compare({"k": counted}, {"crates": {"k": {"depth": 0, "catch_all_errors": 1}}})
    assert rejected and "x" in rejected[0] and "catch_all_errors: k 1 -> 2" in rejected[0], "R1"
    assert all(instance in rejected[0] for instance in counted.catch_all_errors) and "; fix: " in rejected[0], \
        "D6 every original finding remains with the canonical summary"
    below = {"crates": {"k": {"depth": 0, "catch_all_errors": 3}}}
    assert not compare({"k": counted}, below), "R2 a fall is admitted"
    assert tightenable({"k": counted}, below) == ["k.catch_all_errors: 3 -> 2"], "R2 and reported"
    assert not compare({"k": counted}, {"crates": {"k": {"depth": 0, "catch_all_errors": 2}}}), "R admits"

    previous = {"crates": {"k": {"depth": 0, "catch_all_errors": 1}}}
    _, growth = render_baseline({"k": counted}, previous, None)
    assert growth == ["k.catch_all_errors: 1 -> 2"], f"W1 {growth}"
    document, growth = render_baseline({"k": counted}, previous, "typed split pending")
    assert document["crates"]["k"]["growth_reasons"] == {"catch_all_errors": "typed split pending"}, "W2"
    grown = {"crates": {"k": {"depth": 0, "catch_all_errors": 2, "growth_reasons": {"catch_all_errors": "first"}}}}
    document, _ = render_baseline({"k": Measurement(depth=0, catch_all_errors=["x", "y", "z"])}, grown, "second")
    assert document["crates"]["k"]["growth_reasons"] == {"catch_all_errors": "first; then: second"}, "W3 keeps history"
    oversized = Measurement(depth=0, modules={"x.rs": (PORT_LINE_BUDGET + 10, "ports")})
    document, growth = render_baseline({"k": oversized}, previous, "module split pending")
    assert not growth, f"W4 advisory line growth never needs a reason: {growth}"
    assert document["crates"]["k"]["module_lines"] == {"x.rs": PORT_LINE_BUDGET + 10}, "W4 recorded"
    assert document["crates"]["k"]["growth_reasons"]["module_lines"] == "module split pending", \
        "W4 optional reason still uses original history"
    document, growth = render_baseline({"k": oversized}, previous, None)
    assert not growth and document["crates"]["k"]["module_lines"] == {"x.rs": PORT_LINE_BUDGET + 10}, \
        "W4 actual line measurement needs no reason"
    legacy = {"crates": {"k": {"depth": 0, "growth_reasons": {"production_lines": "per-crate budget"}}}}
    assert "growth_reasons" not in render_baseline({"k": Measurement(depth=0)}, legacy, None)[0]["crates"]["k"], \
        "W5 a reason for a retired rule is dropped"
    lowered, growth = render_baseline({"k": Measurement(depth=0)}, previous, None)
    assert not growth and lowered["crates"]["k"] == {"depth": 0}, "W lowering needs no reason"
    _selftest_cli()
    print("check_contract_shape self-test: the decision table holds")


def _selftest_cli() -> None:
    """CLI cause/effect table, keeping this guard's original fall-is-admitted rule.

    SCLI1 default/check/report never write; a fall remains admitted. SCLI2
    tighten+stage writes/stages a real fall once, equal does neither; growth
    refuses both. SCLI3 reviewed writes retain root/crate metadata and reason
    history; hard DAG/kind failures still refuse before writing. SCLI4 absent
    baseline, empty reasons and invalid modes/stage precede measurement; the
    existing --tighten --stage hook and --report combinations remain valid.
    SCLI5 explicit write with an existing empty crates map is its first record,
    retaining metadata; nonempty growth still requires a reason and neither
    check nor tighten borrows this authorization.
    D8: the formatter changes only returned rule messages. This table owns
    unchanged statuses, root/crate bytes, gain staging, report and init rules.
    SCLI6 line-only growth -> WARN/exit0, read-only checks and no tighten/stage
    write; explicit write records actual lines without a growth reason.
    SCLI7 structural fall + line growth/new module -> stage only the fall,
    clamp existing line allowances and omit new ones; a large line fall still
    tightens. Structural growth or DAG/kind errors stay exit1 with no writes.
    """
    import contextlib
    import io
    from types import SimpleNamespace

    foreign = "  foreign review; expiry unchanged  "
    held = {"owner": "shape", "expiry": "review", "growth_reasons": {"future-counter": {"root": foreign}},
            "future-counter": {"root": 9}, "crates": {"c": {"depth": 0, "catch_all_errors": 2,
            "future-counter": 8,
            "owner": "contract", "expiry": "extraction", "growth_reasons": {"catch_all_errors": "first", "future-counter": foreign}}}}
    stage_calls = []
    diagnostics: list[str] = []

    def invoke(argv, count=2, baseline=held, errors=(), measurement=None):
        stderr = io.StringIO()

        def captured_main(argv):
            with contextlib.redirect_stderr(stderr):
                return main(argv)

        diagnostics.clear()
        result = _architecture_ratchet._cli_fixture(
            captured_main, argv, baseline, "measure",
            (list(errors), {"c": measurement or Measurement(depth=0, catch_all_errors=["x"] * count)}),
            load_contract_crates=lambda: {}, subprocess=SimpleNamespace(run=lambda *a, **kw: stage_calls.append((a, kw))))
        diagnostics.append(stderr.getvalue())
        return result

    for argv in ([], ["--check"], ["--report"], ["--report", "--check"]):
        status, before, after, observed, output = invoke(argv, 1)
        assert status == 0 and before == after and observed, f"SCLI1 fall admitted/read-only {argv}"
        if "--report" in argv:
            assert output.startswith(table({"c": Measurement(depth=0, catch_all_errors=["x"])})), "SCLI1 table"
    for argv in (["--tighten", "--stage"], ["--report", "--tighten", "--stage"]):
        stage_calls.clear()
        status, _, after, _, _ = invoke(argv, 1)
        document = json.loads(after)
        assert status == 0 and document["crates"]["c"]["catch_all_errors"] == 1, "SCLI2 hook-compatible fall"
        assert len(stage_calls) == 1 and stage_calls[0][0][0][:2] == ["git", "add"], "SCLI2 original staging"
        assert document["owner"] == held["owner"] and document["expiry"] == held["expiry"], "SCLI2 root metadata"
        assert document["crates"]["c"]["owner"] == "contract" and document["crates"]["c"]["expiry"] == "extraction", \
            "SCLI2 crate metadata"
        assert document["future-counter"] == held["future-counter"] and document["growth_reasons"] == held["growth_reasons"], \
            "SCLI2 foreign root metadata/reason"
        assert document["crates"]["c"]["future-counter"] == 8 and document["crates"]["c"]["growth_reasons"]["future-counter"] == foreign, \
            "SCLI2 unknown crate counter and its reason stay together"
    stage_calls.clear()
    status, before, after, _, _ = invoke(["--tighten", "--stage"])
    assert status == 0 and before == after and not stage_calls, "SCLI2 equal never stages"
    status, before, after, _, _ = invoke(["--tighten", "--stage"], 3)
    assert status == 1 and before == after and not stage_calls, "SCLI2 growth never stages"
    status, before, after, _, _ = invoke(["--write-baseline"], 3)
    assert status == 1 and before == after, "SCLI3 growth needs reason"
    status, _, after, _, _ = invoke(["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], 3)
    document = json.loads(after)
    assert status == 0 and document["crates"]["c"]["growth_reasons"]["catch_all_errors"] == "first; then: " + _architecture_ratchet._test_growth_reason("reviewed"), \
        "SCLI3 review history"
    assert document["crates"]["c"]["future-counter"] == 8 and document["crates"]["c"]["growth_reasons"]["future-counter"] == foreign, \
        "SCLI3 explicit write preserves foreign metadata/reasons"
    # SCLI5 exists + crates={} is the original first-record authorization,
    # only for explicit writes. Existing nonempty growth above remains reasoned.
    empty = {**held, "crates": {}}
    status, _, after, _, _ = invoke(["--write-baseline"], 3, empty)
    initial = json.loads(after)
    assert status == 0 and initial["crates"]["c"]["catch_all_errors"] == 3, "SCLI5 existing empty initializes"
    assert initial["owner"] == held["owner"] and initial["expiry"] == held["expiry"] and initial["growth_reasons"] == held["growth_reasons"], \
        "SCLI5 initial root metadata/reasons retained"
    for argv in (["--check"], ["--tighten"]):
        status, before, after, _, _ = invoke(argv, 3, empty)
        assert status == 1 and before == after, "SCLI5 initialization is not check/tighten authorization"
    status, before, after, _, _ = invoke(["--write-baseline"], 3, empty, errors=("cycle / wrong kind",))
    assert status == 1 and before == after, "SCLI5 initial recording cannot waive hard rules"
    for argv in (["--check"], ["--tighten", "--stage"], ["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")]):
        stage_calls.clear()
        status, before, after, _, _ = invoke(argv, 1, errors=("cycle / wrong kind",))
        assert status == 1 and before == after and not stage_calls, "SCLI3 hard rules cannot be waived"
    for argv in ([], ["--check"], ["--tighten", "--stage"]):
        status, before, after, observed, _ = invoke(argv, baseline=None)
        assert status == 1 and before is None and after is None and not observed, "SCLI4 missing baseline"
    for argv in (["--check", "--tighten"], ["--self-test", "--write-baseline"], ["--stage"],
                 ["--write-baseline", "--stage", "--reason", _architecture_ratchet._test_growth_reason("reviewed")], ["--tighten", "--reason", "x"],
                 ["--write-baseline", "--reason", "   "], ["--self-test", "--report"]):
        status, before, after, observed, _ = invoke(argv)
        assert status == 2 and before == after and not observed, f"SCLI4 {argv}"

    # SCLI6: new and recorded line growth use exactly the check_all/hook main
    # and canonical compare. Baseline bytes stay unchanged for checks and
    # line-only tightening; warnings retain the original module/old/new counts.
    line_held = {**held, "crates": {"c": {**held["crates"]["c"],
        "module_lines": {"held.rs": PORT_LINE_BUDGET + 10},
        "growth_reasons": {**held["crates"]["c"]["growth_reasons"], "module_lines": "earlier line review"}}}}
    modules = {"held.rs": (PORT_LINE_BUDGET + 20, "ports"), "new.rs": (TYPES_LINE_BUDGET + 1, "types")}
    line_only = Measurement(catch_all_errors=["x", "x"], modules=modules)
    for baseline in (held, line_held):
        for argv in ([], ["--check"], ["--report"], ["--report", "--check"],
                     ["--tighten", "--stage"], ["--report", "--tighten", "--stage"]):
            stage_calls.clear()
            status, before, after, observed, _ = invoke(argv, baseline=baseline, measurement=line_only)
            assert status == 0 and before == after and observed and not stage_calls, f"SCLI6 line-only {argv}"
            warning = diagnostics[0]
            old = PORT_LINE_BUDGET + 10 if baseline is line_held else PORT_LINE_BUDGET
            assert f"WARN: module_lines: c::held.rs {old} -> {PORT_LINE_BUDGET + 20}" in warning \
                and f"WARN: module_lines: c::new.rs {TYPES_LINE_BUDGET} -> {TYPES_LINE_BUDGET + 1}" in warning \
                and "ERROR:" not in warning, "SCLI6 warnings preserve both role measurements"
    status, _, after, _, _ = invoke(["--write-baseline"], baseline=line_held, measurement=line_only)
    document = json.loads(after)
    assert status == 0 and document["crates"]["c"]["module_lines"] == {
        rel: lines for rel, (lines, _) in modules.items()}, "SCLI6 explicit write records actual advisory lines"
    assert document["crates"]["c"]["growth_reasons"] == line_held["crates"]["c"]["growth_reasons"], \
        "SCLI6 historical line/structural/foreign reasons survive unreasoned line writes"

    # SCLI7: each existing/new line allowance, including a newly measured crate,
    # may grow while an independent structural rule falls. The one renderer
    # must record only falls, retain historical metadata/reasons, and stage once.
    for baseline in (held, line_held):
        stage_calls.clear()
        shrinking = Measurement(catch_all_errors=["x"], modules=modules)
        status, _, after, _, _ = invoke(["--tighten", "--stage"], baseline=baseline, measurement=shrinking)
        document = json.loads(after)
        assert status == 0 and len(stage_calls) == 1 and document["crates"]["c"]["catch_all_errors"] == 1, \
            "SCLI7 structural fall still stages"
        assert document["crates"]["c"].get("module_lines", {}) == baseline["crates"]["c"].get("module_lines", {}), \
            "SCLI7 no new/raised line record during structural tightening"
        assert "WARN: module_lines:" in diagnostics[0], "SCLI7 clamped growth still warns"
        assert document["crates"]["c"]["growth_reasons"] == baseline["crates"]["c"]["growth_reasons"], \
            "SCLI7 original line and foreign history retained"
        assert document["owner"] == baseline["owner"] and document["growth_reasons"] == baseline["growth_reasons"], \
            "SCLI7 foreign root metadata unchanged"
    new_crate = {"c": Measurement(catch_all_errors=["x"]), "new": Measurement(modules=modules)}
    document, growth = render_baseline(new_crate, line_held, None, tighten=True)
    assert not growth and "module_lines" not in document["crates"]["new"], "SCLI7 no new crate line allowance"
    falling = {**line_held, "crates": {"c": {**line_held["crates"]["c"],
        "module_lines": {"held.rs": PORT_LINE_BUDGET + 10 + LINE_SLACK}}}}
    stage_calls.clear()
    status, _, after, _, _ = invoke(["--tighten", "--stage"], baseline=falling,
        measurement=Measurement(catch_all_errors=["x", "x"], modules={"held.rs": (PORT_LINE_BUDGET + 10, "ports")}))
    assert status == 0 and len(stage_calls) == 1 and json.loads(after)["crates"]["c"]["module_lines"] == {
        "held.rs": PORT_LINE_BUDGET + 10}, "SCLI7 original slack fall still tightens"

    # SCLI8: adding line growth never downgrades a structural refusal. Even a
    # reviewed explicit write cannot waive hard DAG/kind errors; reasonless
    # structural growth still fails the original write admission.
    for argv in (["--check"], ["--tighten", "--stage"], ["--write-baseline"]):
        stage_calls.clear()
        status, before, after, _, _ = invoke(argv, baseline=line_held,
            measurement=Measurement(catch_all_errors=["x"] * 3, modules=modules))
        assert status == 1 and before == after and not stage_calls, "SCLI8 structural growth remains hard"
        assert "ERROR:" in diagnostics[0], "SCLI8 original structural diagnostic"
        if "--write-baseline" not in argv:
            assert "WARN: module_lines:" in diagnostics[0], "SCLI8 mixed growth still warns"
    for error in ("cycle", "wrong kind"):
        for argv in (["--check"], ["--tighten", "--stage"],
                     ["--write-baseline", "--reason", _architecture_ratchet._test_growth_reason("reviewed")]):
            stage_calls.clear()
            status, before, after, _, _ = invoke(argv, baseline=line_held, errors=(error,), measurement=line_only)
            assert status == 1 and before == after and not stage_calls and error in diagnostics[0], \
                "SCLI8 line advisory cannot waive hard measurement rules"


# --------------------------------------------------------------------------- main


def main(argv: list[str] | None = None) -> int:
    def check(measurements, previous):
        warnings: list[str] = []
        errors = compare(measurements, previous, warnings=warnings)
        for warning in warnings:
            print(f"WARN: {warning}", file=sys.stderr)
        return errors

    def lower(measurements, previous):
        if compare(measurements, previous) or not tightenable(measurements, previous):
            return previous
        document, growth = render_baseline(measurements, previous, None, tighten=True)
        # The original comparison admitted the tree. A disagreement with its
        # existing renderer must never silently write a higher allowance.
        assert not growth, growth
        return document

    return _architecture_ratchet.baseline_cli(
        argv, prog="check_contract_shape.py", description=__doc__, baseline_path=BASELINE, self_test=self_test,
        measure=lambda: measure(load_contract_crates()), check=check, tighten=lower, write=render_baseline,
        report=table, is_initialization=lambda previous: not previous.get("crates"),
        stage=lambda: subprocess.run(["git", "add", str(BASELINE)], cwd=ROOT, check=True),
    )


if __name__ == "__main__":
    raise SystemExit(main())
