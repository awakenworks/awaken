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

Why this is the correct check: the section 3.3 constraint -- no Worker-side
implementation anywhere in the Coordinator's closure -- is a property of the
dependency graph, which the compiler cannot see from inside one crate. D2
holds it for the dispatch path this boundary owns, the edge slice E
introduced (Coordinator -> run-ingress -> dispatch-execution ->
realization); D3 holds it for the Coordinator itself. The executor is named
explicitly because it is the Worker's execution engine even where it reaches
the realization only through role traits.

A Worker-side implementation crate is ``awaken-session-realization``,
``awaken-credential-materializer``, or an ``awaken-sandbox*`` crate that is
neither a contract nor a Control-plane store. Optional dependencies count:
an enabled feature must not reopen the edge.
"""

from __future__ import annotations

import sys
from collections.abc import Iterable, Mapping

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

Specs = Mapping[str, _crate_dependency_fitness.CrateSpec]


def is_worker_side(spec: _crate_dependency_fitness.CrateSpec) -> bool:
    """A Worker-side implementation crate (see the module docs)."""
    if spec.name in ("awaken-session-realization", "awaken-credential-materializer"):
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


def check_all() -> list[str]:
    specs = {spec.name: spec for spec in _crate_boundary_workspace.dependency_fitness_specs()}
    return dispatch_boundary_errors(specs)


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        print("usage: python3 scripts/ci/_dispatch_boundary_fitness.py --self-test", file=sys.stderr)
        raise SystemExit(2)
    selftest()
    print("_dispatch_boundary_fitness selftest: the decision table holds")
