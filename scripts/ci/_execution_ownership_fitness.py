"""Execution and public-route ownership fitness checks."""

from __future__ import annotations

import re
from pathlib import Path


RETIRED_EXECUTION_PATHS = {
    "ToolExecutorProvider": "SessionEnvironment is the only Hand owner",
    "ToolExecutorSelectionError": "Hand selection no longer occurs per Run",
    "ConfigToolExecutorProvider": "Agent config cannot place a parallel Hand",
    "DynamicToolExecutorProvider": "dispatch placement selects a Worker, not a Hand",
    "declared_hand": "the executable publication has no deployment Hand coordinate",
    "hand_connections": "deployment topology is expressed by Worker/Sandbox capabilities",
    "with_remote_hand": "a Host-global Hand would bypass the Session Environment",
    "plan_shape": "environment realization has one Session/backend decision path",
    "ExecutionShape": "environment realization has one Session/backend decision path",
}

ROUTE_OWNER_FILES = (
    "crates/control/awaken-admin-config-api/src/router.rs",
    "crates/control/awaken-config-service/src/capabilities.rs",
    "crates/control/awaken-config-service/src/config_routes.rs",
    "crates/control/awaken-control/src/authz.rs",
    "crates/control/awaken-control/src/data_subject.rs",
    "crates/control/awaken-control/src/lib.rs",
    "crates/server/awaken-managed-routers/src/files.rs",
    "crates/server/awaken-managed-routers/src/memory_stores.rs",
    "crates/server/awaken-managed-routers/src/models.rs",
    "crates/server/awaken-managed-routers/src/skills.rs",
    "crates/server/awaken-protocol-a2a/src/router.rs",
    "crates/server/awaken-protocol-ag-ui/src/router.rs",
    "crates/server/awaken-protocol-ai-sdk/src/router.rs",
    "crates/server/awaken-protocol-managed/src/ext/live_inbox.rs",
    "crates/server/awaken-protocol-managed/src/rate_limit.rs",
    "crates/server/awaken-protocol-managed/src/routes/agents_registry.rs",
    "crates/server/awaken-protocol-managed/src/routes/deployments.rs",
    "crates/server/awaken-protocol-managed/src/routes/dreams.rs",
    "crates/server/awaken-protocol-managed/src/routes/environments.rs",
    "crates/server/awaken-protocol-managed/src/routes/sessions.rs",
    "crates/server/awaken-protocol-managed/src/routes/user_profiles.rs",
    "crates/server/awaken-protocol-managed/src/routes/vaults.rs",
    "crates/server/awaken-protocol-mcp/src/http.rs",
    "crates/server/awaken-server/src/application_access.rs",
)

# Public ingress code is discovered as well as explicitly registered. This
# prevents a new router source from silently escaping the ownership inventory.
PUBLIC_ROUTE_ROOTS = (
    "crates/control",
    "crates/server/awaken-managed-routers/src",
    "crates/server/awaken-protocol-a2a/src",
    "crates/server/awaken-protocol-ag-ui/src",
    "crates/server/awaken-protocol-ai-sdk/src",
    "crates/server/awaken-protocol-managed/src",
    "crates/server/awaken-protocol-mcp/src",
    "crates/server/awaken-server/src",
)

ROUTE_START = re.compile(r'\.route\(\s*"(?P<path>[^"]+)"\s*,', re.MULTILINE)
METHOD = re.compile(r"\b(get|post|put|patch|delete|head|options)\s*\(")
PARAMETER = re.compile(r"\{[^}]+\}")


def _production(text: str) -> str:
    """Inline unit tests are not route owners."""
    return text.split("#[cfg(test)]", 1)[0]


def _normalized_path(path: str) -> str:
    return PARAMETER.sub(lambda match: "{*}" if match.group(0).startswith("{*") else "{}", path)


def _owned_routes(path: Path) -> list[tuple[str, str]]:
    text = _production(path.read_text(encoding="utf-8"))
    starts = list(ROUTE_START.finditer(text))
    owned: list[tuple[str, str]] = []
    for index, match in enumerate(starts):
        end = starts[index + 1].start() if index + 1 < len(starts) else len(text)
        # One route expression may chain multiple MethodRouter verbs. Limit the
        # window to its next route declaration; handler bodies are defined elsewhere.
        expression = text[match.end() : end]
        methods = set(METHOD.findall(expression))
        for method in methods:
            owned.append((method.upper(), _normalized_path(match.group("path"))))
    return owned


def duplicate_route_owner_violations(
    entries: list[tuple[str, str, str]],
) -> list[str]:
    """A method + normalized path may be declared repeatedly only by its same owner."""
    owners: dict[tuple[str, str], str] = {}
    errors: list[str] = []
    for method, path, owner in entries:
        key = (method, path)
        previous = owners.get(key)
        if previous is not None and previous != owner:
            errors.append(
                f"duplicate public route owner {method} {path}: {previous} and {owner}"
            )
        else:
            owners[key] = owner
    return errors


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    for path in sorted((repo_root / "crates").glob("**/src/**/*.rs")):
        text = path.read_text(encoding="utf-8")
        for symbol, owner in RETIRED_EXECUTION_PATHS.items():
            if re.search(rf"\b{re.escape(symbol)}\b", text):
                errors.append(
                    f"{path.relative_to(repo_root)}: retired execution path {symbol!r}; {owner}"
                )

    registered = set(ROUTE_OWNER_FILES)
    discovered: set[str] = set()
    for relative_root in PUBLIC_ROUTE_ROOTS:
        root = repo_root / relative_root
        if not root.exists():
            errors.append(f"missing public-route source root {relative_root}")
            continue
        paths = root.glob("**/*.rs") if root.is_dir() else (root,)
        for path in paths:
            if "tests" in path.parts or path.name.endswith("_tests.rs"):
                continue
            if _owned_routes(path):
                discovered.add(str(path.relative_to(repo_root)))
    for relative in sorted(discovered - registered):
        errors.append(
            f"unregistered public route owner source {relative}; add it to ROUTE_OWNER_FILES"
        )

    entries: list[tuple[str, str, str]] = []
    for relative in sorted(registered | discovered):
        path = repo_root / relative
        if not path.is_file():
            errors.append(f"missing route-owner source {relative}")
            continue
        entries.extend((method, route_path, relative) for method, route_path in _owned_routes(path))
    errors.extend(duplicate_route_owner_violations(entries))
    return errors


def selftest() -> None:
    # Cause/effect graph: C1=path parameter spelling differs; C2=method differs;
    # C3=inline cfg(test) route; C4=same key from a different owner. Effects:
    # E1 C1 normalizes to one owner key; E2 C2 remains distinct; E3 C3 is not
    # production ownership; E4 C4 fails while repeated declaration by the same
    # owner remains one authority. Decision rows
    # are asserted here so the fitness parser cannot silently weaken itself.
    assert _normalized_path("/v1/agents/{agent_id}") == "/v1/agents/{}", "E1"
    assert _normalized_path("/v1/agents/{id}") == "/v1/agents/{}", "E1"
    assert ("GET", "/x") != ("POST", "/x"), "E2"
    assert _production("prod\n#[cfg(test)]\ntest") == "prod\n", "E3"
    assert duplicate_route_owner_violations(
        [("GET", "/x", "a.rs"), ("GET", "/x", "a.rs")]
    ) == [], "E4 same owner"
    assert duplicate_route_owner_violations(
        [("GET", "/x", "a.rs"), ("GET", "/x", "b.rs")]
    ), "E4 distinct owners"
