#!/usr/bin/env python3
"""Ratchet the shape of the contract layer so it can only get simpler.

A crate whose Cargo metadata declares ``layer = "contract"`` is the published
seam between bounded contexts (ADR-0059, ADR-0081). Five shapes make such a
seam harder to consume and evolve, and every one of them predates this gate:

1. **Graph** -- contract crates must form a DAG, and a crate's depth (longest
   chain of contract dependencies below it) may not grow. Every level of depth
   is another crate a consumer compiles and another place vocabulary can hide.
2. **Size** -- a contract that grows without bound stops being a seam. A crate
   whose production code declares no trait carrying methods is a *types* crate
   (vocabulary and its laws, budget ``TYPES_LINE_BUDGET``); every other contract
   crate carries ports (budget ``PORT_LINE_BUDGET``). Growth is free under the
   budget; above it a crate may not exceed its recorded line count.
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

Every count is compared with ``scripts/ci/contract_shape_baseline.json``. A
count above its baseline fails and names the instances; a count below it fails
too, asking for the baseline to be lowered so the gained ground is held (line
counts only once they shrink by ``LINE_SLACK`` or fall under budget, so ordinary
edits do not churn the file). ``--write-baseline`` records the current tree; it
refuses to raise a number unless ``--reason`` explains the growth, and that
reason is kept next to the number for review.

Production source is the structural module graph from each Cargo target root,
with ``cfg(test)`` and ``cfg(kani)`` items and all comments removed -- the same
projection every crate-boundary fitness check uses.
"""

from __future__ import annotations

import argparse
import subprocess
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

import _crate_boundary_workspace as workspace

# Every Cargo manifest and production Rust file can add, remove or reshape a
# contract crate; the baseline and the shared lexer define the comparison.
INPUT_GLOBS: tuple[str, ...] = (
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "scripts/ci/_crate_boundary_workspace.py",
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

RULES = ("depth", "production_lines", "backend_nouns", "catch_all_errors", "capability_defaults")
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


@dataclass
class Measurement:
    depth: int = 0
    production_lines: int = 0
    role: str = "types"
    backend_nouns: list[str] = field(default_factory=list)
    catch_all_errors: list[str] = field(default_factory=list)
    capability_defaults: list[str] = field(default_factory=list)

    def count(self, rule: str) -> int:
        value = getattr(self, rule)
        return value if isinstance(value, int) else len(value)

    def budget(self) -> int:
        return TYPES_LINE_BUDGET if self.role == "types" else PORT_LINE_BUDGET


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
        measurement.production_lines += sum(1 for line in production.splitlines() if line.strip())

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
        "contract dependency cycle: " + " -> ".join(cycle + cycle[:1])
        + ". Contracts are the seams between contexts; move the shared vocabulary into the "
        "lower crate so the edge points one way"
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
            f"{name}: rule kinds: [package.metadata.awaken] declares no `kinds`. Fix: declare the "
            f"ADR-0081 interface kinds this crate holds, e.g. kinds = [\"types\", \"spi\"]"
        ]
    unknown = sorted(kinds - set(INTERFACE_KINDS))
    if unknown:
        return [
            f"{name}: rule kinds: unknown kind(s) {unknown}. Fix: use only "
            f"{list(INTERFACE_KINDS)} (ADR-0081 §1)"
        ]
    holds_ports = measurement.role == "ports"
    if not (kinds & PORT_KINDS) and holds_ports:
        return [
            f"{name}: rule kinds: declared types-only but holds a trait with methods. Fix: declare "
            f"the port kind it holds (spi/api/remote/ext) or move the trait to the crate that owns it"
        ]
    if (kinds & PORT_KINDS) and not holds_ports:
        return [
            f"{name}: rule kinds: declares {sorted(kinds & PORT_KINDS)} but holds no trait with "
            f"methods. Fix: declare kinds = [\"types\"] so the budget and direction rules apply "
            f"to what is really there"
        ]
    return []


# --------------------------------------------------------------------------- ratchet

HOW_TO_FIX = {
    "depth": "depend on the lower contract that owns the vocabulary instead of one that re-exports it",
    "production_lines": "split cohesive vocabulary into its own crate (ADR-0081 split-facade policy) "
    "or move behaviour out of the contract",
    "backend_nouns": "move the backend-named type into its provider/adapter crate and keep a neutral "
    "name in the contract (ADR-0059 §1)",
    "catch_all_errors": "give the failure a typed variant the caller can classify instead of a message",
    "capability_defaults": "require the method, or express the optional capability as a supertrait "
    "the capable implementations add (ADR-0081 capability ladder)",
}


def compare(measurements: dict[str, Measurement], baseline: dict) -> list[str]:
    recorded: dict[str, dict] = baseline.get("crates", {})
    max_depth = max((entry.get("depth", 0) for entry in recorded.values()), default=0)
    errors: list[str] = []
    for name, current in sorted(measurements.items()):
        entry = recorded.get(name)
        known = entry is not None
        entry = entry or {}
        # depth
        base_depth = entry.get("depth", max_depth)
        if current.depth > base_depth:
            errors.append(
                f"{name}: rule depth: contract depth {current.depth} exceeds baseline {base_depth}. "
                f"Fix: {HOW_TO_FIX['depth']}"
            )
        # size
        budget = current.budget()
        base_lines = entry.get("production_lines")
        if current.production_lines > budget and (
            base_lines is None or current.production_lines > base_lines
        ):
            limit = budget if base_lines is None else base_lines
            errors.append(
                f"{name}: rule production_lines: {current.production_lines} production lines exceed "
                f"the {current.role} budget {budget} and the baseline {limit}. Fix: "
                f"{HOW_TO_FIX['production_lines']}; growth that is truly needed is recorded with "
                "--write-baseline --reason"
            )
        # counted instances
        for rule in RULES[2:]:
            count = current.count(rule)
            base = entry.get(rule, 0)
            if count > base:
                instances = "\n    ".join(getattr(current, rule))
                errors.append(
                    f"{name}: rule {rule}: {count} instance(s), baseline {base}. Fix: "
                    f"{HOW_TO_FIX[rule]}.\n    {instances}"
                )
    return errors


def tightenable(measurements: dict[str, Measurement], baseline: dict) -> list[str]:
    """Gains the baseline has not recorded yet.

    A fall used to fail the gate until someone hand-lowered the number, which
    made the baseline the file most often co-edited with contract code (26 of
    its 42 edits in 500 commits only lowered a number). A fall is now admitted
    here and recorded mechanically by ``--tighten``, which the pre-commit hook
    runs and stages, so the gain is still kept and no person edits the file for
    it. Only growth needs a human, with ``--write-baseline --reason``.
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
        base_lines = entry.get("production_lines")
        if base_lines is not None and (
            current.production_lines <= current.budget()
            or base_lines - current.production_lines >= LINE_SLACK
        ):
            gains.append(f"{name}.production_lines: {base_lines} -> {current.production_lines}")
        for rule in RULES[2:]:
            if current.count(rule) < entry.get(rule, 0):
                gains.append(f"{name}.{rule}: {entry[rule]} -> {current.count(rule)}")
    return gains


def render_baseline(
    measurements: dict[str, Measurement], previous: dict, reason: str | None
) -> tuple[dict, list[str]]:
    """New baseline plus the growths that ``reason`` must justify."""
    old = previous.get("crates", {})
    growth: list[str] = []
    crates: dict[str, dict] = {}
    for name, current in sorted(measurements.items()):
        entry: dict = {"depth": current.depth}
        if current.production_lines > current.budget():
            entry["production_lines"] = current.production_lines
        for rule in RULES[2:]:
            if current.count(rule):
                entry[rule] = current.count(rule)
        prior = old.get(name, {})
        reasons = dict(prior.get("growth_reasons", {}))
        for rule in RULES:
            if rule not in entry:
                reasons.pop(rule, None)
                continue
            if rule == "depth":
                before = prior.get(rule) if name in old else None
            elif rule == "production_lines":
                before = prior.get(rule, current.budget())
            else:
                before = prior.get(rule, 0)
            if before is not None and entry[rule] > before:
                growth.append(f"{name}.{rule}: {before} -> {entry[rule]}")
                if reason:
                    # Each growth keeps the reasons recorded for earlier growth:
                    # the baseline is the history of why a number may be this high.
                    earlier = reasons.get(rule)
                    reasons[rule] = reason if not earlier or reason in earlier else f"{earlier}; then: {reason}"
        if reasons:
            entry["growth_reasons"] = reasons
        crates[name] = entry
    document = {
        "$comment": (
            "Contract-shape ratchet (scripts/ci/check_contract_shape.py, ADR-0081). Numbers may "
            "only fall; raising one requires --write-baseline --reason, recorded in growth_reasons."
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
    C3 lines above budget and above/below baseline; C4 backend noun in a
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
    | L1   | C3 ports crate over budget, no baseline          | reject                       |
    | L2   | C3 over budget, <= baseline                      | admit                        |
    | L3   | C3 shrinks by >= LINE_SLACK / under budget       | admit; --tighten records it  |
    | L5   | recorded crate no longer a contract crate        | admit; --tighten drops it    |
    | L4   | C3 trait-free crate under TYPES budget           | admit, role types            |
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

    port = "pub trait Port {\n    fn call(&self);\n}\n"
    big = Measurement(depth=0, production_lines=PORT_LINE_BUDGET + 10, role="ports")
    assert compare({"p": big}, {"crates": {}}), "L1"
    assert not compare({"p": big}, {"crates": {"p": {"depth": 0, "production_lines": PORT_LINE_BUDGET + 10}}}), "L2"
    shrunk = {"crates": {"p": {"depth": 0, "production_lines": PORT_LINE_BUDGET + 10 + LINE_SLACK}}}
    assert not compare({"p": big}, shrunk) and tightenable({"p": big}, shrunk), "L3 slack"
    small = Measurement(depth=0, production_lines=PORT_LINE_BUDGET - 1, role="ports")
    under = {"crates": {"p": {"depth": 0, "production_lines": PORT_LINE_BUDGET + 5}}}
    assert not compare({"p": small}, under) and tightenable({"p": small}, under), "L3 budget"
    gone = {"crates": {"p": {"depth": 0}, "retired": {"depth": 1}}}
    assert tightenable({"p": small}, gone) == ["retired: recorded but no longer a contract crate"], "L5"
    assert measure_sources({"x.rs": project(port)}).role == "ports", "L4 ports"
    private_helper = "trait Members {\n    fn insert(&mut self);\n}\n"
    assert measure_sources({"x.rs": project(private_helper)}).role == "types", "L4 private helper"
    ports_measured = measure_sources({"x.rs": project(port)})
    types_measured = measure_sources({"x.rs": project("pub struct Value;\n")})
    assert kind_errors("k", None, ports_measured), "K1"
    assert kind_errors("k", frozenset({"types", "backend"}), types_measured), "K2"
    assert kind_errors("k", frozenset({"types"}), ports_measured), "K3"
    assert kind_errors("k", frozenset({"types", "spi"}), types_measured), "K4"
    assert not kind_errors("k", frozenset({"types", "spi"}), ports_measured), "K5 ports"
    assert not kind_errors("k", frozenset({"types"}), types_measured), "K5 types"
    vocabulary = Measurement(depth=0, production_lines=PORT_LINE_BUDGET + 10)
    marker = "pub trait Marker {}\npub struct Value;\n"
    assert measure_sources({"x.rs": project(marker)}).role == "types", "L4 marker trait is not a port"
    assert not compare({"t": vocabulary}, {"crates": {}}), "L4"

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
    assert rejected and "x" in rejected[0] and "baseline 1" in rejected[0], "R1"
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
    lowered, growth = render_baseline({"k": Measurement(depth=0)}, previous, None)
    assert not growth and lowered["crates"]["k"] == {"depth": 0}, "W lowering needs no reason"
    print("check_contract_shape self-test: the decision table holds")


# --------------------------------------------------------------------------- main


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    parser.add_argument("--report", action="store_true", help="print the per-crate measurement table")
    parser.add_argument("--write-baseline", action="store_true", help="record the current tree")
    parser.add_argument("--reason", help="why a recorded number is allowed to grow")
    parser.add_argument(
        "--tighten",
        action="store_true",
        help="check, then lower every recorded number the tree has fallen below (never raises one)",
    )
    parser.add_argument(
        "--stage", action="store_true", help="with --tighten: git-add the lowered baseline"
    )
    args = parser.parse_args(argv)

    if args.self_test:
        self_test()
        return 0

    errors, measurements = measure(load_contract_crates())
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    if args.report:
        print(table(measurements))
    previous = json.loads(BASELINE.read_text(encoding="utf-8")) if BASELINE.exists() else {"crates": {}}

    if args.write_baseline:
        document, growth = render_baseline(measurements, previous, args.reason)
        if growth and not args.reason and previous.get("crates"):
            for item in growth:
                print(f"ERROR: baseline would grow: {item}", file=sys.stderr)
            print("Pass --reason '<why this contract must grow>' to record the growth.", file=sys.stderr)
            return 1
        BASELINE.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {BASELINE.relative_to(ROOT)} for {len(measurements)} contract crates")
        return 0

    if args.stage and not args.tighten:
        print("usage: --stage only applies to --tighten", file=sys.stderr)
        return 2
    problems = compare(measurements, previous)
    if args.tighten and not problems:
        gains = tightenable(measurements, previous)
        if gains:
            document, growth = render_baseline(measurements, previous, None)
            # compare() admitted the tree, so nothing may have grown; a growth
            # here would be a disagreement between the two and must not be
            # written silently.
            assert not growth, growth
            BASELINE.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
            for gain in gains:
                print(f"tightened {gain}")
            if args.stage:
                subprocess.run(["git", "add", str(BASELINE)], cwd=ROOT, check=True)
    if problems:
        for problem in problems:
            print(f"ERROR: {problem}", file=sys.stderr)
        print(
            f"\n{len(problems)} contract-shape ratchet violation(s). Reproduce with "
            "`python3 scripts/ci/check_contract_shape.py --report`.",
            file=sys.stderr,
        )
        return 1
    print(f"OK - contract shape held for {len(measurements)} contract crates.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
