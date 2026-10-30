"""Execution and public-route ownership fitness checks."""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


RETIRED_EXECUTION_PATHS = {
    # Provider convergence R1: SandboxProvider only -> accepted by this scan;
    # R2: resurrecting the retired container projection anywhere -> refused;
    # R3: recreating the same effect carrier at the Session adapter -> refused.
    # The existing whole-source token matcher owns both rows (catalog change).
    "ContainerEnvironmentProvider": "reuse the sole provisioning-contract SandboxProvider",
    "ProviderEffect": "reuse the sole provisioning-contract SandboxEffect",
    "AgentContainerProvider": "SandboxProvider is the only Sandbox provider abstraction",
    "AgentContainerSession": "ContainerEnvironment plus RuntimeAgentProcess is the sole Session execution shape",
    "EnvironmentOwnedProcess": "process termination cannot authorize Environment disposal",
    "open_agent": "create_environment plus spawn_agent_process is the sole Container launch path",
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
    "SessionRunAdmission": "use the SessionApplication-owned AdmitSessionRun command",
    "AdmittedRunApplication": "use the sole SessionRunApplication adapter",
}

RUNTIME_HOST_ERROR_OWNER = "crates/server/awaken-session-executor/src/lib.rs"
SESSION_RUN_ADMISSION_OWNER = (
    "crates/server/awaken-session-application/src/run_admission.rs"
)
CONTAINER_REPOSITORY_OWNER = (
    "crates/worker/awaken-session-environment/src/container_repositories.rs"
)
SESSION_REPOSITORY_ADAPTER = (
    "crates/worker/awaken-session-environment/src/repository_realizer.rs"
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
THIS_CHECKER = Path(__file__).resolve()

ROUTE_OWNER_FILES = (
    "crates/control/awaken-admin-config-api/src/router.rs",
    "crates/control/awaken-admin-config-api/src/router/cloud_login.rs",
    "crates/control/awaken-config-service/src/capabilities.rs",
    "crates/control/awaken-control/src/config_routes.rs",
    "crates/control/awaken-control/src/config_routes/lifecycle.rs",
    "crates/control/awaken-control/src/authz.rs",
    "crates/control/awaken-control/src/admin_assistant.rs",
    "crates/control/awaken-control/src/application_mcp_credentials.rs",
    "crates/control/awaken-control/src/application_credentials.rs",
    "crates/control/awaken-control/src/mcp_catalog.rs",
    "crates/control/awaken-control/src/telemetry_policy.rs",
    "crates/control/awaken-control/src/data_subject.rs",
    "crates/control/awaken-control/src/lib.rs",
    "crates/control/awaken-control/src/webhook_authoring.rs",
    "crates/server/awaken-protocol-managed/src/resources/files.rs",
    "crates/server/awaken-protocol-managed/src/resources/memory_stores.rs",
    "crates/server/awaken-protocol-managed/src/control/models.rs",
    "crates/server/awaken-protocol-managed/src/resources/skills.rs",
    "crates/server/awaken-protocol-a2a/src/router.rs",
    "crates/server/awaken-protocol-ag-ui/src/router.rs",
    "crates/server/awaken-protocol-ai-sdk/src/router.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/content_capture.rs",
    "crates/server/awaken-protocol-awaken/src/dream_policies.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/execution_evidence.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/event_admissions.rs",
    "crates/server/awaken-protocol-awaken/src/live_inbox.rs",
    "crates/server/awaken-protocol-awaken/src/session_dispatch.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/memory_copy_conflicts.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/profiled_sessions.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/profiled_run_events.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/repositories.rs",
    "crates/server/awaken-protocol-managed/src/awaken_extensions/resource_manifests.rs",
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
    "crates/server/awaken-coordinator/src/worker_fleet.rs",
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

RAW_TOOL_REGISTRY_OWNER = "crates/contract/awaken-tool-contract/src/tool.rs"
RAW_TOOL_REGISTRY_CONSUMERS = (
    "crates/runtime/awaken-runtime/src/runtime.rs",
    "crates/worker/awaken-session-environment/src/environment.rs",
    "crates/worker/awaken-tool-relay/src/serve.rs",
)


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
    text = _crate_boundary_workspace.production_rust(
        path.read_text(encoding="utf-8")
    )
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
        if PARALLEL_RAW_TOOL_REGISTRY.search(
            _crate_boundary_workspace.production_rust(text)
        ):
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


def _is_this_checker(path: Path) -> bool:
    """Match this source independently of relative/absolute path spelling."""

    return path.resolve() == THIS_CHECKER


def runtime_host_boundary_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    host_error_projection = _crate_boundary_workspace.production_rust(
        sources.get(RUNTIME_HOST_ERROR_OWNER, "")
    )
    for token in FORBIDDEN_HOST_ERROR_INFERENCE:
        if token in host_error_projection:
            errors.append(
                f"{RUNTIME_HOST_ERROR_OWNER}: Host fault classification must use stable origin codes; forbidden {token!r}"
            )
    admission = sources.get(SESSION_RUN_ADMISSION_OWNER, "")
    required = (
        "pub struct SessionRunApplication",
        "impl RunApplication for SessionRunApplication",
        "run_admitted_session_for_owner",
    )
    if any(symbol not in admission for symbol in required):
        errors.append(
            f"{SESSION_RUN_ADMISSION_OWNER}: missing the Session-owned Run command or sole public adapter"
        )
    return errors


def repository_git_boundary_violations(sources: dict[str, str]) -> list[str]:
    """Credentialed Repository network effects exist only inside a Sandbox."""
    errors: list[str] = []
    for path, source in sources.items():
        # The Runtime Host and the Session environment adapters it is handed:
        # neither may run Repository Git on the host.
        if not path.startswith(
            (
                "crates/server/awaken-session-executor/src/",
                "crates/worker/awaken-session-environment/src/",
            )
        ):
            continue
        # A `tests.rs` module and the topic files under its `tests/` directory
        # are compiled only behind the parent's `#[cfg(test)] mod tests;`.
        if path.endswith("/tests.rs") or path.endswith("_tests.rs") or "/tests/" in path:
            continue
        production = _crate_boundary_workspace.production_rust(source)
        for forbidden in (
            "clone_repo_bundle",
            "push_repo_bundle",
            'Command::new("git")',
        ):
            if forbidden in production:
                errors.append(
                    f"{path}: Runtime Host Repository Git uses forbidden host path {forbidden!r}"
                )
    owner = _crate_boundary_workspace.production_rust(
        sources.get(CONTAINER_REPOSITORY_OWNER, "")
    )
    for forbidden in ("spawn_blocking",):
        if forbidden in owner:
            errors.append(
                f"{CONTAINER_REPOSITORY_OWNER}: Runtime Host Repository Git uses forbidden host path {forbidden!r}"
            )
    for required in (
        "publish_sandbox_control_service",
        "REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH",
        "run_repository_command(",
        "sandbox.spawn_agent_process(",
    ):
        if required not in owner:
            errors.append(
                f"{CONTAINER_REPOSITORY_OWNER}: missing Sandbox-only Repository Git mechanism {required!r}"
            )
    adapter = _crate_boundary_workspace.production_rust(
        sources.get(SESSION_REPOSITORY_ADAPTER, "")
    )
    for required in (
        "Self::Workdir { .. } | Self::Namespace { .. } if credential.is_some()",
        "credentialed Repository effects require a container Sandbox",
    ):
        if required not in adapter:
            errors.append(
                f"{SESSION_REPOSITORY_ADAPTER}: credentialed Repository bypass is not closed ({required!r})"
            )
    return errors


def retired_execution_path_violations(relative: Path, text: str) -> list[str]:
    # A necessary literal match avoids repeated regex scans of unrelated large
    # sources. The original case-sensitive Unicode word boundary remains the
    # authority, including comments and string literals in the raw Rust source.
    return [
        f"{relative}: retired execution path {symbol!r}; {owner}"
        for symbol, owner in RETIRED_EXECUTION_PATHS.items()
        if symbol in text and re.search(rf"\b{re.escape(symbol)}\b", text)
    ]


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    rust_sources: dict[str, str] = {}
    for path in sorted((repo_root / "crates").glob("**/src/**/*.rs")):
        text = path.read_text(encoding="utf-8")
        rust_sources[_crate_boundary_workspace.repository_path(path, repo_root)] = text
        errors.extend(retired_execution_path_violations(path.relative_to(repo_root), text))
    errors.extend(duplicate_tool_registry_violations(rust_sources))
    errors.extend(runtime_host_boundary_violations(rust_sources))
    errors.extend(repository_git_boundary_violations(rust_sources))

    fixture_sources: dict[str, str] = {}
    for relative_root in EXECUTABLE_FIXTURE_ROOTS:
        root = repo_root / relative_root
        for path in sorted(candidate for candidate in root.glob("**/*") if candidate.is_file()):
            if _is_this_checker(path) or "node_modules" in path.parts:
                continue
            try:
                fixture_sources[_crate_boundary_workspace.repository_path(path, repo_root)] = path.read_text(encoding="utf-8")
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
                discovered.add(_crate_boundary_workspace.repository_path(path, repo_root))
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
    # Literal presence is necessary but insufficient: preserve raw-source
    # scope, Unicode/underscore word boundaries, one result per catalog entry,
    # native path spelling and catalog order. These witnesses also reject
    # substring-only and empty-result mutants by actually executing them.
    relative = Path("crates") / "example/src/body.rs"

    def reference(source):
        return [
            f"{relative}: retired execution path {symbol!r}; {owner}"
            for symbol, owner in RETIRED_EXECUTION_PATHS.items()
            if re.search(rf"\b{re.escape(symbol)}\b", source)
        ]

    def witnesses(scan):
        boundaries = [("", ""), (" ", "\n"), ("Z_", "_Z"), ("α", "β"), ("-", "."), ("(", ")")]
        for symbol in RETIRED_EXECUTION_PATHS:
            for prefix, suffix in boundaries:
                source = prefix + symbol + suffix
                assert scan(source) == reference(source), repr(source)
            for source in [symbol + " " + symbol, "// " + symbol,
                           'let text = "' + symbol + '";', symbol.swapcase()]:
                assert scan(source) == reference(source), repr(source)
        source = " ".join(reversed(RETIRED_EXECUTION_PATHS))
        assert scan(source) == reference(source), "catalog diagnostic order"
        assert scan("ordinary Rust source") == []

    witnesses(lambda source: retired_execution_path_violations(relative, source))
    # Exercise the actual repository scan as well, so a disconnected helper
    # cannot pass only its own unit witnesses. Other missing fixture owners
    # remain unrelated diagnostics; the seeded raw-source violation must exist.
    from tempfile import TemporaryDirectory

    with TemporaryDirectory(prefix="retired-scan-") as temporary:
        root = Path(temporary)
        source = root / relative
        source.parent.mkdir(parents=True)
        symbol = next(iter(RETIRED_EXECUTION_PATHS))
        source.write_text("// " + symbol, encoding="utf-8")
        expected = reference("// " + symbol)
        observed = check_all(root)
        assert expected and all(error in observed for error in expected)
    mutants = [
        lambda source: [],
        lambda source: [
            f"{relative}: retired execution path {symbol!r}; {owner}"
            for symbol, owner in RETIRED_EXECUTION_PATHS.items() if symbol in source
        ],
    ]
    for mutant in mutants:
        try:
            witnesses(mutant)
        except AssertionError:
            continue
        raise AssertionError("weakened retired-symbol checker escaped its witnesses")

    # Cause/effect graph: C1=path parameter spelling differs; C2=method differs;
    # C3=inline cfg(test) route; C4=same key from a different owner. Effects:
    # E1 C1 normalizes to one owner key; E2 C2 remains distinct; E3 C3 is not
    # production ownership; E4 C4 fails while repeated declaration by the same
    # owner remains one authority. Decision rows
    # are asserted here so the fitness parser cannot silently weaken itself.
    assert _normalized_path("/v1/agents/{agent_id}") == "/v1/agents/{}", "E1"
    assert _normalized_path("/v1/agents/{id}") == "/v1/agents/{}", "E1"
    assert ("GET", "/x") != ("POST", "/x"), "E2"
    assert _crate_boundary_workspace.production_rust(
        "prod\n#[cfg(test)]\nuse x;\nroute"
    ).strip().endswith("route"), "E3 import"
    assert "route" not in _crate_boundary_workspace.production_rust(
        "prod\n#[cfg(test)]\nmod tests { route }"
    ), "E3 module"
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

    # Checker self-exclusion table: C1 the same checker arrives through either
    # a relative or absolute spelling; C2 a peer fixture is scanned. Effects:
    # S1 C1 is excluded exactly, preventing the retired-token catalog from
    # flagging itself; S2 C2 remains eligible for the fixture gate.
    assert _is_this_checker(Path(__file__)), "S1 absolute checker source"
    assert _is_this_checker(Path(__file__).resolve()), "S1 resolved checker source"
    assert not _is_this_checker(Path(__file__).with_name("peer.py")), "S2 peer fixture"

    # Runtime boundary decision table: B1 Session application owns the admitted
    # Run command and sole public adapter while Host maps typed codes -> accept;
    # B2 any required ownership surface is missing -> reject; B3 Host infers
    # identity from message text -> reject.
    boundary = {
        RUNTIME_HOST_ERROR_OWNER: "match error.code { _ => () }",
        SESSION_RUN_ADMISSION_OWNER: (
            "pub struct SessionRunApplication; "
            "impl RunApplication for SessionRunApplication {} "
            "fn run_admitted_session_for_owner() {}"
        ),
    }
    assert runtime_host_boundary_violations(boundary) == [], "B1"
    assert runtime_host_boundary_violations(
        {RUNTIME_HOST_ERROR_OWNER: boundary[RUNTIME_HOST_ERROR_OWNER]}
    ), "B2"
    boundary[RUNTIME_HOST_ERROR_OWNER] = 'message.contains("401")'
    assert runtime_host_boundary_violations(boundary), "B3"

    # Repository Git decision table: G1 clone+push use Sandbox processes and a
    # single control protocol -> accept; G2 host bundle/subprocess transport ->
    # reject; G3 Workdir/Namespace accept a credential -> reject; G4 moving the
    # host Git call to another Session environment module, or G5 into the
    # Runtime Host, still cannot evade the gate.
    git_boundary = {
        CONTAINER_REPOSITORY_OWNER: (
            "publish_sandbox_control_service REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH "
            "run_repository_command( sandbox.spawn_agent_process("
        ),
        SESSION_REPOSITORY_ADAPTER: (
            "Self::Workdir { .. } | Self::Namespace { .. } if credential.is_some() "
            "credentialed Repository effects require a container Sandbox"
        ),
    }
    assert repository_git_boundary_violations(git_boundary) == [], "G1"
    # G6 clone and push share one piped Sandbox executor; dropping its
    # Sandbox spawn edge cannot pass merely by retaining the helper name.
    missing_spawn = dict(git_boundary)
    missing_spawn[CONTAINER_REPOSITORY_OWNER] = missing_spawn[CONTAINER_REPOSITORY_OWNER].replace(
        "sandbox.spawn_agent_process(", ""
    )
    assert repository_git_boundary_violations(missing_spawn), "G6"
    git_boundary[CONTAINER_REPOSITORY_OWNER] += " clone_repo_bundle"
    assert repository_git_boundary_violations(git_boundary), "G2"
    git_boundary[CONTAINER_REPOSITORY_OWNER] = git_boundary[
        CONTAINER_REPOSITORY_OWNER
    ].replace(" clone_repo_bundle", "")
    git_boundary[SESSION_REPOSITORY_ADAPTER] = "credential accepted"
    assert repository_git_boundary_violations(git_boundary), "G3"
    git_boundary[SESSION_REPOSITORY_ADAPTER] = (
        "Self::Workdir { .. } | Self::Namespace { .. } if credential.is_some() "
        "credentialed Repository effects require a container Sandbox"
    )
    git_boundary[
        "crates/worker/awaken-session-environment/src/host_git_bypass.rs"
    ] = 'Command::new("git")'
    assert repository_git_boundary_violations(git_boundary), "G4"
    del git_boundary["crates/worker/awaken-session-environment/src/host_git_bypass.rs"]
    git_boundary["crates/server/awaken-session-executor/src/host_git_bypass.rs"] = (
        'Command::new("git")'
    )
    assert repository_git_boundary_violations(git_boundary), "G5"
