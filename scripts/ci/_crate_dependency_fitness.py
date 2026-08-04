"""Metadata-derived crate dependency fitness rules.

Every workspace crate declares one semantic context, layer, and authority.  This
module is the single source of truth for dependency direction; physical bucket
names and per-crate dependency lists are deliberately irrelevant.
"""

from __future__ import annotations

from typing import NamedTuple


CONTEXTS = frozenset(
    {"shared", "runtime", "control", "coordinator", "resources", "worker", "apps", "devtools"}
)
LAYERS = frozenset(
    {"contract", "domain", "application", "interface", "infrastructure", "bootstrap", "tooling"}
)

# Context direction is expressed once by role, never once per crate.  Contract
# edges are handled separately because a published contract is precisely the
# legal cross-context seam.
CONTEXT_DEPENDENCIES: dict[str, frozenset[str]] = {
    # Shared application/infrastructure crates integrate cross-context ports.
    # Their lower contract/domain layers remain protected by layer direction.
    "shared": CONTEXTS,
    "runtime": frozenset({"shared", "runtime"}),
    "control": frozenset({"shared", "runtime", "resources", "control"}),
    "coordinator": frozenset(
        {"shared", "runtime", "resources", "worker", "control", "coordinator"}
    ),
    "resources": frozenset({"shared", "runtime", "resources"}),
    "worker": frozenset({"shared", "runtime", "resources", "worker"}),
    "apps": CONTEXTS,
    "devtools": CONTEXTS,
}

LAYER_DEPENDENCIES: dict[str, frozenset[str]] = {
    "contract": frozenset({"contract"}),
    "domain": frozenset({"contract", "domain"}),
    "application": frozenset(
        {"contract", "domain", "application", "interface", "infrastructure"}
    ),
    # Interface and infrastructure are sibling outer rings.  A wire adapter may
    # use a concrete transport/storage adapter and an outbound adapter may use a
    # protocol codec, while Cargo still prevents cycles.
    "interface": frozenset({"contract", "domain", "application", "interface", "infrastructure"}),
    "infrastructure": frozenset(
        {"contract", "domain", "application", "interface", "infrastructure"}
    ),
    "bootstrap": LAYERS,
    "tooling": LAYERS,
}


class CrateSpec(NamedTuple):
    name: str
    context: str
    layer: str
    authority: str
    normal_deps: frozenset[str]


def metadata_violations(spec: CrateSpec) -> list[str]:
    errors: list[str] = []
    if spec.context not in CONTEXTS:
        errors.append(f"{spec.name} must declare metadata.awaken.context as one of {sorted(CONTEXTS)}")
    if spec.layer not in LAYERS:
        errors.append(f"{spec.name} must declare metadata.awaken.layer as one of {sorted(LAYERS)}")
    if not spec.authority.strip():
        errors.append(f"{spec.name} must declare a non-empty metadata.awaken.authority")
    return errors


def dependency_violations(specs: list[CrateSpec]) -> list[str]:
    by_name = {spec.name: spec for spec in specs}
    errors: list[str] = []
    for source in specs:
        if metadata_violations(source):
            continue
        for dep_name in sorted(source.normal_deps):
            target = by_name.get(dep_name)
            if target is None or metadata_violations(target):
                continue
            allowed_layers = LAYER_DEPENDENCIES[source.layer]
            if target.layer not in allowed_layers:
                errors.append(
                    f"{source.name} ({source.context}/{source.layer}) depends on {target.name} "
                    f"({target.context}/{target.layer}); layer `{source.layer}` may depend only on "
                    f"{sorted(allowed_layers)}"
                )
                continue
            if source.layer in {"bootstrap", "tooling"} or target.layer == "contract":
                continue
            allowed_contexts = CONTEXT_DEPENDENCIES[source.context]
            if target.context not in allowed_contexts:
                errors.append(
                    f"{source.name} ({source.context}/{source.layer}) depends on {target.name} "
                    f"({target.context}/{target.layer}); context `{source.context}` may depend only "
                    f"on {sorted(allowed_contexts)} or another context's contract"
                )
    return errors


def check_all(specs: list[CrateSpec]) -> list[str]:
    errors = [error for spec in specs for error in metadata_violations(spec)]
    return errors + dependency_violations(specs)


def selftest() -> None:
    """Cause/effect decision table.

    Causes: C1 metadata complete/valid, C2 dependency layer allowed, C3 dependency
    context allowed, C4 target is a contract, C5 source is bootstrap/tooling.
    Effects: E1 accept; E2 reject incomplete metadata; E3 reject outward layer;
    E4 reject cross-context implementation; E5 accept explicit contract seam;
    E6 accept composition/test assembly.
    """
    c = lambda n, x, l, a="owner", d=frozenset(): CrateSpec(n, x, l, a, d)
    assert metadata_violations(c("missing", "", "", "")), "R2"
    assert dependency_violations(
        [c("domain", "runtime", "domain", d=frozenset({"adapter"})), c("adapter", "runtime", "infrastructure")]
    ), "R3"
    assert dependency_violations(
        [c("worker", "worker", "application", d=frozenset({"control"})), c("control", "control", "application")]
    ), "R4"
    assert dependency_violations(
        [c("worker", "worker", "application", d=frozenset({"contract"})), c("contract", "control", "contract")]
    ) == [], "R5"
    assert dependency_violations(
        [c("app", "apps", "bootstrap", d=frozenset({"infra"})), c("infra", "control", "infrastructure")]
    ) == [], "R6"
