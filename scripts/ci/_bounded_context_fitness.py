"""Product and bounded-context dependency rules over crate placement metadata.

Every crate declares `metadata.awaken.product` (sdk / platform / apps /
devtools) and, inside the SDK or the Platform, `metadata.awaken.bounded_context`
(validated by `_crate_dependency_fitness.placement_violations`). This module
turns that placement into dependency direction:

* `sdk-standalone`: an SDK crate depends only on SDK crates, so the Runtime SDK
  builds and publishes without any Platform crate.
* `extension-engine-split`: an SDK extension (`awaken-ext-*`) never depends on
  the engine `awaken-runtime`, and the engine never depends on an extension;
  both meet only at the contracts.
* `context-contract-seam`: a Platform crate reaches another bounded context only
  through that context's `contract`-layer crates, or through the shared
  `fencing` kernel and `storage` infrastructure.
* `protocol-leaf`: no Platform crate outside `protocols` depends on a protocol
  crate; a protocol adapter is mounted only by a composition root.
* `composition-root-leaf`: no SDK or Platform crate depends on an `apps` crate.

Edges that violate these rules today are recorded in
`bounded_context_baseline.json` with the planned change that removes each one.
The baseline only shrinks: a violation missing from it fails, and an entry that
no longer violates fails too, so a removal is recorded in the same commit that
achieves it.
"""

from __future__ import annotations

import json
from collections.abc import Iterable
from pathlib import Path

from _crate_dependency_fitness import CrateSpec

BASELINE = Path(__file__).with_name("bounded_context_baseline.json")

SHARED_CONTEXTS = frozenset({"fencing", "storage"})
ENGINE = "awaken-runtime"
EXTENSION_PREFIX = "awaken-ext-"

Edge = tuple[str, str]

ADVICE = {
    "sdk-standalone": "an SDK crate may depend only on SDK crates; move the needed type into the SDK or behind an SDK port",
    "extension-engine-split": "extensions and the engine meet only at the contracts; depend on awaken-runtime-contract instead",
    "context-contract-seam": "reach another bounded context only through its contract-layer crates, fencing or storage",
    "protocol-leaf": "only protocols and composition roots may depend on a protocol crate; move the shared part behind a contract",
    "composition-root-leaf": "SDK and Platform crates never depend on a composition root; invert the dependency through a port",
}


def _edge_key(edge: Edge) -> str:
    return f"{edge[0]} -> {edge[1]}"


def violations(specs: Iterable[CrateSpec]) -> dict[str, set[Edge]]:
    """Every dependency edge that breaks a placement rule, grouped by rule."""
    by_name = {spec.name: spec for spec in specs}
    found: dict[str, set[Edge]] = {
        "sdk-standalone": set(),
        "extension-engine-split": set(),
        "context-contract-seam": set(),
        "protocol-leaf": set(),
        "composition-root-leaf": set(),
    }
    for source in by_name.values():
        for dependency in source.normal_deps:
            target = by_name.get(dependency)
            if target is None:
                continue
            edge = (source.name, target.name)
            if source.product == "sdk" and target.product != "sdk":
                found["sdk-standalone"].add(edge)
            if (source.name.startswith(EXTENSION_PREFIX) and target.name == ENGINE) or (
                source.name == ENGINE and target.name.startswith(EXTENSION_PREFIX)
            ):
                found["extension-engine-split"].add(edge)
            if source.product in ("sdk", "platform") and target.product == "apps":
                found["composition-root-leaf"].add(edge)
            if source.product != "platform" or target.product != "platform":
                continue
            if source.bounded_context == target.bounded_context:
                continue
            if target.bounded_context == "protocols":
                found["protocol-leaf"].add(edge)
            elif target.layer != "contract" and target.bounded_context not in SHARED_CONTEXTS:
                found["context-contract-seam"].add(edge)
    return found


def load_baseline(path: Path = BASELINE) -> dict[str, dict[str, str]]:
    return json.loads(path.read_text(encoding="utf-8"))


def ratchet_errors(
    found: dict[str, set[Edge]], baseline: dict[str, dict[str, str]]
) -> list[str]:
    errors: list[str] = []
    for rule in sorted(set(found) | set(baseline)):
        if rule not in found:
            errors.append(f"bounded-context baseline names unknown rule {rule!r}")
            continue
        recorded = baseline.get(rule, {})
        current = {_edge_key(edge) for edge in found[rule]}
        for key in sorted(current - set(recorded)):
            errors.append(f"{rule}: new dependency {key}; {ADVICE[rule]}")
        for key in sorted(set(recorded) - current):
            errors.append(
                f"{rule}: {key} no longer violates; remove it from "
                f"{BASELINE.name} in this commit"
            )
        for key, reason in sorted(recorded.items()):
            if not str(reason).strip():
                errors.append(f"{rule}: baseline entry {key} must name the change that removes it")
    return errors


def check_all(specs: Iterable[CrateSpec], baseline_path: Path = BASELINE) -> list[str]:
    return ratchet_errors(violations(specs), load_baseline(baseline_path))


def selftest() -> None:
    """Decision table.

    | Rule | source              | target                         | effect |
    | B1   | sdk                 | sdk                            | accept |
    | B2   | sdk                 | platform (any layer)           | sdk-standalone |
    | B3   | awaken-ext-x        | awaken-runtime                 | extension-engine-split (and accept as sdk) |
    | B4   | awaken-runtime      | awaken-ext-x                   | extension-engine-split |
    | B5   | platform ctx A      | platform ctx A, any layer      | accept |
    | B6   | platform ctx A      | platform ctx B, contract       | accept |
    | B7   | platform ctx A      | platform ctx B, application    | context-contract-seam |
    | B8   | platform ctx A      | fencing / storage, any layer   | accept |
    | B9   | platform ctx A      | protocols                      | protocol-leaf |
    | B10  | protocols           | protocols                      | accept |
    | B11  | platform / sdk      | apps                           | composition-root-leaf |
    | B12  | apps / devtools     | anything                       | accept |
    | B13  | baseline lists a non-violation | -                   | stale entry error |
    | B14  | baseline entry without a reason | -                  | error |
    """

    def spec(name: str, product: str, context: str, layer: str, deps: Iterable[str] = ()) -> CrateSpec:
        return CrateSpec(name, "shared", layer, name, frozenset(deps), product, context)

    def rules(*specs: CrateSpec) -> dict[str, set[Edge]]:
        return {rule: edges for rule, edges in violations(specs).items() if edges}

    sdk_runtime = spec("awaken-runtime", "sdk", "agent-runtime", "application")
    assert rules(spec("a", "sdk", "agent-runtime", "contract", ["b"]),
                 spec("b", "sdk", "agent-runtime", "contract")) == {}, "B1"
    assert rules(spec("a", "sdk", "agent-runtime", "contract", ["p"]),
                 spec("p", "platform", "access", "contract")) == {"sdk-standalone": {("a", "p")}}, "B2"
    assert rules(spec("awaken-ext-x", "sdk", "agent-runtime", "infrastructure", [ENGINE]), sdk_runtime) == {
        "extension-engine-split": {("awaken-ext-x", ENGINE)}
    }, "B3"
    assert rules(sdk_runtime._replace(normal_deps=frozenset({"awaken-ext-x"})),
                 spec("awaken-ext-x", "sdk", "agent-runtime", "infrastructure")) == {
        "extension-engine-split": {(ENGINE, "awaken-ext-x")}
    }, "B4"
    assert rules(spec("a", "platform", "session", "application", ["b"]),
                 spec("b", "platform", "session", "infrastructure")) == {}, "B5"
    assert rules(spec("a", "platform", "session", "application", ["b"]),
                 spec("b", "platform", "dispatch", "contract")) == {}, "B6"
    assert rules(spec("a", "platform", "session", "application", ["b"]),
                 spec("b", "platform", "dispatch", "application")) == {
        "context-contract-seam": {("a", "b")}
    }, "B7"
    assert rules(spec("a", "platform", "session", "application", ["f", "s"]),
                 spec("f", "platform", "fencing", "domain"),
                 spec("s", "platform", "storage", "infrastructure")) == {}, "B8"
    assert rules(spec("a", "platform", "worker", "infrastructure", ["p"]),
                 spec("p", "platform", "protocols", "interface")) == {"protocol-leaf": {("a", "p")}}, "B9"
    assert rules(spec("a", "platform", "protocols", "interface", ["p"]),
                 spec("p", "platform", "protocols", "interface")) == {}, "B10"
    assert rules(spec("a", "platform", "session", "application", ["c"]),
                 spec("c", "apps", "", "bootstrap")) == {"composition-root-leaf": {("a", "c")}}, "B11"
    assert rules(spec("c", "apps", "", "bootstrap", ["a", "t"]),
                 spec("a", "platform", "session", "application"),
                 spec("t", "devtools", "", "tooling", ["a"])) == {}, "B12"
    empty = {rule: set() for rule in violations([])}
    assert ratchet_errors(empty, {"protocol-leaf": {"a -> p": "moves"}}), "B13"
    assert ratchet_errors(
        {**empty, "protocol-leaf": {("a", "p")}}, {"protocol-leaf": {"a -> p": " "}}
    ), "B14"
    assert not ratchet_errors(
        {**empty, "protocol-leaf": {("a", "p")}}, {"protocol-leaf": {"a -> p": "moves"}}
    ), "recorded violation with a reason is accepted"
    print("_bounded_context_fitness selftest: the decision table holds")
