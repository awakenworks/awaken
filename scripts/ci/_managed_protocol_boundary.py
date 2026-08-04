"""Semantic route-inventory boundary for the Managed protocol adapter."""

from pathlib import Path

import _execution_ownership_fitness


# Exact normalized method/path inventory verified against the Anthropic Managed
# Agents beta surface. Parameters use the shared ownership check's `{}` form.
# This is the compatibility contract: adding an Awaken feature requires the
# explicitly separate protocol crate inventory below, never another core route.
ANTHROPIC_MANAGED_ROUTES = frozenset(
    {
        ("GET", "/v1/agents"), ("POST", "/v1/agents"),
        ("GET", "/v1/agents/{}"), ("POST", "/v1/agents/{}"),
        ("POST", "/v1/agents/{}/disable"), ("POST", "/v1/agents/{}/archive"),
        ("GET", "/v1/agents/{}/versions"),
        ("GET", "/v1/deployments"), ("POST", "/v1/deployments"),
        ("GET", "/v1/deployments/{}"), ("POST", "/v1/deployments/{}"),
        ("POST", "/v1/deployments/{}/archive"),
        ("POST", "/v1/deployments/{}/pause"),
        ("POST", "/v1/deployments/{}/unpause"),
        ("POST", "/v1/deployments/{}/run"),
        ("GET", "/v1/deployment_runs"), ("GET", "/v1/deployment_runs/{}"),
        ("GET", "/v1/dreams"), ("POST", "/v1/dreams"),
        ("GET", "/v1/dreams/{}"), ("POST", "/v1/dreams/{}/cancel"),
        ("POST", "/v1/dreams/{}/archive"),
        ("GET", "/v1/environments"), ("POST", "/v1/environments"),
        ("GET", "/v1/environments/{}"), ("POST", "/v1/environments/{}"),
        ("DELETE", "/v1/environments/{}"),
        ("POST", "/v1/environments/{}/archive"),
        ("GET", "/v1/environments/{}/work"),
        ("GET", "/v1/environments/{}/work/poll"),
        ("GET", "/v1/environments/{}/work/stats"),
        ("GET", "/v1/environments/{}/work/{}"),
        ("POST", "/v1/environments/{}/work/{}"),
        ("POST", "/v1/environments/{}/work/{}/ack"),
        ("POST", "/v1/environments/{}/work/{}/heartbeat"),
        ("POST", "/v1/environments/{}/work/{}/stop"),
        ("GET", "/v1/sessions"), ("POST", "/v1/sessions"),
        ("GET", "/v1/sessions/{}"), ("POST", "/v1/sessions/{}"),
        ("DELETE", "/v1/sessions/{}"), ("POST", "/v1/sessions/{}/archive"),
        ("GET", "/v1/sessions/{}/events"), ("POST", "/v1/sessions/{}/events"),
        ("GET", "/v1/sessions/{}/events/stream"),
        ("GET", "/v1/sessions/{}/threads"),
        ("GET", "/v1/sessions/{}/threads/{}"),
        ("POST", "/v1/sessions/{}/threads/{}/archive"),
        ("GET", "/v1/sessions/{}/threads/{}/events"),
        ("GET", "/v1/sessions/{}/threads/{}/stream"),
        ("GET", "/v1/sessions/{}/resources"),
        ("POST", "/v1/sessions/{}/resources"),
        ("GET", "/v1/sessions/{}/resources/{}"),
        ("POST", "/v1/sessions/{}/resources/{}"),
        ("DELETE", "/v1/sessions/{}/resources/{}"),
        ("GET", "/v1/user_profiles"), ("POST", "/v1/user_profiles"),
        ("GET", "/v1/user_profiles/{}"), ("POST", "/v1/user_profiles/{}"),
        ("POST", "/v1/user_profiles/{}/enrollment_url"),
        ("GET", "/v1/vaults"), ("POST", "/v1/vaults"),
        ("GET", "/v1/vaults/{}"), ("POST", "/v1/vaults/{}"),
        ("DELETE", "/v1/vaults/{}"), ("POST", "/v1/vaults/{}/archive"),
        ("GET", "/v1/vaults/{}/credentials"),
        ("POST", "/v1/vaults/{}/credentials"),
        ("GET", "/v1/vaults/{}/credentials/{}"),
        ("POST", "/v1/vaults/{}/credentials/{}"),
        ("DELETE", "/v1/vaults/{}/credentials/{}"),
        ("POST", "/v1/vaults/{}/credentials/{}/archive"),
        ("POST", "/v1/vaults/{}/credentials/{}/mcp_oauth_validate"),
        ("GET", "/v1/files"), ("POST", "/v1/files"),
        ("GET", "/v1/files/{}"), ("DELETE", "/v1/files/{}"),
        ("GET", "/v1/files/{}/content"),
        ("GET", "/v1/memory_stores"), ("POST", "/v1/memory_stores"),
        ("GET", "/v1/memory_stores/{}"),
        ("POST", "/v1/memory_stores/{}"),
        ("DELETE", "/v1/memory_stores/{}"),
        ("POST", "/v1/memory_stores/{}/archive"),
        ("GET", "/v1/memory_stores/{}/memories"),
        ("POST", "/v1/memory_stores/{}/memories"),
        ("GET", "/v1/memory_stores/{}/memories/{}"),
        ("POST", "/v1/memory_stores/{}/memories/{}"),
        ("DELETE", "/v1/memory_stores/{}/memories/{}"),
        ("GET", "/v1/memory_stores/{}/memory_versions"),
        ("GET", "/v1/memory_stores/{}/memory_versions/{}"),
        ("POST", "/v1/memory_stores/{}/memory_versions/{}/redact"),
        ("GET", "/v1/models"), ("GET", "/v1/models/{*}"),
        ("GET", "/v1/skills"), ("POST", "/v1/skills"),
        ("GET", "/v1/skills/{}"), ("DELETE", "/v1/skills/{}"),
        ("GET", "/v1/skills/{}/versions"),
        ("POST", "/v1/skills/{}/versions"),
        ("GET", "/v1/skills/{}/versions/{}"),
        ("DELETE", "/v1/skills/{}/versions/{}"),
        ("GET", "/v1/skills/{}/versions/{}/content"),
        ("GET", "/v1/skills/{}/versions/{}/files/{*}"),
    }
)

AWAKEN_MANAGED_EXTENSION_ROUTES = frozenset(
    {
        ("GET", "/v1/awaken/memory-stores/{}/dream-policy"),
        ("PUT", "/v1/awaken/memory-stores/{}/dream-policy"),
        ("GET", "/v1/awaken/sessions/{}/live-inbox"),
        ("POST", "/v1/awaken/sessions/{}/live-inbox"),
        ("PUT", "/v1/awaken/sessions/{}/live-inbox/order"),
        ("PUT", "/v1/awaken/sessions/{}/live-inbox/{}"),
        ("DELETE", "/v1/awaken/sessions/{}/live-inbox/{}"),
        ("POST", "/v1/awaken/sandbox-execution-policies"),
        ("POST", "/v1/awaken/sandbox-execution-policies/{}/versions"),
        ("GET", "/v1/awaken/environments/{}/sandbox-execution-policy"),
        ("POST", "/v1/awaken/environments/{}/sandbox-execution-policy"),
    }
)


def managed_route_inventory_violations(
    core: set[tuple[str, str]], extensions: set[tuple[str, str]]
) -> list[str]:
    errors: list[str] = []
    for route in sorted(core - ANTHROPIC_MANAGED_ROUTES):
        errors.append(f"non-Anthropic route in managed core: {route[0]} {route[1]}")
    for route in sorted(ANTHROPIC_MANAGED_ROUTES - core):
        errors.append(f"missing Anthropic managed route owner: {route[0]} {route[1]}")
    for route in sorted(extensions - AWAKEN_MANAGED_EXTENSION_ROUTES):
        errors.append(f"unlisted Awaken managed extension route: {route[0]} {route[1]}")
    for route in sorted(AWAKEN_MANAGED_EXTENSION_ROUTES - extensions):
        errors.append(f"missing listed Awaken managed extension route: {route[0]} {route[1]}")
    for method, path in sorted(extensions):
        if not path.startswith("/v1/awaken/"):
            errors.append(f"Awaken extension is not explicitly namespaced: {method} {path}")
    return errors


def check_managed_route_inventory(repo_root: Path) -> list[str]:
    compatible_roots = (
        repo_root / "crates/server/awaken-protocol-managed/src",
        repo_root / "crates/server/awaken-protocol-managed-resources/src",
    )
    extension_root = repo_root / "crates/server/awaken-protocol-awaken/src"
    core: set[tuple[str, str]] = set()
    for root in compatible_roots:
        for path in sorted(root.glob("**/*.rs")):
            core.update(_execution_ownership_fitness._owned_routes(path))
    extensions: set[tuple[str, str]] = set()
    for path in sorted(extension_root.glob("**/*.rs")):
        extensions.update(_execution_ownership_fitness._owned_routes(path))
    return managed_route_inventory_violations(core, extensions)


def selftest() -> None:
    # Cause/effect graph and decision table:
    # C1 exact official compatible protocol-family inventory, C2 exact separately packaged and
    # namespaced extension inventory, C3 unknown core route, C4 unknown extension.
    # R1 C1+C2 -> accept; R2 C3 -> reject compatibility contamination;
    # R3 C4 -> reject implicit extension; R4 missing declared route -> reject drift.
    assert managed_route_inventory_violations(
        set(ANTHROPIC_MANAGED_ROUTES), set(AWAKEN_MANAGED_EXTENSION_ROUTES)
    ) == [], "R1"
    assert managed_route_inventory_violations(
        set(ANTHROPIC_MANAGED_ROUTES) | {("GET", "/v1/custom")},
        set(AWAKEN_MANAGED_EXTENSION_ROUTES),
    ), "R2"
    assert managed_route_inventory_violations(
        set(ANTHROPIC_MANAGED_ROUTES),
        set(AWAKEN_MANAGED_EXTENSION_ROUTES) | {("GET", "/v1/custom")},
    ), "R3"
    assert managed_route_inventory_violations(set(), set()), "R4"
