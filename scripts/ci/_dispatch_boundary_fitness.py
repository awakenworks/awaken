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
* D3 The Coordinator binary ``awaken-coordinator`` still links the Session
  executor and the Sandbox providers directly. Each direct dependency through
  which a Worker-side implementation enters its closure is listed in
  ``COORDINATOR_WORKER_SIDE_ENTRIES``; the list only shrinks. A new entry is
  refused, and a listed entry that no longer brings one in leaves the list in
  the same commit.

Why this is the narrowest correct check: the section 3.3 constraint -- no
Worker-side implementation anywhere in the Coordinator's closure -- is not yet
true, because the Coordinator composes the executor (``awaken-session-executor``)
and Sandbox providers itself until the executor split (plan v4 P4) and the
Sandbox provider work (P6) remove them. D2 makes the constraint hard for the
dispatch path this boundary owns, which is the edge slice E introduced
(Coordinator -> run-ingress -> dispatch-execution -> realization); D3 ratchets
the remaining entries toward zero instead of pretending they are gone.

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

# Direct Coordinator dependencies through which a Worker-side implementation
# still enters its closure. Only shrinks: the executor split (plan v4 P4) and
# the Sandbox provider work (P6) remove these.
COORDINATOR_WORKER_SIDE_ENTRIES: frozenset[str] = frozenset(
    {
        "awaken-credential-materializer",
        "awaken-session-executor",
        "awaken-sandbox-container",
        "awaken-sandbox-kubernetes",
        "awaken-sandbox-memoryd",
        "awaken-session-mcp",
        "awaken-store-fs",
    }
)

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


def worker_side_entries(specs: Specs, root: str) -> set[str]:
    """Direct dependencies of ``root`` that are, or reach, a Worker-side implementation."""
    return {
        dependency
        for dependency in specs[root].normal_deps
        if dependency in specs
        and (
            is_worker_side(specs[dependency])
            or any(is_worker_side(specs[reached]) for reached in closure(specs, dependency))
        )
    }


def dispatch_boundary_errors(
    specs: Specs, coordinator_entries: frozenset[str] = COORDINATOR_WORKER_SIDE_ENTRIES
) -> list[str]:
    """Violations of D1-D3.

    Cause/effect graph: C1 dispatch-execution names realization or an
    extension directly; C2 a dispatch-path crate reaches a Worker-side
    implementation; C3 the Coordinator gains an entry; C4 a listed entry no
    longer reaches one. E1 accept when none holds; E2 reject naming the crate
    and the rule.
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
        entries = worker_side_entries(specs, COORDINATOR)
        for entry in sorted(entries - coordinator_entries):
            errors.append(
                f"dispatch-boundary: {COORDINATOR} gains Worker-side entry {entry}; the "
                "Coordinator composes dispatch, not the Worker's executor or Sandbox (D3)"
            )
        for entry in sorted(coordinator_entries - entries):
            errors.append(
                f"dispatch-boundary: {entry} no longer brings a Worker-side implementation "
                f"into {COORDINATOR}; remove it from COORDINATOR_WORKER_SIDE_ENTRIES (D3)"
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
    | B1 | dispatch-execution reaches only contracts; Coordinator entries = list | accept |
    | B2 | dispatch-execution depends on awaken-session-realization     | reject D1 |
    | B3 | dispatch-execution depends on an awaken-ext-* crate          | reject D1 (and D2 only if Worker-side) |
    | B4 | run-ingress reaches a Worker-side Sandbox through a library  | reject D2 |
    | B5 | a Sandbox contract or a Control-plane Sandbox store reached  | accept (not Worker-side) |
    | B6 | the Coordinator gains a new Worker-side entry                | reject D3 |
    | B7 | a listed Coordinator entry no longer reaches one             | reject D3 (shrink the list) |
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
        ("awaken-session-executor", "shared", "infrastructure", {"awaken-session-realization"}),
        ("awaken-ext-memory", "runtime", "infrastructure", set()),
        ("awaken-coordinator", "coordinator", "bootstrap", {"awaken-dispatch", "awaken-session-executor"}),
    ]
    listed = frozenset({"awaken-session-executor"})

    def with_edge(name: str, *added: str) -> dict[str, _crate_dependency_fitness.CrateSpec]:
        rows = [(n, c, l, set(d) | (set(added) if n == name else set())) for n, c, l, d in base]
        return _specs(rows)

    assert dispatch_boundary_errors(_specs(base), listed) == [], "B1"
    b2 = dispatch_boundary_errors(with_edge("awaken-dispatch-execution", "awaken-session-realization"), listed)
    assert any("(D1)" in error for error in b2) and any("(D2)" in error for error in b2), "B2"
    b3 = dispatch_boundary_errors(with_edge("awaken-dispatch-execution", "awaken-ext-memory"), listed)
    assert [error for error in b3 if "(D1)" in error] and not [e for e in b3 if "(D2)" in e], "B3"
    b4 = dispatch_boundary_errors(with_edge("awaken-dispatch", "awaken-sandbox-local"), listed)
    assert any("awaken-dispatch reaches Worker-side implementation awaken-sandbox-local" in e for e in b4), "B4"
    assert dispatch_boundary_errors(
        with_edge("awaken-dispatch", "awaken-sandbox-control", "awaken-sandbox-policy-store"), listed
    ) == [], "B5"
    b6 = dispatch_boundary_errors(with_edge("awaken-coordinator", "awaken-sandbox-local"), listed)
    assert any("gains Worker-side entry awaken-sandbox-local" in e for e in b6), "B6"
    b7 = dispatch_boundary_errors(_specs(base), listed | {"awaken-sandbox-local"})
    assert any("awaken-sandbox-local no longer brings" in e for e in b7), "B7"


def check_all() -> list[str]:
    specs = {spec.name: spec for spec in _crate_boundary_workspace.dependency_fitness_specs()}
    return dispatch_boundary_errors(specs)


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        print("usage: python3 scripts/ci/_dispatch_boundary_fitness.py --self-test", file=sys.stderr)
        raise SystemExit(2)
    selftest()
    print("_dispatch_boundary_fitness selftest: the decision table holds")
