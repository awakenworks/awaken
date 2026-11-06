"""The ADR-0088 context map and SDK–Platform contract, as two ratchets.

ADR-0088 decision 5 makes the context map executable. Both rules read the same
crate placement metadata as `_bounded_context_fitness` and only ever shrink.

* `context-pairs`: a Platform crate may depend on a Platform crate in another
  bounded context only when that (from, to) pair is listed in
  `ALLOWED_CONTEXT_PAIRS`. The shared `fencing` kernel and `storage`
  infrastructure are reachable from every context and are not listed. A listed
  pair that no edge uses any more is an error, so pruning the map is recorded in
  the commit that achieves it.
* `sdk-engine-spi`: only Execution and Committed Truth implement or drive the
  SDK's engine SPI (ADR-0088 decision 4). The SPI is the SPI tier of the
  ADR-0086 amendment of 2026-10-07, its single source:
  - the `awaken-runtime-contract` modules in `RUNTIME_CONTRACT_SPI_MODULES`,
    named by an `awaken_runtime_contract::<module>` path;
  - the `awaken-agent-contract` SPI parts in `AGENT_CONTRACT_SPI_PATHS`
    (`thread::commit`, `stream::sink`, `stream::checkpoint`) and the reader
    traits `CheckpointReader` / `RunRecoverySource` under `thread::read`;
  - a dependency on the engine `awaken-runtime` or any `awaken-ext-*` crate.
  `SPI_EXCEPTIONS` records the one exception the amendment names. Every use
  path (brace groups included) is flattened, so a grouped import counts like a
  plain one; one path site counts each SPI part it names once. Comments and
  `cfg(test)` code are not scanned. A `tooling`-layer crate (conformance kits,
  fixtures) is exempt, as are the `apps` and `devtools` products, which are not
  Platform. Today's sites are counted per crate and SPI part in
  `sdk_engine_spi_baseline.json`; a count may only go down, and a lower count
  is recorded in the same commit.

* `context-order`: the ADR-0088 amendment of 2026-10-07 orders the Platform
  contexts (`CONTEXT_ORDER`, top depends on bottom). A Platform crate edge from
  a lower context to a higher one is an upward edge; today's are listed in
  `context_order_baseline.json`, each with the change that removes it. A new
  upward edge fails, and one that is gone must leave the baseline in the same
  commit. A context the order does not place fails closed.

Known limits of the path rule: the mixed modules `capture`, `permission` and
`data_subject` hold values beside SPI traits, and a path cannot tell which one a
crate names, so they are not checked yet; `awaken-tool-contract`'s executor and
discovery traits are not checked either; and an item reached through a crate-root
re-export (`awaken_runtime_contract::LlmExecutor`) or a renamed import of the
crate is not seen. The value/trait split of the amendment removes the first two.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from collections.abc import Callable, Iterable
from pathlib import Path

import _crate_boundary_workspace
from _bounded_context_fitness import SHARED_CONTEXTS, Edge
from _crate_dependency_fitness import CrateSpec

SPI_BASELINE = Path(__file__).with_name("sdk_engine_spi_baseline.json")
ORDER_BASELINE = Path(__file__).with_name("context_order_baseline.json")

# ADR-0088 decision 5: the context pairs a Platform crate may cross. Seeded from
# the dependency graph when the map was adopted, so the rule started green; each
# pair is an accepted crossing until a reviewer prunes it. Read as
# "<from> may depend on <to>".
ALLOWED_CONTEXT_PAIRS: dict[str, frozenset[str]] = {
    "committed-truth": frozenset(
        {"credentials", "deployment", "dispatch", "environment", "resources", "session"}
    ),
    "config": frozenset({"access", "credentials", "environment", "resources", "session"}),
    "credentials": frozenset({"environment"}),
    "deployment": frozenset({"config", "resources", "session"}),
    "dispatch": frozenset({"credentials", "environment", "execution", "session"}),
    "environment": frozenset(
        {"access", "credentials", "execution", "resources", "sandbox", "session"}
    ),
    # execution -> governance: the Run boundary consults the consent port and the
    # ACP session home erases captured content; ADR-0086 §2 moved those ports out
    # of the Runtime SDK into awaken-data-governance-contract.
    "execution": frozenset(
        {"credentials", "dispatch", "environment", "governance", "protocols", "resources", "sandbox", "session"}
    ),
    "protocols": frozenset(
        {"access", "config", "credentials", "deployment", "dispatch", "environment", "governance", "resources", "sandbox", "session"}
    ),
    "resources": frozenset({"dispatch", "environment", "session"}),
    "sandbox": frozenset({"environment", "resources"}),
    "session": frozenset(
        {"config", "credentials", "deployment", "dispatch", "environment", "resources"}
    ),
}

# ADR-0088 decision 4 and the ADR-0086 amendment of 2026-10-07 (SPI tier).
SPI_CONTEXTS = frozenset({"execution", "committed-truth"})
SPI_EXEMPT_LAYERS = frozenset({"tooling"})
RUNTIME_CONTRACT = "awaken_runtime_contract"
AGENT_CONTRACT = "awaken_agent_contract"
RUNTIME_CONTRACT_SPI_MODULES = frozenset(
    {
        "llm",
        "plugin",
        "execution",
        "resume",
        "pause",
        "control",
        "live_inbox",
        "terminal",
        "attempt_termination",
        "compaction",
        "boundary",
        "metrics",
        "runtime_context",
        "resolver",
        "activation",
        "model_operation",
    }
)
AGENT_CONTRACT_SPI_PATHS = (("thread", "commit"), ("stream", "sink"), ("stream", "checkpoint"))
AGENT_CONTRACT_SPI_READERS = frozenset({"CheckpointReader", "RunRecoverySource"})
# ADR-0086 amendment: `model_operation` is the SDK's evidence-store port and its
# Platform implementation sits in the credentials context until ADR-0088
# classifies it; that is the one recorded exception.
SPI_EXCEPTIONS: dict[str, frozenset[str]] = {
    f"{RUNTIME_CONTRACT}::model_operation": frozenset({"credentials"}),
}
ENGINE = "awaken-runtime"
EXTENSION_PREFIX = "awaken-ext-"

_CONTRACT_ROOT = re.compile(rf"\b({RUNTIME_CONTRACT}|{AGENT_CONTRACT})\s*::")
_PATH_TOKEN = re.compile(r"\s*(::|\{|\}|,|\*|[A-Za-z_][A-Za-z0-9_]*)")


def context_pair_errors(
    specs: Iterable[CrateSpec], allowed: dict[str, frozenset[str]] = ALLOWED_CONTEXT_PAIRS
) -> list[str]:
    by_name = {spec.name: spec for spec in specs}
    used: dict[tuple[str, str], set[Edge]] = {}
    for source in by_name.values():
        if source.product != "platform":
            continue
        for dependency in source.normal_deps:
            target = by_name.get(dependency)
            if target is None or target.product != "platform":
                continue
            if target.bounded_context in (source.bounded_context, *SHARED_CONTEXTS):
                continue
            pair = (source.bounded_context, target.bounded_context)
            used.setdefault(pair, set()).add((source.name, target.name))
    listed = {(origin, target) for origin, targets in allowed.items() for target in targets}
    errors = [
        f"context-pairs: {origin} -> {target} is not an ADR-0088 context pair "
        f"({', '.join(f'{a} -> {b}' for a, b in sorted(edges))}); reach it through an "
        "allowed context or record the pair in ALLOWED_CONTEXT_PAIRS with review"
        for (origin, target), edges in sorted(used.items())
        if (origin, target) not in listed
    ]
    errors += [
        f"context-pairs: no crate depends across {origin} -> {target} any more; "
        "remove it from ALLOWED_CONTEXT_PAIRS in this commit"
        for origin, target in sorted(listed - set(used))
    ]
    return errors


# ADR-0088 amendment of 2026-10-07: the Platform contexts from the top (depends on
# the contexts below it) to the bottom. The shared `fencing` kernel and `storage`
# infrastructure sit under all of them and are not listed.
CONTEXT_ORDER: tuple[str, ...] = (
    "committed-truth",
    "protocols",
    "deployment",
    "config",
    "execution",
    "sandbox",
    "resources",
    "dispatch",
    "environment",
    "access",
    "session",
    "credentials",
    "governance",
)


def context_order_errors(
    specs: Iterable[CrateSpec], baseline: dict, order: tuple[str, ...] = CONTEXT_ORDER
) -> list[str]:
    by_name = {spec.name: spec for spec in specs}
    position = {context: index for index, context in enumerate(order)}
    errors: list[str] = []
    upward: dict[str, tuple[str, str]] = {}
    for source in sorted(by_name.values()):
        if source.product != "platform" or source.bounded_context in SHARED_CONTEXTS:
            continue
        if source.bounded_context not in position:
            errors.append(
                f"context-order: {source.name} is in context {source.bounded_context!r}, which "
                "CONTEXT_ORDER does not place; add it to the ADR-0088 order"
            )
            continue
        for dependency in sorted(source.normal_deps):
            target = by_name.get(dependency)
            if (
                target is None
                or target.product != "platform"
                or target.bounded_context in (source.bounded_context, *SHARED_CONTEXTS)
                or target.bounded_context not in position
            ):
                continue
            if position[target.bounded_context] < position[source.bounded_context]:
                upward[f"{source.name} -> {target.name}"] = (source.bounded_context, target.bounded_context)
    recorded: dict[str, str] = baseline.get("upward-edges", {})
    for key, (origin, target) in sorted(upward.items()):
        if key not in recorded:
            errors.append(
                f"context-order: {key} depends upward from {origin} on {target} (ADR-0088 order); "
                "move the shared part down into a contract the lower context owns, or invert it through a port"
            )
    for key in sorted(set(recorded) - set(upward)):
        errors.append(f"context-order: {key} no longer depends upward; remove it from {ORDER_BASELINE.name} in this commit")
    for key, reason in sorted(recorded.items()):
        if not str(reason).strip():
            errors.append(f"context-order: baseline entry {key} must name the change that removes it")
    return errors


def _flatten_path_tree(source: str, cursor: int) -> list[tuple[str, ...]]:
    """Every path a `::`-continued path or use tree names, starting after `crate::`.

    A `{` is a use group only right after `::`; anywhere else (a struct literal,
    a block) it ends the path. Turbofish and other punctuation end it too.
    """

    def token(at: int) -> tuple[str | None, int]:
        match = _PATH_TOKEN.match(source, at)
        return (match.group(1), match.end()) if match else (None, at)

    def tree(at: int) -> tuple[list[tuple[str, ...]], int]:
        kind, after = token(at)
        if kind == "{":
            paths: list[tuple[str, ...]] = []
            at = after
            while True:
                kind, after = token(at)
                if kind == "}":
                    return paths, after
                if kind == ",":
                    at = after
                    continue
                if kind is None:
                    return paths, at
                inner, moved = tree(at)
                if moved == at:
                    return paths, at
                paths += inner
                at = moved
        if kind is None or kind in {"::", "}", ","}:
            return [()], at
        if kind == "*":
            return [("*",)], after
        separator, past = token(after)
        if separator == "::":
            rest, end = tree(past)
            return [(kind, *path) for path in rest], end
        if separator == "as":
            _, end = token(past)
            return [(kind,)], end
        return [(kind,)], after

    return tree(cursor)[0]


def spi_sites(source: str) -> Counter[str]:
    """SPI parts named by a contract path in already projected Rust source."""
    found: Counter[str] = Counter()
    for match in _CONTRACT_ROOT.finditer(source):
        parts: set[str] = set()
        for path in _flatten_path_tree(source, match.end()):
            if match.group(1) == RUNTIME_CONTRACT:
                if path and path[0] in RUNTIME_CONTRACT_SPI_MODULES:
                    parts.add(f"{RUNTIME_CONTRACT}::{path[0]}")
                continue
            for prefix in AGENT_CONTRACT_SPI_PATHS:
                if path[: len(prefix)] == prefix:
                    parts.add(f"{AGENT_CONTRACT}::{'::'.join(prefix)}")
            if path[:2] == ("thread", "read"):
                parts.update(
                    f"{AGENT_CONTRACT}::thread::read::{name}"
                    for name in path[2:]
                    if name in AGENT_CONTRACT_SPI_READERS
                )
        found.update(parts)
    return found


def spi_findings(
    specs: Iterable[CrateSpec], sources: Callable[[str], Iterable[str]]
) -> tuple[dict[str, dict[str, int]], set[Edge]]:
    """Per-crate SPI path counts and forbidden crate dependencies."""
    paths: dict[str, dict[str, int]] = {}
    crates: set[Edge] = set()
    for spec in specs:
        if (
            spec.product != "platform"
            or spec.bounded_context in SPI_CONTEXTS
            or spec.layer in SPI_EXEMPT_LAYERS
        ):
            continue
        crates.update(
            (spec.name, dependency)
            for dependency in spec.normal_deps
            if dependency == ENGINE or dependency.startswith(EXTENSION_PREFIX)
        )
        counted: Counter[str] = Counter()
        for source in sources(spec.name):
            counted.update(spi_sites(source))
        for part in list(counted):
            if spec.bounded_context in SPI_EXCEPTIONS.get(part, frozenset()):
                del counted[part]
        if counted:
            paths[spec.name] = dict(sorted(counted.items()))
    return paths, crates


def spi_ratchet_errors(
    paths: dict[str, dict[str, int]], crates: set[Edge], baseline: dict
) -> list[str]:
    errors: list[str] = []
    recorded_paths: dict[str, dict[str, int]] = baseline.get("spi-paths", {})
    for crate in sorted(set(paths) | set(recorded_paths)):
        current, recorded = paths.get(crate, {}), recorded_paths.get(crate, {})
        for part in sorted(set(current) | set(recorded)):
            now, held = current.get(part, 0), recorded.get(part, 0)
            if now > held:
                errors.append(
                    f"sdk-engine-spi: {crate} names {part} at {now} site(s), baseline {held}; "
                    "only Execution and Committed Truth use the engine SPI (ADR-0088), so "
                    "take the value from the published language instead"
                )
            elif now < held:
                errors.append(
                    f"sdk-engine-spi: {crate} names {part} at {now} site(s), below the "
                    f"baseline {held}; lower {SPI_BASELINE.name} in this commit"
                )
    recorded_crates: dict[str, str] = baseline.get("crate-dependencies", {})
    current_crates = {f"{source} -> {target}" for source, target in crates}
    for key in sorted(current_crates - set(recorded_crates)):
        errors.append(
            f"sdk-engine-spi: new dependency {key}; only Execution and Committed Truth "
            "depend on the engine or an extension (ADR-0088)"
        )
    for key in sorted(set(recorded_crates) - current_crates):
        errors.append(
            f"sdk-engine-spi: {key} no longer violates; remove it from {SPI_BASELINE.name} in this commit"
        )
    for key, reason in sorted(recorded_crates.items()):
        if not str(reason).strip():
            errors.append(f"sdk-engine-spi: baseline entry {key} must name the change that removes it")
    return errors


def _production_sources(crates: Path) -> Callable[[str], list[str]]:
    manifests = {
        _crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(path)): path
        for path in _crate_boundary_workspace.crate_manifests(crates)
    }

    def sources(crate: str) -> list[str]:
        manifest = manifests.get(crate)
        if manifest is None:
            return []
        return [
            _crate_boundary_workspace.production_rust(path.read_text(encoding="utf-8"))
            for path in sorted(_crate_boundary_workspace.production_module_files([manifest]))
        ]

    return sources


def check_all(
    specs: Iterable[CrateSpec], crates: Path, baseline_path: Path = SPI_BASELINE
) -> list[str]:
    specs = list(specs)
    paths, dependencies = spi_findings(specs, _production_sources(crates))
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    order = json.loads(ORDER_BASELINE.read_text(encoding="utf-8"))
    return (
        context_pair_errors(specs)
        + spi_ratchet_errors(paths, dependencies, baseline)
        + context_order_errors(specs, order)
    )


def selftest() -> None:
    """Decision tables.

    Context pairs (Platform source, Platform target in another context):
    | Rule | cause                                         | effect |
    | P1   | pair listed                                   | accept |
    | P2   | pair not listed                               | error naming the edges |
    | P3   | target is fencing / storage                   | accept, never listed |
    | P4   | same context, or a non-Platform side          | accept, never listed |
    | P5   | listed pair no edge uses                      | stale error |

    SDK engine SPI (Platform crate):
    | Rule | cause                                                        | effect |
    | S1   | outside SPI contexts, `awaken_runtime_contract::llm::X`      | counted |
    | S2   | `awaken_runtime_contract::{llm::X, resolved::{Y}, plugin}`   | llm, plugin counted once each |
    | S3   | value module path (`resolved`), struct literal after a path  | not counted / path ends |
    | S4   | in execution / committed-truth, or tooling layer             | not scanned |
    | S5   | depends on awaken-runtime or awaken-ext-*                    | crate dependency |
    | S6   | count above baseline / below baseline / equal                | error / error / accept |
    | S7   | baseline dependency gone, or recorded without a reason       | error |
    | S8   | comment or `cfg(test)` code (projected by the caller)         | not counted |
    | S9   | agent-contract `thread::commit`, `stream::{sink, checkpoint}` | counted |
    | S10  | `thread::read::{CheckpointReader, transcript::X}`            | the reader counted, transcript not |
    | S11  | `model_operation` from credentials / from another context   | exempt / counted |

    Context order (Platform crates, contexts other than fencing / storage):
    | Rule | cause                                              | effect |
    | O1   | edge from a higher context to a lower one          | accept |
    | O2   | upward edge not in the baseline                    | error |
    | O3   | upward edge in the baseline                        | accept |
    | O4   | baseline edge no longer upward, or gone            | stale error |
    | O5   | source context not placed by the order             | error |
    | O6   | target in fencing / storage, or not Platform       | ignored |
    | O7   | baseline entry without a reason                    | error |
    """

    def spec(name: str, context: str, deps: Iterable[str] = (), layer: str = "infrastructure",
             product: str = "platform") -> CrateSpec:
        return CrateSpec(name, "shared", layer, name, frozenset(deps), product, context)

    allowed = {"session": frozenset({"config"})}
    session_to_config = [spec("s", "session", ["c"]), spec("c", "config")]
    assert context_pair_errors(session_to_config, allowed) == [], "P1"
    unlisted = context_pair_errors([spec("r", "session", ["d"]), spec("d", "dispatch")] +
                                   session_to_config, allowed)
    assert len(unlisted) == 1 and "session -> dispatch" in unlisted[0] and "r -> d" in unlisted[0], "P2"
    assert context_pair_errors(
        session_to_config + [spec("x", "session", ["f", "st"]), spec("f", "fencing"), spec("st", "storage")],
        allowed,
    ) == [], "P3"
    assert context_pair_errors(
        session_to_config + [spec("y", "session", ["z", "k"]), spec("z", "session"),
                             spec("k", "agent-runtime", product="sdk")],
        allowed,
    ) == [], "P4"
    stale = context_pair_errors([spec("s", "session")], allowed)
    assert len(stale) == 1 and "any more" in stale[0], "P5"

    rc, ac = RUNTIME_CONTRACT, AGENT_CONTRACT
    sources = {
        "a": [f"use {rc}::llm::TokenUsage;\nfn f() {{ {rc}::resolved::X {{ llm: 1 }}; }}"],
        "b": [f"use {rc}::{{llm::A, resolved::{{B, llm}}, plugin, llm::C as D}};"],
        "h": [f"use {rc}::llm::A;"],
        "t": [f"use {rc}::llm::A;"],
        "e": [_crate_boundary_workspace.production_rust(
            f"// {rc}::llm::A\n#[cfg(test)]\nmod tests {{ use {rc}::llm::A; }}\n"
        )],
        "g": [f"use {ac}::thread::commit::coordinator::X;\nuse {ac}::stream::{{sink::S, checkpoint}};\n"
              f"use {ac}::thread::read::{{recovery::RunRecoverySource as R, CheckpointReader, transcript::T}};\n"
              f"use {ac}::agent::run::Record;"],
        "m": [f"use {rc}::model_operation::Store;"],
        "n": [f"use {rc}::model_operation::Store;"],
    }
    specs = [
        spec("a", "session"),
        spec("b", "dispatch", ["awaken-runtime", "awaken-ext-memory", "awaken-agent-contract"]),
        spec("h", "execution", ["awaken-runtime"]),
        spec("t", "session", ["awaken-runtime"], layer="tooling"),
        spec("e", "config"),
        spec("g", "session"),
        spec("m", "credentials"),
        spec("n", "config"),
    ]
    paths, dependencies = spi_findings(specs, lambda crate: sources.get(crate, []))
    assert paths["a"] == {f"{rc}::llm": 1}, "S1/S3"
    assert paths["b"] == {f"{rc}::llm": 1, f"{rc}::plugin": 1}, "S2"
    assert "h" not in paths and "t" not in paths, "S4"
    assert dependencies == {("b", "awaken-runtime"), ("b", "awaken-ext-memory")}, "S5/S4"
    assert "e" not in paths, "S8"
    assert paths["g"] == {
        f"{ac}::thread::commit": 1,
        f"{ac}::stream::sink": 1,
        f"{ac}::stream::checkpoint": 1,
        f"{ac}::thread::read::CheckpointReader": 1,
        f"{ac}::thread::read::RunRecoverySource": 1,
    }, "S9/S10"
    assert "m" not in paths and paths["n"] == {f"{rc}::model_operation": 1}, "S11"
    held = {
        "spi-paths": {crate: dict(parts) for crate, parts in paths.items()},
        "crate-dependencies": {"b -> awaken-runtime": "moves", "b -> awaken-ext-memory": "moves"},
    }
    assert spi_ratchet_errors(paths, dependencies, held) == [], "S6 equal"
    above = spi_ratchet_errors(paths, dependencies, {**held, "spi-paths": {**held["spi-paths"], "a": {}}})
    assert len(above) == 1 and "baseline 0" in above[0], "S6 above"
    below = spi_ratchet_errors(
        paths, dependencies, {**held, "spi-paths": {**held["spi-paths"], "a": {f"{rc}::llm": 2}}}
    )
    assert len(below) == 1 and "below the baseline" in below[0], "S6 below"
    gone = spi_ratchet_errors(paths, {("b", "awaken-runtime")}, held)
    assert len(gone) == 1 and "no longer violates" in gone[0], "S7 gone"
    unreasoned = {**held, "crate-dependencies": {**held["crate-dependencies"], "b -> awaken-runtime": " "}}
    assert spi_ratchet_errors(paths, dependencies, unreasoned), "S7 reason"
    order = ("top", "middle", "bottom")
    graph = [spec("t", "top", ["m", "f"]), spec("m", "middle", ["b"]), spec("b", "bottom", ["k"]),
             spec("f", "fencing"), spec("k", "agent-runtime", product="sdk")]
    assert context_order_errors(graph, {}, order) == [], "O1/O6"
    upward = [spec("t", "top"), spec("b", "bottom", ["t"])]
    found = context_order_errors(upward, {}, order)
    assert len(found) == 1 and "b -> t depends upward" in found[0], "O2"
    held = {"upward-edges": {"b -> t": "moves with the port"}}
    assert context_order_errors(upward, held, order) == [], "O3"
    stale = context_order_errors([spec("t", "top"), spec("b", "bottom")], held, order)
    assert len(stale) == 1 and "no longer depends upward" in stale[0], "O4"
    assert "does not place" in context_order_errors([spec("x", "elsewhere")], {}, order)[0], "O5"
    assert "must name" in context_order_errors(upward, {"upward-edges": {"b -> t": " "}}, order)[0], "O7"
    print("_context_map_fitness selftest: the decision tables hold")

