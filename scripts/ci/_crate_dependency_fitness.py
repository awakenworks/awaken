"""Metadata-derived crate dependency fitness rules.

Every workspace crate declares one semantic context, layer, and authority.  This
module is the single source of truth for dependency direction; physical bucket
names and per-crate dependency lists are deliberately irrelevant.
"""

from __future__ import annotations

from typing import NamedTuple


CONTEXTS = frozenset(
    {
        "shared",
        "protocol",
        "runtime",
        "control",
        "coordinator",
        "resources",
        "worker",
        "apps",
        "devtools",
    }
)
LAYERS = frozenset(
    {"contract", "domain", "application", "interface", "infrastructure", "bootstrap", "tooling"}
)

# Context direction is expressed once by role, never once per crate.  Contract
# edges are handled separately because a published contract is precisely the
# legal cross-context seam.
CONTEXT_DEPENDENCIES: dict[str, frozenset[str]] = {
    # Shared crates sit under every context, so they may reach only each other
    # and the neutral runtime; everything else arrives through a contract.
    # This used to be `CONTEXTS`, and that wildcard is how the shared runtime
    # host came to link the worker's sandbox implementations and the
    # coordinator's stores into the control plane (see KNOWN_INVERSIONS).
    "shared": frozenset({"shared", "runtime"}),
    # Protocol crates translate public wire vocabularies across domain
    # applications but own no domain state or persistence authority; the
    # persistence half of that sentence is enforced by
    # PROTOCOL_INFRASTRUCTURE_CONTEXTS below.
    "protocol": CONTEXTS,
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
    "application": frozenset({"contract", "domain", "application"}),
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


# A protocol adapter may use outer-ring infrastructure only when it is neutral
# plumbing (shared transport, observability) or a runtime extension. A store or
# catalog owned by another context is that context's persistence authority, and
# reading it from a wire adapter is how the same projection came to be computed
# twice (protocol state/ beside the session application).
PROTOCOL_INFRASTRUCTURE_CONTEXTS = frozenset({"shared", "runtime", "protocol"})

# Edges that violate the direction above and exist today. Each entry names the
# refactor that removes it. The set only shrinks: a listed edge that no longer
# exists, or no longer violates, is itself an error, so the removal is recorded
# in the same commit that achieves it. Nothing may be added here.
KNOWN_INVERSIONS: dict[tuple[str, str], str] = {
    **{
        ("awaken-session-executor", target): "runtime-host links execution-plane implementations; inject them from the bin through provisioning/environment ports"
        for target in ("awaken-run-executor-acp",)
    },
    **{
        ("awaken-session-executor", target): "runtime-host reaches another context's application/store; depend on its contract and let the bin inject the implementation"
        for target in ("awaken-dispatch",)
    },
    **{
        ("awaken-credential-materializer", target): "credential materializer reaches control/worker implementations; depend on their contracts"
        for target in (
            "awaken-admin-config-api",
            "awaken-credential-vault",
            "awaken-model-catalog",
            "awaken-run-executor-acp",
        )
    },
    **{
        ("awaken-webhook-managed", target): "managed webhook projection reaches control/coordinator implementations; depend on their contracts"
        for target in (
            "awaken-config-resolver",
            "awaken-credential-vault",
            "awaken-webhook",
        )
    },
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


def dependency_violations(
    specs: list[CrateSpec],
    known: dict[tuple[str, str], str] | None = None,
) -> list[str]:
    """Direction violations, minus the ratcheted KNOWN_INVERSIONS.

    `known` defaults to the module ratchet; the self-test passes an explicit
    table so its fixtures do not depend on today's workspace.
    """

    known = KNOWN_INVERSIONS if known is None else known
    raw = _raw_dependency_violations(specs)
    errors = [message for edge, message in raw if edge not in known]
    violating = {edge for edge, _ in raw}
    names = {spec.name for spec in specs}
    for edge, removal in sorted(known.items()):
        source, target = edge
        if source not in names:
            continue
        if edge not in violating:
            errors.append(
                f"KNOWN_INVERSIONS lists {source} -> {target}, which no longer violates the "
                f"dependency direction ({removal}); delete the entry in the commit that removed it"
            )
    return errors


def _raw_dependency_violations(specs: list[CrateSpec]) -> list[tuple[tuple[str, str], str]]:
    by_name = {spec.name: spec for spec in specs}
    errors: list[tuple[tuple[str, str], str]] = []
    for source in specs:
        if metadata_violations(source):
            continue
        for dep_name in sorted(source.normal_deps):
            target = by_name.get(dep_name)
            if target is None or metadata_violations(target):
                continue
            allowed_layers = LAYER_DEPENDENCIES[source.layer]
            edge = (source.name, target.name)
            if target.layer not in allowed_layers:
                errors.append((edge,
                    f"{source.name} ({source.context}/{source.layer}) depends on {target.name} "
                    f"({target.context}/{target.layer}); layer `{source.layer}` may depend only on "
                    f"{sorted(allowed_layers)}"
                ))
                continue
            if target.layer == "contract":
                continue
            # Process bootstrap is the only domain-owned layer allowed to mount
            # a protocol/interface crate. Domain and application layers remain
            # protocol-neutral; the protocol crate still points inward to their
            # contracts and applications.
            if (
                source.layer == "bootstrap"
                and target.context == "protocol"
                and target.layer == "interface"
            ):
                continue
            allowed_contexts = CONTEXT_DEPENDENCIES[source.context]
            if target.context not in allowed_contexts:
                errors.append((edge,
                    f"{source.name} ({source.context}/{source.layer}) depends on {target.name} "
                    f"({target.context}/{target.layer}); context `{source.context}` may depend only "
                    f"on {sorted(allowed_contexts)} or another context's contract"
                ))
                continue
            if (
                source.context == "protocol"
                and source.layer in {"interface", "infrastructure"}
                and target.layer == "infrastructure"
                and target.context not in PROTOCOL_INFRASTRUCTURE_CONTEXTS
            ):
                errors.append((edge,
                    f"{source.name} ({source.context}/{source.layer}) depends on {target.name} "
                    f"({target.context}/{target.layer}); a protocol adapter may not read another "
                    "context's persistence or implementation directly -- depend on its application "
                    "or repository contract"
                ))
    return errors


def check_all(specs: list[CrateSpec]) -> list[str]:
    errors = [error for spec in specs for error in metadata_violations(spec)]
    return errors + dependency_violations(specs)


def selftest() -> None:
    """Cause/effect decision table.

    Causes: C1 metadata complete/valid, C2 dependency layer allowed, C3 dependency
    context allowed, C4 target is a contract, C5 source is bootstrap/tooling,
    C6 source context is a process application or developer tool, C7 source is
    a protocol/interface crate projecting an application contract, C8 source is
    process bootstrap mounting a protocol/interface adapter, C9 an extension
    implementation depends on its neutral contract, C10 the contract attempts
    the reverse dependency.
    Effects: E1 accept; E2 reject incomplete metadata; E3 reject domain-to-adapter;
    E4 reject application-to-adapter; E5 reject cross-context implementation;
    E6 accept explicit contract boundary; E7 accept process/test startup; E8
    reject a domain bootstrap/tooling crate that uses its layer as a bypass; E9
    accept a protocol projection without classifying it as shared ownership; E10
    accept protocol mounting only at process bootstrap; E11 accept extension →
    contract; E12 reject contract → extension.

    FMECA: misclassifying a wire projection as `shared` hides its protocol owner
    and permits unrelated contexts to treat transport DTOs as reusable domain
    truth. R9 locks the explicit protocol/interface -> application direction;
    application code importing the adapter would reverse the boundary. R10
    therefore admits the outward adapter only at process bootstrap.

    | Rule | cross context | target contract | bootstrap/tooling | apps/devtools | Effect |
    | R5   | yes           | no              | no                | no            | E5     |
    | R6   | yes           | yes             | any               | any           | E6     |
    | R7   | yes           | no              | yes               | yes           | E7     |
    | R8   | yes           | no              | yes               | no            | E8     |
    | R9   | protocol app  | application     | no                | no            | E9     |
    | R10  | domain        | protocol iface  | bootstrap         | no            | E10    |
    | R11  | extension     | contract        | no                | no            | E11    |
    | R12  | contract      | extension       | no                | no            | E12    |
    """
    c = lambda n, x, l, a="owner", d=frozenset(): CrateSpec(n, x, l, a, d)
    # R13-R16: the shared wildcard, the protocol persistence rule, and the ratchet.
    shared_to_worker = [
        c("host", "shared", "infrastructure", d=frozenset({"sandbox"})),
        c("sandbox", "worker", "infrastructure"),
    ]
    assert dependency_violations(shared_to_worker, known={}), "R13 shared may not reach a worker implementation"
    assert dependency_violations(
        [c("host", "shared", "infrastructure", d=frozenset({"port"})), c("port", "worker", "contract")],
        known={},
    ) == [], "R13 shared reaches a worker contract"
    assert dependency_violations(
        [
            c("wire", "protocol", "interface", d=frozenset({"store"})),
            c("store", "coordinator", "infrastructure"),
        ],
        known={},
    ), "R14 protocol may not read another context's store"
    assert dependency_violations(
        [
            c("wire", "protocol", "interface", d=frozenset({"http"})),
            c("http", "shared", "infrastructure"),
        ],
        known={},
    ) == [], "R14 protocol may use shared plumbing"
    ratchet = {("host", "sandbox"): "fixture"}
    assert dependency_violations(shared_to_worker, known=ratchet) == [], "R15 a ratcheted edge is admitted"
    stale = dependency_violations(
        [c("host", "shared", "infrastructure"), c("sandbox", "worker", "infrastructure")],
        known=ratchet,
    )
    assert stale and "no longer violates" in stale[0], "R16 a removed edge must leave the ratchet"
    assert metadata_violations(c("missing", "", "", "")), "R2"
    assert dependency_violations(
        [c("domain", "runtime", "domain", d=frozenset({"adapter"})), c("adapter", "runtime", "infrastructure")]
    ), "R3"
    assert dependency_violations(
        [c("service", "control", "application", d=frozenset({"store"})), c("store", "control", "infrastructure")]
    ), "R4"
    assert dependency_violations(
        [c("worker", "worker", "application", d=frozenset({"control"})), c("control", "control", "application")]
    ), "R5"
    assert dependency_violations(
        [c("worker", "worker", "application", d=frozenset({"contract"})), c("contract", "control", "contract")]
    ) == [], "R6"
    assert dependency_violations(
        [c("app", "apps", "bootstrap", d=frozenset({"infra"})), c("infra", "control", "infrastructure")]
    ) == [], "R7"
    assert dependency_violations(
        [c("worker", "worker", "bootstrap", d=frozenset({"control"})), c("control", "control", "infrastructure")]
    ), "R8 bootstrap"
    assert dependency_violations(
        [c("worker-tool", "worker", "tooling", d=frozenset({"control"})), c("control", "control", "application")]
    ), "R8 tooling"
    assert dependency_violations(
        [c("tool", "devtools", "tooling", d=frozenset({"infra"})), c("infra", "control", "infrastructure")]
    ) == [], "R7 devtool"
    assert dependency_violations(
        [
            c("managed-http", "protocol", "interface", d=frozenset({"sessions"})),
            c("sessions", "coordinator", "application"),
        ]
    ) == [], "R9 protocol projects an application"
    assert dependency_violations(
        [
            c("coordinator-startup", "coordinator", "bootstrap", d=frozenset({"managed-http"})),
            c("managed-http", "protocol", "interface"),
        ]
    ) == [], "R10 process bootstrap mounts protocol"
    assert dependency_violations(
        [
            c("sessions", "coordinator", "application", d=frozenset({"managed-http"})),
            c("managed-http", "protocol", "interface"),
        ]
    ), "R10 application remains protocol-neutral"
    assert dependency_violations(
        [
            c("ext", "runtime", "infrastructure", "ext-example", frozenset({"contract"})),
            c("contract", "runtime", "contract", "runtime"),
        ]
    ) == [], "R11 extension may depend on contract"
    assert dependency_violations(
        [
            c("contract", "runtime", "contract", "runtime", frozenset({"ext"})),
            c("ext", "runtime", "infrastructure", "ext-example"),
        ]
    ), "R12 contract must not depend on extension"
