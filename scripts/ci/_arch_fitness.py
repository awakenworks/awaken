"""Architecture fitness functions for the neutral-core / crate-layout invariants
(ADR-0059, Phases 0.1 / 0.2 / 3), split out of `check_crate_boundaries.py` to keep that
file under the 2000-line hard limit.

Each rule is a PURE predicate over already-parsed crate data plus a cause-effect
`_selftest_*` decision table run on every invocation, so a regression in the rule itself
fails fast. The predicates take data (never walk the filesystem), so this module has no
dependency on the manifest-walking helpers in `check_crate_boundaries.py` — the caller
builds the `CrateSpec` list from its manifest walk and passes it to `check_all`.
"""

from __future__ import annotations

from typing import NamedTuple


class CrateSpec(NamedTuple):
    """One crate as the fitness rules see it: its package name, its NORMAL (non-dev)
    dependency names, and the workspace bucket directory it lives in (crates/<bucket>/)."""

    name: str
    normal_deps: frozenset[str]
    bucket: str


# ── Phase 0.1: contract/ crates are port + value-object leaves ───────────────────
# A contract/ crate names only data/serialization vocabulary + async-trait for dyn-safe
# ports. It must NOT pull an async runtime, a DB driver, or an HTTP/wire framework as a
# NORMAL dependency — those are the realizing adapter's concern. Keeping the port leaf
# free of them is what lets any plane depend on the contract without inheriting a backend.
CONTRACT_FORBIDDEN_DEPS: frozenset[str] = frozenset(
    {
        "tokio", "tokio-stream", "tokio-util",  # async runtime
        "rusqlite", "sqlx", "redis", "deadpool",  # DB / pool backends
        "axum", "hyper", "reqwest", "tonic",  # HTTP / RPC wire
        "tower", "tower-http", "async-nats",  # middleware / broker wire
    }
)


def contract_purity_violations(name: str, normal_deps: frozenset[str]) -> list[str]:
    """Pure predicate: the banned normal deps a contract/ crate names, one message per
    offending dep (sorted-stable). Dev-deps are excluded by the caller."""
    return [
        f"contract/{name} has a normal dependency on `{dep}` — a contract/ crate is a "
        f"port + value-object leaf (no runtime/backend/wire); move the impl to the "
        f"adapter that realizes the port"
        for dep in sorted(normal_deps & CONTRACT_FORBIDDEN_DEPS)
    ]


def _selftest_contract_purity() -> None:
    """Causes: C2 = a banned crate appears in NORMAL deps; C3 = the banned crate is
    dev-only. Effects: E1 = one violation per banned normal dep; E2 = none. (C1 = "lives
    under contract/" is the bucket filter in check_all, exercised live: stores/ crates
    carry rusqlite yet are never flagged — that real-repo pass IS the C1=false case.)"""
    v = contract_purity_violations("x-contract", frozenset({"tokio", "serde"}))
    assert len(v) == 1 and "tokio" in v[0], v  # T1  C2=T single -> E1
    v = contract_purity_violations("x-contract", frozenset({"rusqlite", "axum", "serde_json"}))
    assert len(v) == 2 and "axum" in v[0] and "rusqlite" in v[1], v  # T2 multiple, sorted
    assert contract_purity_violations("x-contract", frozenset({"serde", "async-trait"})) == []  # T3
    assert contract_purity_violations("x-contract", frozenset()) == []  # T4 empty


# ── Phase 0.2: protocol-* adapters are leaves ────────────────────────────────────
# A `protocol-*` crate is a pure wire translator. Nothing may depend on it except a
# composition root, the service layer that assembles the managed adapter, the control
# plane (mounts managed routes + reuses the wire error envelope), a sibling protocol
# adapter over the shared transport base, or a run-executor adapter implementing a
# neutral port over the protocol. Any OTHER depender means a neutral port has leaked into
# an adapter (the root disease) — the fix is to move the port inward to a contract/ leaf.
PROTOCOL_LEAF_CONSUMERS: frozenset[str] = frozenset(
    {
        "awaken-cli", "awaken-server", "awaken-scenario-host",  # composition roots
        "awaken-runtime-host",  # the service layer assembling the managed adapter
        "awaken-control",  # mounts the managed routes + reuses its wire ErrorResponse
    }
)


def is_protocol_leaf_consumer(name: str) -> bool:
    """Whether `name` may depend on a `protocol-*` adapter: an allowlisted role, a sibling
    protocol adapter, or a run-executor adapter implementing a neutral port over it."""
    return (
        name in PROTOCOL_LEAF_CONSUMERS
        or name.startswith("awaken-protocol-")
        or name.startswith("awaken-run-executor-")
    )


def protocol_leaf_violations(name: str, normal_deps: frozenset[str]) -> list[str]:
    """Pure predicate: the `protocol-*` adapters `name` depends on when `name` is NOT an
    allowed consumer, one message per offending dep (sorted-stable)."""
    if is_protocol_leaf_consumer(name):
        return []
    return [
        f"{name} depends on protocol adapter `{dep}` — a `protocol-*` crate is a leaf "
        f"(wire translator). If you need a type it holds, move that neutral port/value to "
        f"a contract/ leaf; only composition roots, the host/control service layer, and "
        f"sibling/executor adapters may mount an adapter."
        for dep in sorted(normal_deps)
        if dep.startswith("awaken-protocol-")
    ]


def _selftest_protocol_leaves() -> None:
    """Causes: C1 = depends on a protocol-* crate; C2 = allowlisted consumer; C3 = sibling
    `awaken-protocol-*`; C4 = `awaken-run-executor-*`. Effects: E1 = one per dep; E2 = none."""
    v = protocol_leaf_violations("awaken-config-store", frozenset({"awaken-protocol-managed", "serde"}))
    assert len(v) == 1 and "awaken-protocol-managed" in v[0], v  # T1 C1∧¬C2..4 -> E1
    assert protocol_leaf_violations("awaken-runtime-host", frozenset({"awaken-protocol-managed"})) == []  # T2 C2
    assert protocol_leaf_violations("awaken-server", frozenset({"awaken-protocol-a2a"})) == []  # T2
    assert protocol_leaf_violations("awaken-protocol-a2a", frozenset({"awaken-protocol-transport"})) == []  # T3 C3
    assert protocol_leaf_violations("awaken-run-executor-a2a", frozenset({"awaken-protocol-a2a"})) == []  # T4 C4
    assert protocol_leaf_violations("awaken-config-store", frozenset({"serde", "tokio"})) == []  # T5 ¬C1
    v = protocol_leaf_violations("awaken-data-subject", frozenset({"awaken-protocol-managed", "awaken-protocol-a2a"}))
    assert len(v) == 2 and "awaken-protocol-a2a" in v[0] and "awaken-protocol-managed" in v[1], v  # T6 sorted


# ── Phase 3: the runtime-host god-hub dependency ratchet ─────────────────────────
# `awaken-runtime-host` is the historical god-hub (~6 bounded contexts, 36 first-party
# deps). Its full crate-split is a multi-session, port-first effort — each extraction must
# introduce a narrow port at the `SharedHost` boundary first, because the modules own the
# host's fields, add `impl SharedHost` methods, and the host itself depends on
# `awaken-run-ingress` (so dispatch glue cannot move DOWN without a cycle). This RATCHET
# makes the split monotone: the hub's first-party dep count may only ever DROP. When a
# context is extracted, LOWER this ceiling in the same commit. NEVER raise it — a new hub
# dependency means the substrate grew a responsibility, the regression we are undoing.
GOD_HUB_CRATE = "awaken-runtime-host"
GOD_HUB_FIRST_PARTY_DEP_CEILING = 36


def god_hub_ratchet_violation(dep_count: int, ceiling: int) -> list[str]:
    """Pure predicate: the god-hub grew if its first-party dep count exceeds the ceiling."""
    if dep_count <= ceiling:
        return []
    return [
        f"{GOD_HUB_CRATE} has {dep_count} first-party deps, over the ratchet ceiling of "
        f"{ceiling} (ADR-0059/Phase 3). The god-hub must only SHRINK: extract the new "
        f"responsibility to its own crate behind a port, don't add a dep to the substrate. "
        f"(If you genuinely extracted a context and the count DROPPED, lower the ceiling.)"
    ]


def _selftest_god_hub_ratchet() -> None:
    """Cause: C1 = dep_count > ceiling. Effect: E1 = one directive; E2 = none."""
    assert god_hub_ratchet_violation(37, 36), "over ceiling -> violation"  # C1=T -> E1
    assert god_hub_ratchet_violation(36, 36) == []  # C1=F equal -> E2
    assert god_hub_ratchet_violation(30, 36) == []  # C1=F shrunk -> E2
    v = god_hub_ratchet_violation(40, 36)
    assert len(v) == 1 and "40" in v[0] and "36" in v[0], v  # E1 names both counts


def _property_check() -> None:
    """Formal (property-based) verification of the three predicates: universally-quantified
    invariants checked over many SEEDED-random inputs (ADR-0059 verification pass). Seeded
    so CI is deterministic; no `hypothesis` dep. Complements the cause-effect tables by
    asserting the general law, not just sampled rows.

    Laws:
      contract_purity   — result set == sorted(deps ∩ FORBIDDEN); order-independent;
                          every message names its dep; empty iff the intersection is empty.
      protocol_leaf     — a consumer/sibling/executor name ⟹ always empty; otherwise the
                          count equals the number of `awaken-protocol-*` deps; sorted-stable.
      god_hub_ratchet   — empty iff count <= ceiling; monotone in count; message names both.
    """
    import random

    rng = random.Random(0xA5CE)  # fixed seed → reproducible CI
    forbidden = list(CONTRACT_FORBIDDEN_DEPS)
    innocuous = ["serde", "serde_json", "thiserror", "async-trait", "awaken-agent-contract"]
    protocols = [f"awaken-protocol-{p}" for p in ("managed", "a2a", "ai-sdk", "ag-ui", "mcp", "transport")]
    plain_names = ["awaken-config-store", "awaken-data-subject", "awaken-model-catalog", "x-crate"]
    consumer_names = list(PROTOCOL_LEAF_CONSUMERS) + ["awaken-protocol-a2a", "awaken-run-executor-a2a"]

    for _ in range(500):
        # ---- contract purity ----
        picked_forbidden = set(rng.sample(forbidden, rng.randint(0, len(forbidden))))
        deps = frozenset(picked_forbidden | set(rng.sample(innocuous, rng.randint(0, len(innocuous)))))
        v = contract_purity_violations("x-contract", deps)
        assert len(v) == len(deps & CONTRACT_FORBIDDEN_DEPS), (deps, v)  # result == intersection
        assert bool(v) == bool(deps & CONTRACT_FORBIDDEN_DEPS)  # empty iff no forbidden dep
        assert all(any(d in msg for d in picked_forbidden) for msg in v)  # each names a dep
        # order-independence: a reordered frozenset yields the same messages (predicate sorts)
        assert contract_purity_violations("x-contract", frozenset(list(deps))) == v

        # ---- protocol leaves ----
        pdeps = frozenset(rng.sample(protocols, rng.randint(0, len(protocols))) + rng.sample(innocuous, rng.randint(0, 2)))
        name = rng.choice(plain_names + consumer_names)
        pv = protocol_leaf_violations(name, pdeps)
        if is_protocol_leaf_consumer(name):
            assert pv == [], (name, pv)  # consumer/sibling/executor ⟹ always allowed
        else:
            n_proto = sum(1 for d in pdeps if d.startswith("awaken-protocol-"))
            assert len(pv) == n_proto, (name, pdeps, pv)  # one per protocol dep
            assert pv == sorted(pv), "messages must be sorted-stable"

        # ---- god-hub ratchet ----
        ceiling = rng.randint(0, 60)
        count = rng.randint(0, 60)
        gv = god_hub_ratchet_violation(count, ceiling)
        assert (gv == []) == (count <= ceiling), (count, ceiling, gv)  # empty iff within ceiling
        if gv:
            assert str(count) in gv[0] and str(ceiling) in gv[0]  # names both counts


def selftest() -> None:
    """Run every rule's cause-effect decision table + the property checks (called on each
    CI invocation)."""
    _selftest_contract_purity()
    _selftest_protocol_leaves()
    _selftest_god_hub_ratchet()
    _property_check()


def check_all(specs: list[CrateSpec]) -> list[str]:
    """Run the architecture fitness rules over already-parsed crate specs."""
    errors: list[str] = []
    for spec in specs:
        if spec.bucket == "contract":
            errors.extend(contract_purity_violations(spec.name, spec.normal_deps))
        errors.extend(protocol_leaf_violations(spec.name, spec.normal_deps))
    hub = next((s for s in specs if s.name == GOD_HUB_CRATE), None)
    if hub is not None:
        first_party = frozenset(d for d in hub.normal_deps if d.startswith("awaken-"))
        errors.extend(god_hub_ratchet_violation(len(first_party), GOD_HUB_FIRST_PARTY_DEP_CEILING))
    return errors
