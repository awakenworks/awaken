"""Dispatch hand-off stays apart from the Session executor and Worker-side effects.

Plan v4 D4 and section 3.3, principle P4 (dependencies point to the more
stable side). Dispatch owns the hand-off of claimed work: worker resolution,
placement, claim-failure mapping, the envelope codec, cancellation, the
terminal scan and the pool. It reaches the Session realization only through
the role traits its resolver is constructed with, and plugins declare their
auxiliary Agents through runtime-contract. The compiler cannot see a
dependency edge come back, so three rules hold it:

* D1 ``awaken-dispatch-execution`` has no normal dependency on
  ``awaken-session-realization`` or on any ``awaken-ext-*`` crate.
* D2 The Coordinator's dispatch path -- ``awaken-dispatch-execution``,
  ``awaken-dispatch`` (the Coordinator's dispatch adapter) and
  ``awaken-dispatch-http`` -- has no Worker-side implementation crate in its
  normal dependency closure.
* D3 The Coordinator ``awaken-coordinator`` has neither the Session executor
  (``awaken-session-executor``) nor any Worker-side implementation crate in
  its normal dependency closure. It consumes the executor through the
  execution ports a composition root selects; the root that is both
  Coordinator and Worker links the executor and realization, the Coordinator
  crate never does.
* D4 The Coordinator product binary (``awaken-coordinator``) is checked over
  the normal dependency closure of the package that declares it, because Cargo
  resolves dependencies per package: every target of that package links that
  closure. Each Worker-side crate or the Session executor the closure reaches
  must carry a reviewed transition in ``coordinator_binary_closure_baseline.json``
  (a complete D10 review record); a new reach is refused, and a transition whose
  crate is no longer reached must be removed with ``--tighten``. The binary is
  a target of ``awaken-cli``, the AllInOne root, until the Coordinator process
  startup and the Coordinator-side Session execution leave the executor (see
  ``docs/design/runtime-interface-boundaries.md#coordinator-only-composition``);
  the transitions record exactly what remains.

Why this is the correct check: the section 3.3 constraint -- no Worker-side
implementation anywhere in the Coordinator's closure -- is a property of the
dependency graph, which the compiler cannot see from inside one crate. D2
holds it for the dispatch path this boundary owns, the edge slice E
introduced (Coordinator -> run-ingress -> dispatch-execution ->
realization); D3 holds it for the Coordinator itself. The executor is named
explicitly because it is the Worker's execution engine even where it reaches
the realization only through role traits.

A Worker-side implementation crate is ``awaken-session-realization``,
``awaken-credential-materializer``, an ``awaken-sandbox*`` crate that is
neither a contract nor a Control-plane store, or a Worker composition root (a
``bootstrap`` crate of the ``worker`` context). Optional dependencies count:
an enabled feature must not reopen the edge.
"""

from __future__ import annotations

import json
import sys
from collections.abc import Iterable, Mapping
from pathlib import Path

import _architecture_ratchet
import _crate_boundary_workspace
import _crate_dependency_fitness

DISPATCH_EXECUTION = "awaken-dispatch-execution"
DISPATCH_FORBIDDEN = "awaken-session-realization"
DISPATCH_PATH: tuple[str, ...] = (
    "awaken-dispatch-execution",
    "awaken-dispatch",
    "awaken-dispatch-http",
)
COORDINATOR = "awaken-coordinator"

# The Worker's execution engine: never in the Coordinator's closure (D3).
SESSION_EXECUTOR = "awaken-session-executor"

# The Coordinator product binary whose package closure D4 checks.
COORDINATOR_BINARY = "awaken-coordinator"
CLOSURE_BASELINE = Path(__file__).with_name("coordinator_binary_closure_baseline.json")
CLOSURE_COMMENT = (
    "Reviewed transitions of scripts/ci/_dispatch_boundary_fitness.py rule D4: each Worker-side crate the "
    "awaken-coordinator binary's package still links, with its D10 review record. Shrink with --tighten; a new "
    "entry needs --write-baseline --reason '<JSON review record>'."
)

Specs = Mapping[str, _crate_dependency_fitness.CrateSpec]


def is_worker_side(spec: _crate_dependency_fitness.CrateSpec) -> bool:
    """A Worker-side implementation crate (see the module docs)."""
    if spec.name in ("awaken-session-realization", "awaken-credential-materializer"):
        return True
    if spec.context == "worker" and spec.layer == "bootstrap":
        return True
    return (
        spec.name.startswith("awaken-sandbox")
        and spec.layer != "contract"
        and spec.context != "control"
    )


def closure(specs: Specs, root: str) -> set[str]:
    """The workspace crates reachable from ``root`` by normal dependencies."""
    seen: set[str] = set()
    pending = [root]
    while pending:
        for dependency in specs[pending.pop()].normal_deps:
            if dependency in specs and dependency not in seen:
                seen.add(dependency)
                pending.append(dependency)
    return seen


def dispatch_boundary_errors(specs: Specs) -> list[str]:
    """Violations of D1-D3.

    Cause/effect graph: C1 dispatch-execution names realization or an
    extension directly; C2 a dispatch-path crate reaches a Worker-side
    implementation; C3 the Coordinator reaches a Worker-side implementation
    or the Session executor. E1 accept when none holds; E2 reject naming the
    crate and the rule.
    """
    errors: list[str] = []
    if DISPATCH_EXECUTION in specs:
        for dependency in sorted(specs[DISPATCH_EXECUTION].normal_deps):
            if dependency == DISPATCH_FORBIDDEN or dependency.startswith("awaken-ext-"):
                errors.append(
                    f"dispatch-boundary: {DISPATCH_EXECUTION} depends on {dependency}; "
                    "dispatch reaches the realization through its resolver's role traits "
                    "and plugins declare auxiliary Agents through runtime-contract (D1)"
                )
    for crate in DISPATCH_PATH:
        if crate not in specs:
            continue
        reached = sorted(name for name in closure(specs, crate) if is_worker_side(specs[name]))
        if reached:
            errors.append(
                f"dispatch-boundary: the Coordinator dispatch path crate {crate} reaches "
                f"Worker-side implementation {', '.join(reached)} (D2)"
            )
    if COORDINATOR in specs:
        reached = sorted(
            name
            for name in closure(specs, COORDINATOR)
            if name == SESSION_EXECUTOR or is_worker_side(specs[name])
        )
        if reached:
            errors.append(
                f"dispatch-boundary: {COORDINATOR} reaches {', '.join(reached)}; the "
                "Coordinator composes dispatch and consumes the executor through the "
                "execution ports its composition root selects (D3)"
            )
    return errors


def binary_package(binaries: Mapping[str, Iterable[str]], binary: str) -> str | None:
    """The one package that declares ``binary`` as a target, if exactly one does."""
    owners = sorted(package for package, targets in binaries.items() if binary in targets)
    return owners[0] if len(owners) == 1 else None


def coordinator_binary_reach(specs: Specs, package: str) -> list[str]:
    """The Worker-side crates and the Session executor ``package`` links (D4)."""
    linked = closure(specs, package) | {package}
    return sorted(
        name for name in linked if name == SESSION_EXECUTOR or is_worker_side(specs[name])
    )


def coordinator_binary_errors(package: str | None, reached: Iterable[str],
                              transitions: Mapping[str, object]) -> list[str]:
    """Violations of D4.

    Cause/effect graph: C1 no single package declares the binary; C2 the
    package reaches a crate with no transition; C3 a transition names a crate
    the package no longer reaches; C4 a transition lacks a complete review
    record. E1 accept when none holds; E2 reject naming the crate and the fix.
    """
    if package is None:
        return [f"dispatch-boundary: no single package declares the {COORDINATOR_BINARY} binary, so its "
                "closure cannot be checked (D4)"]
    reached = set(reached)
    errors = [
        f"dispatch-boundary: the {COORDINATOR_BINARY} binary's package {package} reaches {name} with no "
        f"reviewed transition in {CLOSURE_BASELINE.name}; compose the Coordinator without it (D4)"
        for name in sorted(reached - set(transitions))
    ]
    errors += [
        f"dispatch-boundary: {CLOSURE_BASELINE.name} names {name}, which {package} no longer reaches; "
        "remove it with `python3 scripts/ci/_dispatch_boundary_fitness.py --tighten` (D4)"
        for name in sorted(set(transitions) - reached)
    ]
    for name, record in sorted(transitions.items()):
        try:
            _architecture_ratchet.validate_growth_reason(json.dumps(record))
        except ValueError:
            errors.append(f"dispatch-boundary: the {name} transition in {CLOSURE_BASELINE.name} needs a "
                          "complete review record: reason, approved_by, date, remove_when (D4)")
    return errors


def workspace_binaries() -> dict[str, list[str]]:
    """Each workspace package and the binary targets it declares."""
    binaries: dict[str, list[str]] = {}
    for manifest_path in _crate_boundary_workspace.iter_crate_manifests():
        manifest = _crate_boundary_workspace.load_manifest(manifest_path)
        binaries[_crate_boundary_workspace.package_name(manifest)] = [
            str(target["name"]) for target in manifest.get("bin", []) if "name" in target
        ]
    return binaries


def _measure_coordinator_binary() -> tuple[list[str], tuple[str | None, list[str]]]:
    specs = {spec.name: spec for spec in _crate_boundary_workspace.dependency_fitness_specs()}
    package = binary_package(workspace_binaries(), COORDINATOR_BINARY)
    reached = coordinator_binary_reach(specs, package) if package else []
    return [], (package, reached)


def _transitions(document: Mapping) -> dict[str, object]:
    return dict(document.get("transitions", {}))


def _check_closure(measured: tuple[str | None, list[str]], document: Mapping) -> list[str]:
    return coordinator_binary_errors(*measured, _transitions(document))


def _tighten_closure(measured: tuple[str | None, list[str]], document: Mapping) -> dict:
    _, reached = measured
    kept = {name: record for name, record in _transitions(document).items() if name in reached}
    return {"$comment": CLOSURE_COMMENT, "transitions": dict(sorted(kept.items()))}


def _write_closure(measured: tuple[str | None, list[str]], document: Mapping,
                   reason: str | None) -> tuple[dict, list[str]]:
    _, reached = measured
    held = _transitions(document)
    grown = [name for name in reached if name not in held]
    record = json.loads(reason) if reason else None
    transitions = {name: held.get(name, record) for name in reached if name in held or record is not None}
    return {"$comment": CLOSURE_COMMENT, "transitions": dict(sorted(transitions.items()))}, grown


def check_coordinator_binary() -> list[str]:
    if not CLOSURE_BASELINE.exists():
        return [f"dispatch-boundary: {CLOSURE_BASELINE.name} is missing; restore it (D4)"]
    _, measured = _measure_coordinator_binary()
    return _check_closure(measured, json.loads(CLOSURE_BASELINE.read_text(encoding="utf-8")))


def _specs(rows: Iterable[tuple[str, str, str, set[str]]]) -> dict[str, _crate_dependency_fitness.CrateSpec]:
    return {
        name: _crate_dependency_fitness.CrateSpec(
            name=name,
            context=context,
            layer=layer,
            authority=name,
            normal_deps=frozenset(deps),
        )
        for name, context, layer, deps in rows
    }


def selftest() -> None:
    """Decision table for the dispatch boundary.

    | Rule | Graph                                                        | Effect |
    |---|---|---|
    | B1 | dispatch-execution reaches only contracts; the Coordinator reaches neither the executor nor a Worker-side crate | accept |
    | B2 | dispatch-execution depends on awaken-session-realization     | reject D1 |
    | B3 | dispatch-execution depends on an awaken-ext-* crate          | reject D1 (and D2 only if Worker-side) |
    | B4 | run-ingress reaches a Worker-side Sandbox through a library  | reject D2 |
    | B5 | a Sandbox contract or a Control-plane Sandbox store reached  | accept (not Worker-side) |
    | B6 | the Coordinator depends on a Worker-side crate directly      | reject D3 |
    | B7 | the Coordinator reaches the executor, through a library      | reject D3 |
    | B8 | the Coordinator depends on the realization directly; its contract stays reachable | reject D3 naming only the realization |
    | B9 | the Coordinator binary's package reaches the executor through a Worker composition root, both with reviewed transitions | accept |
    | B10 | that package reaches a Worker-side crate without a transition | reject D4 naming the crate |
    | B11 | a transition names a crate the package no longer reaches | reject D4: tighten |
    | B12 | a transition carries no complete review record | reject D4 |
    | B13 | no package, or two packages, declare the binary | reject D4 |
    | B14 | the Coordinator crate depends on a Worker composition root | reject D3 |
    """
    base = [
        ("awaken-dispatch-execution", "shared", "infrastructure", {"awaken-dispatch-contract"}),
        ("awaken-dispatch-contract", "shared", "contract", set()),
        ("awaken-dispatch", "coordinator", "infrastructure", {"awaken-dispatch-execution"}),
        ("awaken-dispatch-http", "coordinator", "interface", {"awaken-dispatch"}),
        ("awaken-session-realization", "shared", "infrastructure", {"awaken-sandbox-control"}),
        ("awaken-sandbox-control", "worker", "contract", set()),
        ("awaken-sandbox-local", "worker", "infrastructure", {"awaken-sandbox-control"}),
        ("awaken-sandbox-policy-store", "control", "infrastructure", set()),
        ("awaken-session-executor", "shared", "infrastructure", {"awaken-dispatch-contract"}),
        ("awaken-ext-memory", "runtime", "infrastructure", set()),
        ("awaken-session-application", "shared", "application", {"awaken-dispatch-contract"}),
        ("awaken-coordinator", "coordinator", "bootstrap", {"awaken-dispatch", "awaken-session-application"}),
        ("awaken-worker", "worker", "bootstrap", {"awaken-session-executor"}),
    ]

    def with_edge(name: str, *added: str) -> dict[str, _crate_dependency_fitness.CrateSpec]:
        rows = [(n, c, l, set(d) | (set(added) if n == name else set())) for n, c, l, d in base]
        return _specs(rows)

    assert dispatch_boundary_errors(_specs(base)) == [], "B1"
    b2 = dispatch_boundary_errors(with_edge("awaken-dispatch-execution", "awaken-session-realization"))
    assert any("(D1)" in error for error in b2) and any("(D2)" in error for error in b2), "B2"
    b3 = dispatch_boundary_errors(with_edge("awaken-dispatch-execution", "awaken-ext-memory"))
    assert [error for error in b3 if "(D1)" in error] and not [e for e in b3 if "(D2)" in e], "B3"
    b4 = dispatch_boundary_errors(with_edge("awaken-dispatch", "awaken-sandbox-local"))
    assert any("awaken-dispatch reaches Worker-side implementation awaken-sandbox-local" in e for e in b4), "B4"
    assert dispatch_boundary_errors(
        with_edge("awaken-dispatch", "awaken-sandbox-control", "awaken-sandbox-policy-store")
    ) == [], "B5"
    b6 = dispatch_boundary_errors(with_edge("awaken-coordinator", "awaken-sandbox-local"))
    assert any("awaken-coordinator reaches awaken-sandbox-local" in e and "(D3)" in e for e in b6), "B6"
    b7 = dispatch_boundary_errors(with_edge("awaken-session-application", "awaken-session-executor"))
    assert any("awaken-coordinator reaches awaken-session-executor" in e for e in b7), "B7"
    b8 = dispatch_boundary_errors(with_edge("awaken-coordinator", "awaken-session-realization"))
    assert [e for e in b8 if "(D3)" in e] == [
        "dispatch-boundary: awaken-coordinator reaches awaken-session-realization; the "
        "Coordinator composes dispatch and consumes the executor through the "
        "execution ports its composition root selects (D3)"
    ], "B8"
    b14 = dispatch_boundary_errors(with_edge("awaken-coordinator", "awaken-worker"))
    assert [e for e in b14 if "(D3)" in e] == [
        "dispatch-boundary: awaken-coordinator reaches awaken-session-executor, awaken-worker; the "
        "Coordinator composes dispatch and consumes the executor through the "
        "execution ports its composition root selects (D3)"
    ], "B14"
    record = {"reason": "fixture", "approved_by": "fixture-reviewer", "date": "2026-10-24",
              "remove_when": "fixture completes"}
    product = _specs(base + [("awaken-cli", "apps", "bootstrap", {"awaken-coordinator", "awaken-worker"})])
    assert dispatch_boundary_errors(product) == [], "B9 the binary's package is outside D1-D3"
    reached = coordinator_binary_reach(product, "awaken-cli")
    assert reached == ["awaken-session-executor", "awaken-worker"], f"B9 reach {reached}"
    assert coordinator_binary_errors("awaken-cli", reached, {name: record for name in reached}) == [], "B9"
    b10 = coordinator_binary_errors("awaken-cli", reached, {"awaken-worker": record})
    assert b10 == [
        "dispatch-boundary: the awaken-coordinator binary's package awaken-cli reaches "
        "awaken-session-executor with no reviewed transition in coordinator_binary_closure_baseline.json; "
        "compose the Coordinator without it (D4)"
    ], "B10"
    b11 = coordinator_binary_errors("awaken-cli", ["awaken-worker"],
                                    {"awaken-worker": record, "awaken-session-executor": record})
    assert len(b11) == 1 and "no longer reaches" in b11[0] and "--tighten" in b11[0], "B11"
    b12 = coordinator_binary_errors("awaken-cli", ["awaken-worker"],
                                    {"awaken-worker": {**record, "approved_by": ""}})
    assert len(b12) == 1 and "complete review record" in b12[0], "B12"
    assert binary_package({"awaken-cli": ["awaken", "awaken-coordinator"], "awaken-worker": ["awaken-worker"]},
                          "awaken-coordinator") == "awaken-cli", "B13 single owner"
    for binaries in ({"awaken-cli": ["awaken"]},
                     {"awaken-cli": ["awaken-coordinator"], "other": ["awaken-coordinator"]}):
        package = binary_package(binaries, "awaken-coordinator")
        assert package is None, "B13"
        assert "no single package" in coordinator_binary_errors(package, [], {})[0], "B13"
    held = {"$comment": CLOSURE_COMMENT, "transitions": {"awaken-worker": record, "awaken-session-executor": record}}
    assert _tighten_closure(("awaken-cli", ["awaken-worker"]), held)["transitions"] == {"awaken-worker": record}, \
        "B11 tighten drops only the unreached transition"
    written, grown = _write_closure(("awaken-cli", ["awaken-sandbox-local", "awaken-worker"]),
                                    {"transitions": {"awaken-worker": record}}, json.dumps({**record, "reason": "new"}))
    assert grown == ["awaken-sandbox-local"] and written["transitions"]["awaken-worker"] == record \
        and written["transitions"]["awaken-sandbox-local"]["reason"] == "new", "B10 growth keeps held records"


def check_all() -> list[str]:
    specs = {spec.name: spec for spec in _crate_boundary_workspace.dependency_fitness_specs()}
    return dispatch_boundary_errors(specs) + check_coordinator_binary()


def main(argv: list[str]) -> int:
    """``--self-test`` runs the decision table; the D4 transitions use the shared baseline CLI."""
    if argv == ["--self-test"]:
        selftest()
        print("_dispatch_boundary_fitness selftest: the decision table holds")
        return 0
    return _architecture_ratchet.baseline_cli(
        argv, prog=Path(__file__).name, description=__doc__, baseline_path=CLOSURE_BASELINE,
        self_test=selftest, measure=_measure_coordinator_binary, check=_check_closure,
        tighten=_tighten_closure, write=_write_closure)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
