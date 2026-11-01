"""Focused semantic architecture rules not expressible by dependency metadata."""

from __future__ import annotations

from _crate_dependency_fitness import CrateSpec


CONTRACT_FORBIDDEN_DEPS = frozenset(
    {
        "tokio-stream", "rusqlite", "sqlx", "redis",
        "deadpool", "axum", "hyper", "reqwest", "tonic", "tower", "tower-http",
        "async-nats",
    }
)


def contract_purity_violations(spec: CrateSpec) -> list[str]:
    if spec.layer != "contract":
        return []
    return [
        f"{spec.name} ({spec.context}/contract) has normal dependency `{dep}`; contracts "
        "contain ports and values only, so move runtime/backend/wire behavior outward"
        for dep in sorted(spec.normal_deps & CONTRACT_FORBIDDEN_DEPS)
    ]


def runtime_host_facade_violations(spec: CrateSpec) -> list[str]:
    if spec.name != "awaken-session-executor":
        return []
    return [
        f"awaken-session-executor publicly re-exports `{owner}`; consumers must depend on the "
        "authoritative owner directly"
        for owner in sorted(spec.public_first_party_reexports)
    ]


def interface_facade_violations(spec: CrateSpec) -> list[str]:
    """Wire adapters expose their own DTO/routes, never another owner's API."""
    if spec.layer != "interface":
        return []
    return [
        f"{spec.name} publicly re-exports `{owner}`; interface adapters must keep "
        "first-party ownership explicit, so consumers depend on that owner directly"
        for owner in sorted(spec.public_first_party_reexports)
    ]


def check_all(specs: list[CrateSpec]) -> list[str]:
    errors: list[str] = []
    for spec in specs:
        errors.extend(contract_purity_violations(spec))
        errors.extend(runtime_host_facade_violations(spec))
        errors.extend(interface_facade_violations(spec))
    return errors


def selftest() -> None:
    """Cause/effect table: contract backends are rejected; non-contract and owned
    exports are accepted; runtime-host and interface first-party facades fail."""
    c = lambda n, l, d=frozenset(), r=frozenset(): CrateSpec(n, "runtime", l, n, d, public_first_party_reexports=r)
    assert contract_purity_violations(c("contract", "contract", frozenset({"sqlx"}))), "R1"
    assert contract_purity_violations(c("application", "application", frozenset({"tokio"}))) == [], "R2"
    assert runtime_host_facade_violations(CrateSpec("awaken-session-executor", "runtime", "application", "runtime", frozenset(), public_first_party_reexports=frozenset({"awaken-runtime"}))), "R5"
    assert interface_facade_violations(
        CrateSpec("wire", "coordinator", "interface", "wire", frozenset(), public_first_party_reexports=frozenset({"owner"}))
    ), "R6"
    # DTO1: Arch and dependency rules receive the same canonical DTO. A
    # parallel class fails even when its five common values happen to match.
    from _crate_dependency_fitness import CrateSpec as CanonicalCrateSpec
    assert CrateSpec is CanonicalCrateSpec, "DTO1 one canonical crate model"
