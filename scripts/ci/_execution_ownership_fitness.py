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
    "SessionDefaultsPreparer": "Session admission is owned by awaken-session-application",
    "SessionDefaultsPreparationError": "Session admission is owned by awaken-session-application",
}

RUNTIME_HOST_ERROR_OWNER = "crates/server/awaken-runtime-host/src/lib.rs"
SESSION_RUN_ADMISSION_OWNER = (
    "crates/server/awaken-session-application/src/run_admission.rs"
)
FORBIDDEN_HOST_ERROR_INFERENCE = (
    '.contains("401")',
    '.contains("auth")',
    '.contains("mcp server")',
)

# Deployment/test compositions are architecture-bearing too. These process-wide
# selectors were removed by ADR-0073 because they create a Host-global Hand beside
# the Worker-owned SessionEnvironment. Historical ADR text may still name them;
# executable fixtures may not.
RETIRED_DEPLOYMENT_SELECTORS = {
    "AWAKEN_REMOTE_HAND": "the Worker-owned SessionEnvironment is the only Hand owner",
    "AWAKEN_REMOTE_HAND_UNIX": "the Worker-owned SessionEnvironment is the only Hand owner",
    "AWAKEN_REMOTE_HAND_LISTEN": "the Worker-owned SessionEnvironment is the only Hand owner",
    "AWAKEN_REMOTE_HAND_NATS": "the Worker-owned SessionEnvironment is the only Hand owner",
}
EXECUTABLE_FIXTURE_ROOTS = ("deploy", "e2e", "scripts")

ROUTE_OWNER_FILES = (
    "crates/control/awaken-admin-config-api/src/router.rs",
    "crates/control/awaken-config-service/src/capabilities.rs",
    "crates/control/awaken-config-service/src/config_routes.rs",
    "crates/control/awaken-control/src/authz.rs",
    "crates/control/awaken-control/src/admin_assistant.rs",
    "crates/control/awaken-control/src/data_subject.rs",
    "crates/control/awaken-control/src/lib.rs",
    "crates/server/awaken-protocol-managed/src/resources/files.rs",
    "crates/server/awaken-protocol-managed/src/resources/memory_stores.rs",
    "crates/server/awaken-protocol-managed/src/control/models.rs",
    "crates/server/awaken-protocol-managed/src/resources/skills.rs",
    "crates/server/awaken-protocol-a2a/src/router.rs",
    "crates/server/awaken-protocol-ag-ui/src/router.rs",
    "crates/server/awaken-protocol-ai-sdk/src/router.rs",
    "crates/server/awaken-protocol-awaken/src/dream_policies.rs",
    "crates/server/awaken-protocol-awaken/src/live_inbox.rs",
    "crates/server/awaken-protocol-awaken/src/resource_manifests.rs",
    "crates/server/awaken-protocol-awaken/src/sandbox_policies.rs",
    "crates/server/awaken-protocol-managed/src/rate_limit.rs",
    "crates/server/awaken-protocol-managed/src/routes/agents_registry.rs",
    "crates/server/awaken-protocol-managed/src/routes/deployments.rs",
    "crates/server/awaken-protocol-managed/src/routes/dreams.rs",
    "crates/server/awaken-protocol-managed/src/routes/environments.rs",
    "crates/server/awaken-protocol-managed/src/routes/sessions.rs",
    "crates/server/awaken-protocol-managed/src/routes/tunnels.rs",
    "crates/server/awaken-protocol-managed/src/routes/user_profiles.rs",
    "crates/server/awaken-protocol-managed/src/routes/vaults.rs",
    "crates/server/awaken-protocol-mcp/src/http.rs",
    "crates/server/awaken-coordinator/src/application_access.rs",
)

# Public ingress code is discovered as well as explicitly registered. This
# prevents a new router source from silently escaping the ownership inventory.
PUBLIC_ROUTE_ROOTS = (
    "crates/control",
    "crates/server/awaken-protocol-a2a/src",
    "crates/server/awaken-protocol-ag-ui/src",
    "crates/server/awaken-protocol-ai-sdk/src",
    "crates/server/awaken-protocol-awaken/src",
    "crates/server/awaken-protocol-managed/src",
    "crates/server/awaken-protocol-mcp/src",
    "crates/server/awaken-coordinator/src",
)

ROUTE_START = re.compile(r'\.route\(\s*"(?P<path>[^"]+)"\s*,', re.MULTILINE)
METHOD = re.compile(r"\b(get|post|put|patch|delete|head|options)\s*\(")
PARAMETER = re.compile(r"\{[^}]+\}")
PARALLEL_RAW_TOOL_REGISTRY = re.compile(
    r"HashMap\s*<\s*String\s*,\s*(?:std::sync::)?Arc\s*<\s*dyn\s+RawTool\s*>\s*>"
)

RAW_TOOL_REGISTRY_OWNER = "crates/runtime/awaken-runtime-contract/src/tool.rs"
RAW_TOOL_REGISTRY_CONSUMERS = (
    "crates/runtime/awaken-runtime/src/runtime.rs",
    "crates/server/awaken-runtime-host/src/session_environment.rs",
    "crates/worker/awaken-tool-relay/src/serve.rs",
)


def _production(text: str) -> str:
    """A terminal inline unit-test module is not a route owner.

    Test-only imports/constants may appear before production routers, so cutting
    at the first `#[cfg(test)]` silently omitted real owners. Rust source in this
    repository keeps the inline `mod tests` last; cut only at that module marker.
    """
    return re.split(r"#\[cfg\(test\)\]\s*mod\s+\w+\s*\{", text, maxsplit=1)[0]


def _normalized_path(path: str) -> str:
    return PARAMETER.sub(lambda match: "{*}" if match.group(0).startswith("{*") else "{}", path)


def _call_end(text: str, opening: int) -> int:
    """Return the byte after the balanced call beginning at `opening` (`(`).

    Route handler bodies live later in the file and contain method-like words;
    limiting ownership parsing to the balanced `.route(...)` call prevents those
    handlers from being attributed to the final route declaration.
    """
    depth = 0
    quote: str | None = None
    escaped = False
    for index in range(opening, len(text)):
        char = text[index]
        if quote is not None:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in {'"', "'"}:
            quote = char
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index + 1
    return len(text)


def _owned_routes(path: Path) -> list[tuple[str, str]]:
    text = _production(path.read_text(encoding="utf-8"))
    owned: list[tuple[str, str]] = []
    for match in ROUTE_START.finditer(text):
        opening = text.find("(", match.start(), match.end())
        expression = text[match.end() : _call_end(text, opening)]
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


def duplicate_tool_registry_violations(sources: dict[str, str]) -> list[str]:
    """Every execution location reuses the contract-owned RawToolRegistry."""
    errors: list[str] = []
    owner = sources.get(RAW_TOOL_REGISTRY_OWNER, "")
    if "pub struct RawToolRegistry" not in owner:
        errors.append(f"{RAW_TOOL_REGISTRY_OWNER}: missing authoritative RawToolRegistry")
    for relative, text in sources.items():
        if PARALLEL_RAW_TOOL_REGISTRY.search(_production(text)):
            errors.append(
                f"{relative}: parallel RawTool HashMap; reuse RawToolRegistry"
            )
    for relative in RAW_TOOL_REGISTRY_CONSUMERS:
        if "RawToolRegistry" not in sources.get(relative, ""):
            errors.append(f"{relative}: execution owner bypasses RawToolRegistry")
    return errors


def retired_deployment_selector_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    for relative, text in sources.items():
        for selector, owner in RETIRED_DEPLOYMENT_SELECTORS.items():
            if re.search(rf"\b{re.escape(selector)}\b", text):
                errors.append(
                    f"{relative}: retired deployment selector {selector!r}; {owner}"
                )
    return errors


def runtime_host_boundary_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    host_error_projection = _production(sources.get(RUNTIME_HOST_ERROR_OWNER, ""))
    for token in FORBIDDEN_HOST_ERROR_INFERENCE:
        if token in host_error_projection:
            errors.append(
                f"{RUNTIME_HOST_ERROR_OWNER}: Host fault classification must use stable origin codes; forbidden {token!r}"
            )
    admission = sources.get(SESSION_RUN_ADMISSION_OWNER, "")
    if "pub struct AdmittedRunApplication" not in admission:
        errors.append(
            f"{SESSION_RUN_ADMISSION_OWNER}: missing Session-owned public Run admission decorator"
        )
    return errors


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    rust_sources: dict[str, str] = {}
    for path in sorted((repo_root / "crates").glob("**/src/**/*.rs")):
        text = path.read_text(encoding="utf-8")
        rust_sources[str(path.relative_to(repo_root))] = text
        for symbol, owner in RETIRED_EXECUTION_PATHS.items():
            if re.search(rf"\b{re.escape(symbol)}\b", text):
                errors.append(
                    f"{path.relative_to(repo_root)}: retired execution path {symbol!r}; {owner}"
                )
    errors.extend(duplicate_tool_registry_violations(rust_sources))
    errors.extend(runtime_host_boundary_violations(rust_sources))

    fixture_sources: dict[str, str] = {}
    for relative_root in EXECUTABLE_FIXTURE_ROOTS:
        root = repo_root / relative_root
        for path in sorted(candidate for candidate in root.glob("**/*") if candidate.is_file()):
            if path == Path(__file__) or "node_modules" in path.parts:
                continue
            try:
                fixture_sources[str(path.relative_to(repo_root))] = path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
    errors.extend(retired_deployment_selector_violations(fixture_sources))

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
    assert _production("prod\n#[cfg(test)]\nuse x;\nroute").endswith("route"), "E3 import"
    assert _production("prod\n#[cfg(test)]\nmod tests { route }") == "prod\n", "E3 module"
    assert duplicate_route_owner_violations(
        [("GET", "/x", "a.rs"), ("GET", "/x", "a.rs")]
    ) == [], "E4 same owner"
    assert duplicate_route_owner_violations(
        [("GET", "/x", "a.rs"), ("GET", "/x", "b.rs")]
    ), "E4 distinct owners"

    # Registry decision table: C5 authoritative owner exists; C6 every execution
    # consumer imports it; C7 a consumer declares its own RawTool HashMap.
    # R1 C5,C6,!C7 -> no violation. R2 C5,C6,C7 -> parallel registry rejected.
    sources = {
        RAW_TOOL_REGISTRY_OWNER: "pub struct RawToolRegistry;",
        **{relative: "use x::RawToolRegistry;" for relative in RAW_TOOL_REGISTRY_CONSUMERS},
    }
    assert duplicate_tool_registry_violations(sources) == [], "registry R1"
    sources[RAW_TOOL_REGISTRY_CONSUMERS[0]] += (
        "\nlet x: HashMap<String, Arc<dyn RawTool>>;"
    )
    assert duplicate_tool_registry_violations(sources), "registry R2"

    # Deployment selector decision table: D1 current Worker/Environment config
    # contains no retired selector -> accept; D2 any executable fixture revives a
    # Host-global Hand coordinate -> reject before the stale topology can ship.
    assert retired_deployment_selector_violations({"deploy/current.yaml": "worker: true"}) == [], "D1"
    assert retired_deployment_selector_violations(
        {"deploy/stale.yaml": "AWAKEN_REMOTE_HAND=hand:9000"}
    ), "D2"

    # Runtime boundary decision table: B1 Session application owns the one
    # admission decorator and Host maps typed codes -> accept; B2 decorator
    # missing -> reject; B3 Host infers identity from message text -> reject.
    boundary = {
        RUNTIME_HOST_ERROR_OWNER: "match error.code { _ => () }",
        SESSION_RUN_ADMISSION_OWNER: "pub struct AdmittedRunApplication;",
    }
    assert runtime_host_boundary_violations(boundary) == [], "B1"
    assert runtime_host_boundary_violations(
        {RUNTIME_HOST_ERROR_OWNER: boundary[RUNTIME_HOST_ERROR_OWNER]}
    ), "B2"
    boundary[RUNTIME_HOST_ERROR_OWNER] = 'message.contains("401")'
    assert runtime_host_boundary_violations(boundary), "B3"
