"""Every crate states why it is a separate crate, and the reason is checked.

The ADR-0088 amendment of 2026-10-07. A crate exists only for a reason a
module could not give: `metadata.awaken.separation` names one reason from
`REASONS`, and each reason has a mechanical check against the crate's metadata,
manifest and dependents, so a new crate cannot be split off without one. Code
that needs no reason of its own belongs in an existing crate as a module.

Crates that had no checkable reason when the rule was adopted are listed in
`UNJUSTIFIED`. The list only shrinks: a listed crate that gains a reason, is
merged or is deleted leaves the list in the same commit. Being listed is not an
order to merge; a merge is made when it pays for itself, not to empty the list.
"""

from __future__ import annotations

from collections.abc import Iterable
from pathlib import Path
from typing import NamedTuple

import _crate_boundary_workspace

REASONS: dict[str, str] = {
    "publish": "an SDK crate users select and version on its own (product sdk, publishable)",
    "process": "owns a process: a binary target, or the composition root a binary assembles (layer bootstrap)",
    "contract": "a contract-layer crate that another bounded context depends on instead of the implementation",
    "isolation": "keeps a dependency named in `isolates` (one of ISOLATED_DEPENDENCIES) out of "
    "a sibling crate of its bounded context",
    "protocol": "adapts the external wire protocol named in `protocol`, the only crate of its "
    "bounded context that does",
    "sole-in-layer": "the only crate of its bounded context at its layer, so the compiler keeps the "
    "layer direction",
    "test-support": "test-only support (layer tooling) kept out of production builds",
    "coordinator-side": "the Coordinator half of a Worker seam: a sibling of its bounded context is a "
    "Worker-side implementation (WORKER_SIDE), and its own normal closure links none, so the "
    "Coordinator process composes it without that sibling",
}

# The Worker-side implementation crates a `coordinator-side` crate keeps out of
# the Coordinator (the Session executor and the crates rule D4 of
# _dispatch_boundary_fitness.py names; Sandbox adapters are checked by prefix).
WORKER_SIDE: frozenset[str] = frozenset(
    {
        "awaken-session-executor",
        "awaken-session-realization",
        "awaken-credential-materializer",
        "awaken-run-executor-acp",
    }
)


def _is_worker_side(crate: "CrateSeparation") -> bool:
    return crate.name in WORKER_SIDE or (
        crate.name.startswith("awaken-sandbox") and crate.layer != "contract"
    )


def _normal_closure(name: str, crates: dict[str, "CrateSeparation"]) -> set[str]:
    seen: set[str] = set()
    pending = [name]
    while pending:
        for dependency in crates[pending.pop()].deps:
            if dependency in crates and dependency not in seen:
                seen.add(dependency)
                pending.append(dependency)
    return seen

# Dependencies heavy, optional or OS-specific enough that keeping them out of
# the rest of a bounded context justifies a crate of their own.
ISOLATED_DEPENDENCIES: dict[str, str] = {
    # Execution last-mile adapter: links provider SDK, crypto and optional HTTP,
    # while the realization library remains independent of the Runtime engine/Host.
    "awaken-credential-materializer": "concrete credential/provider materialization",
    # The provider SDK (genai and its pinned HTTP client): only the crates that
    # turn a materialized endpoint into an executor link it.
    "awaken-provider-genai": "provider SDK",
    "chacha20poly1305": "AEAD cipher",
    "sqlx": "database driver",
    # SQLx 0.9's facade does not export the transaction/depth manager trait.
    "sqlx-core": "database driver transaction/depth management",
    "axum": "HTTP server",
    "reqwest": "HTTP client",
    "ureq": "HTTP client",
    "kube": "Kubernetes client",
    "k8s-openapi": "Kubernetes API types",
    "bollard": "Docker client",
    "object_store": "object storage",
    "async-nats": "NATS client",
    "agent-client-protocol": "ACP wire",
    "yamux": "stream multiplexer",
    "tar": "archive format",
    "zip": "archive format",
    "opentelemetry": "telemetry",
    "opentelemetry_sdk": "telemetry",
    "opentelemetry-otlp": "telemetry exporter",
    "cap-std": "capability file system",
    "rustix": "POSIX system calls",
    "nix": "POSIX system calls",
    "libc": "C library FFI",
    "seccompiler": "seccomp filters",
    "fuser": "FUSE",
    "windows-sys": "Win32 API",
}

# External wire protocols an adapter may name in `protocol`.
EXTERNAL_PROTOCOLS: dict[str, str] = {
    "a2a": "Agent2Agent",
    "acp": "Agent Client Protocol",
    "ag-ui": "AG-UI",
    "ai-sdk": "Vercel AI SDK UI message stream",
    "mcp": "Model Context Protocol",
    "managed-agents": "Anthropic Managed Agents API",
    "awaken-http": "Awaken HTTP extensions under /v1/awaken",
}

# Crates without a checkable reason when the rule was adopted. Only shrinks.
UNJUSTIFIED: frozenset[str] = frozenset(
    {
        "awaken-acp-application",
        "awaken-admin-assistant",
        "awaken-agent-config",
        "awaken-config-resolver",
        "awaken-dispatch-erasure",
        "awaken-dispatch-execution",
        "awaken-dream-application",
        "awaken-durable-projection",
        "awaken-environment-package-image-builder",
        "awaken-model-catalog",
        "awaken-resource-reclaimer",
        "awaken-sandbox-control-transport",
        "awaken-session-application",
        "awaken-session-environment",
        "awaken-store-fs",
        "awaken-store-schema",
        "awaken-tool-relay",
        "awaken-webhook",
    }
)


class CrateSeparation(NamedTuple):
    name: str
    product: str
    bounded_context: str
    layer: str
    deps: frozenset[str]
    separation: str = ""
    isolates: tuple[str, ...] = ()
    protocol: str = ""
    publishable: bool = True
    has_binary: bool = False


def _home(crate: CrateSeparation) -> tuple[str, str]:
    return crate.product, crate.bounded_context


def _reason_errors(crate: CrateSeparation, crates: dict[str, CrateSeparation]) -> list[str]:
    name, reason = crate.name, crate.separation
    siblings = [other for other in crates.values() if other.name != name and _home(other) == _home(crate)]
    home = crate.bounded_context or crate.product
    if reason == "publish":
        if crate.product != "sdk" or not crate.publishable:
            return [f"{name} states separation = \"publish\" but is not a publishable SDK crate"]
    elif reason == "process":
        if not crate.has_binary and crate.layer != "bootstrap":
            return [f"{name} states separation = \"process\" but has no binary target and is not a composition root"]
    elif reason == "contract":
        if crate.layer != "contract":
            return [f"{name} states separation = \"contract\" but its layer is {crate.layer!r}"]
        if not any(name in other.deps and _home(other) != _home(crate) for other in crates.values()):
            return [f"{name} states separation = \"contract\" but no crate outside {home} depends on it"]
    elif reason == "isolation":
        if not crate.isolates:
            return [f"{name} states separation = \"isolation\" but lists nothing in `isolates`"]
        errors = []
        for dependency in crate.isolates:
            if dependency not in ISOLATED_DEPENDENCIES:
                errors.append(f"{name} isolates {dependency!r}, which is not in ISOLATED_DEPENDENCIES")
            elif dependency not in crate.deps:
                errors.append(f"{name} isolates {dependency!r} but does not depend on it")
            elif not any(dependency not in other.deps for other in siblings):
                errors.append(f"{name} isolates {dependency!r} but no other {home} crate is kept free of it")
        return errors
    elif reason == "protocol":
        if crate.protocol not in EXTERNAL_PROTOCOLS:
            return [f"{name} states separation = \"protocol\" with protocol {crate.protocol!r}, "
                    f"not one of {sorted(EXTERNAL_PROTOCOLS)}"]
        twins = sorted(other.name for other in siblings
                       if other.separation == "protocol" and other.protocol == crate.protocol)
        if twins:
            return [f"{name} adapts protocol {crate.protocol!r}, as {home} crate(s) {', '.join(twins)} already do; "
                    "one crate per protocol per bounded context"]
    elif reason == "sole-in-layer":
        same_layer = sorted(other.name for other in siblings if other.layer == crate.layer)
        if same_layer:
            return [f"{name} states separation = \"sole-in-layer\" but {', '.join(same_layer)} share "
                    f"{home} / {crate.layer}; state another reason or fold one into the other"]
    elif reason == "coordinator-side":
        if not any(_is_worker_side(other) for other in siblings):
            return [f"{name} states separation = \"coordinator-side\" but no {home} sibling is Worker-side"]
        linked = sorted(dep for dep in _normal_closure(name, crates) if _is_worker_side(crates[dep]))
        if linked:
            return [f"{name} states separation = \"coordinator-side\" but links Worker-side {', '.join(linked)}"]
    elif reason == "test-support":
        if crate.layer != "tooling":
            return [f"{name} states separation = \"test-support\" but its layer is {crate.layer!r}"]
    return []


def separation_errors(
    crates: Iterable[CrateSeparation], unjustified: frozenset[str] = UNJUSTIFIED
) -> list[str]:
    by_name = {crate.name: crate for crate in crates}
    errors: list[str] = []
    for crate in sorted(by_name.values()):
        name = crate.name
        if crate.isolates and crate.separation != "isolation":
            errors.append(f"{name} lists `isolates` but states separation = {crate.separation!r}")
        if crate.protocol and crate.separation != "protocol":
            errors.append(f"{name} names a `protocol` but states separation = {crate.separation!r}")
        if name in unjustified:
            if crate.separation:
                errors.append(f"{name} now states a separation; remove it from UNJUSTIFIED in this commit")
            else:
                continue
        if not crate.separation:
            errors.append(
                f"{name} must state why it is a separate crate: set metadata.awaken.separation to one of "
                f"{sorted(REASONS)}, or fold it into an existing crate as a module"
            )
        elif crate.separation not in REASONS:
            errors.append(f"{name} states unknown separation {crate.separation!r}; use one of {sorted(REASONS)}")
        else:
            errors.extend(_reason_errors(crate, by_name))
    errors += [
        f"{name} is in UNJUSTIFIED but is no longer a crate; remove it in this commit"
        for name in sorted(unjustified - set(by_name))
    ]
    return [f"crate-separation: {error}" for error in errors]


def crate_separation(manifest_path: Path) -> CrateSeparation:
    manifest = _crate_boundary_workspace.load_manifest(manifest_path)
    metadata = manifest.get("package", {}).get("metadata", {}).get("awaken", {})
    root = manifest_path.parent
    return CrateSeparation(
        name=_crate_boundary_workspace.package_name(manifest),
        product=str(metadata.get("product", "")),
        bounded_context=str(metadata.get("bounded_context", "")),
        layer=str(metadata.get("layer", "")),
        deps=frozenset(_crate_boundary_workspace.normal_dependency_names(manifest)),
        separation=str(metadata.get("separation", "")),
        isolates=tuple(metadata.get("isolates", ())),
        protocol=str(metadata.get("protocol", "")),
        publishable=manifest.get("package", {}).get("publish", True) is not False,
        has_binary=bool(manifest.get("bin")) or (root / "src/main.rs").exists() or (root / "src/bin").is_dir(),
    )


def check_all(crates: Path) -> list[str]:
    return separation_errors(crate_separation(path) for path in _crate_boundary_workspace.crate_manifests(crates))


def selftest() -> None:
    """Decision table.

    | Rule | cause                                                          | effect |
    | U1   | no separation, not in UNJUSTIFIED                              | error |
    | U2   | in UNJUSTIFIED without / with a separation                     | accept / error |
    | U3   | UNJUSTIFIED names a crate that is gone                         | stale error |
    | U4   | unknown reason; `isolates` or `protocol` beside another reason | error |
    | P1   | publish: publishable SDK / Platform / SDK with publish = false | accept / error / error |
    | B1   | process: binary target / bootstrap layer / neither             | accept / accept / error |
    | C1   | contract: dependent in another context / same context only / not contract layer | accept / error / error |
    | I1   | isolation: listed dependency held, a sibling kept free of it   | accept |
    | I2   | isolation: empty, unlisted dependency, not a dependency, every sibling has it | error |
    | R1   | protocol: known and unique / twice in a context / in two contexts / unknown | accept / error / accept / error |
    | L1   | sole-in-layer: alone / a sibling at the same layer             | accept / error |
    | T1   | test-support: tooling / other layer                            | accept / error |
    | K1   | coordinator-side: Worker-side sibling, links none / links one / no such sibling | accept / error / error |
    """

    def crate(name: str, separation: str = "", context: str = "session", layer: str = "infrastructure",
              deps: Iterable[str] = (), product: str = "platform", **extra) -> CrateSeparation:
        return CrateSeparation(name, product, context, layer, frozenset(deps), separation, **extra)

    def errors(*crates: CrateSeparation, unjustified: frozenset[str] = frozenset()) -> list[str]:
        return separation_errors(crates, unjustified)

    assert "must state why" in errors(crate("a"))[0], "U1"
    assert errors(crate("a"), unjustified=frozenset({"a"})) == [], "U2 listed"
    assert "remove it from UNJUSTIFIED" in errors(
        crate("a", "sole-in-layer"), unjustified=frozenset({"a"}))[0], "U2 declares"
    assert "no longer a crate" in errors(unjustified=frozenset({"gone"}))[0], "U3"
    assert "unknown separation" in errors(crate("a", "small"))[0], "U4 unknown"
    assert any("lists `isolates`" in e for e in errors(crate("a", "sole-in-layer", isolates=("sqlx",)))), "U4"
    assert any("names a `protocol`" in e for e in errors(crate("a", "sole-in-layer", protocol="mcp"))), "U4"

    assert errors(crate("s", "publish", "agent-runtime", product="sdk")) == [], "P1"
    assert errors(crate("s", "publish")), "P1 platform"
    assert errors(crate("s", "publish", "agent-runtime", product="sdk", publishable=False)), "P1 unpublished"

    assert errors(crate("b", "process", has_binary=True)) == [], "B1 binary"
    assert errors(crate("b", "process", layer="bootstrap")) == [], "B1 bootstrap"
    assert errors(crate("b", "process")), "B1 neither"

    port = crate("p", "contract", layer="contract")
    assert errors(port, crate("u", "sole-in-layer", "config", deps=["p"])) == [], "C1"
    assert "no crate outside" in errors(port, crate("u", "sole-in-layer", deps=["p"]))[0], "C1 same context"
    assert "its layer is" in errors(crate("p", "contract"))[0], "C1 layer"

    for driver in ("sqlx", "sqlx-core"):
        store = crate("st", "isolation", deps=[driver], isolates=(driver,))
        assert errors(store, crate("app", "sole-in-layer", layer="application")) == [], f"I1 {driver}"
        assert "does not depend" in errors(crate("st", "isolation", isolates=(driver,)))[0], f"I2 missing {driver}"
        assert "kept free" in errors(store, crate("app", "sole-in-layer", layer="application",
                                                  deps=[driver]))[0], f"I2 sibling {driver}"
    assert "lists nothing" in errors(crate("st", "isolation"))[0], "I2 empty"
    for unlisted in ("serde", "sqlx-unregistered"):
        assert "not in ISOLATED" in errors(crate("st", "isolation", deps=[unlisted], isolates=(unlisted,)))[0], "I2 unlisted"

    # I3: the concrete last-mile provider/crypto adapter is an isolation edge,
    # under the same I1/I2 dependency and sibling checks, never a name-only waiver.
    materialization = crate("effects", "isolation", deps=["awaken-credential-materializer"],
                            isolates=("awaken-credential-materializer",))
    assert errors(materialization, crate("run", "sole-in-layer", layer="application")) == [], "I3"
    assert "kept free" in errors(materialization, crate("run", "sole-in-layer", layer="application",
                                                      deps=["awaken-credential-materializer"]))[0], "I3 sibling"

    ag_ui = crate("ag", "protocol", "protocols", "interface", protocol="ag-ui")
    ai_sdk = crate("ai", "protocol", "protocols", "interface", protocol="ai-sdk")
    assert errors(ag_ui, ai_sdk) == [], "R1"
    twin = errors(ag_ui, crate("ag2", "protocol", "protocols", "interface", protocol="ag-ui"))
    assert len(twin) == 2 and "one crate per protocol" in twin[0], "R1 twice"
    assert errors(crate("serve", "protocol", "protocols", protocol="acp"),
                  crate("client", "protocol", "execution", protocol="acp")) == [], "R1 two contexts"
    assert "not one of" in errors(crate("x", "protocol", protocol="ag-ui-2"))[0], "R1 unknown"

    assert errors(crate("d", "sole-in-layer", layer="domain")) == [], "L1"
    pair = errors(crate("d", "sole-in-layer", layer="domain"), crate("e", "sole-in-layer", layer="domain"))
    assert len(pair) == 2 and "share session / domain" in pair[0], "L1 sibling"

    assert errors(crate("t", "test-support", layer="tooling")) == [], "T1"
    assert errors(crate("t", "test-support")), "T1 layer"

    executor = crate("awaken-session-executor", "isolation", "execution", deps=["sqlx"], isolates=("sqlx",))
    store = crate("store", "isolation", "execution")
    assert errors(executor, crate("delivery", "coordinator-side", "execution")) == [], "K1"
    assert "links Worker-side awaken-session-executor" in errors(
        executor, crate("delivery", "coordinator-side", "execution", deps=["mid"]),
        crate("mid", "sole-in-layer", "execution", layer="application", deps=["awaken-session-executor"]))[0], "K1 links"
    assert "no execution sibling" in errors(store, crate("delivery", "coordinator-side", "execution"))[0], "K1 sibling"
    print("_crate_separation_fitness selftest: the decision table holds")
