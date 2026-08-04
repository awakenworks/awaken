"""Focused semantic architecture rules not expressible by dependency metadata."""

from __future__ import annotations

from typing import NamedTuple


class CrateSpec(NamedTuple):
    name: str
    normal_deps: frozenset[str]
    context: str
    layer: str
    authority: str
    public_first_party_reexports: frozenset[str]


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


def interface_leaf_violations(spec: CrateSpec, specs: list[CrateSpec]) -> list[str]:
    """An interface adapter is consumed only by another outer adapter or assembly."""
    if spec.layer not in {"domain", "application", "contract"}:
        return []
    by_name = {item.name: item for item in specs}
    return [
        f"{spec.name} ({spec.layer}) depends on interface adapter `{dep}`; move the needed "
        "port/value to its authoritative contract or consume the adapter from an outer layer"
        for dep in sorted(spec.normal_deps)
        if dep in by_name and by_name[dep].layer == "interface"
    ]


def runtime_host_facade_violations(spec: CrateSpec) -> list[str]:
    if spec.name != "awaken-runtime-host":
        return []
    return [
        f"awaken-runtime-host publicly re-exports `{owner}`; consumers must depend on the "
        "authoritative owner directly"
        for owner in sorted(spec.public_first_party_reexports)
    ]


def check_all(specs: list[CrateSpec]) -> list[str]:
    errors: list[str] = []
    for spec in specs:
        errors.extend(contract_purity_violations(spec))
        errors.extend(interface_leaf_violations(spec, specs))
        errors.extend(runtime_host_facade_violations(spec))
    return errors


def selftest() -> None:
    """Cause/effect table: contract backend and inward interface edges are rejected;
    outer consumers and owned exports are accepted; first-party facade exports fail."""
    c = lambda n, l, d=frozenset(), r=frozenset(): CrateSpec(n, d, "runtime", l, n, r)
    assert contract_purity_violations(c("contract", "contract", frozenset({"sqlx"}))), "R1"
    assert contract_purity_violations(c("application", "application", frozenset({"tokio"}))) == [], "R2"
    interface = c("wire", "interface")
    assert interface_leaf_violations(c("domain", "domain", frozenset({"wire"})), [interface, c("domain", "domain")]), "R3"
    assert interface_leaf_violations(c("infra", "infrastructure", frozenset({"wire"})), [interface, c("infra", "infrastructure")]) == [], "R4"
    assert runtime_host_facade_violations(CrateSpec("awaken-runtime-host", frozenset(), "runtime", "application", "runtime", frozenset({"awaken-runtime"}))), "R5"
