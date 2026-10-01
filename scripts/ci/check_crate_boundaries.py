#!/usr/bin/env python3
"""Enforce metadata-derived dependency direction and semantic ownership rules."""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from pathlib import Path

import _arch_fitness
import _cancel_safety_fitness
import _coordinator_authority_fitness
import _crate_dependency_fitness
import _execution_ownership_fitness
import _managed_protocol_boundary
import _migration_fitness
import _ownership_fence_fitness
import _provider_env_fitness
import _realization_permit_fitness
import _resource_plane_fitness
import _runtime_secret_boundary
import _session_effect_fitness
import _session_state_ownership_fitness
import _service_data_ownership_fitness
import _sqlite_scheduler_fitness
import _wire_closure_fitness
import _crate_boundary_workspace
from _crate_boundary_workspace import architecture_fitness_specs, dependency_fitness_specs, text_files


REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CRATES = REPO_ROOT / "crates"

# Hosted Cloud placement is foreign configuration, not an Awaken domain.
# Exactly one CLI composition adapter may decode it and project scoped storage
# handles; every other crate is mechanically prevented from naming it.
CLOUD_PLACEMENT_TOKENS = (
    "DataAuthority",
    "DataShard",
    "CellRuntimeIdentity",
    "data_shard_id",
    "data_authority_epoch",
    "data_shard_class",
    "runtime_cell_id",
    "runtime_cell_incarnation",
    "runtime_placement_epoch",
)
CLOUD_HOSTING_ACL = Path("crates/bin/awaken-cli/src/config/workspace_data.rs")

NEUTRAL_CRATES = {
    "awaken-agent-contract",
    "awaken-runtime-contract",
    "awaken-tool-contract",
    "awaken-runtime",
}
EXTENSION_CRATES = {
    "awaken-ext-background-task",
    "awaken-ext-builtin-tools",
    "awaken-ext-permission",
}
FORBIDDEN_NEUTRAL_TERMS = {"managed"}

# A leaf backend (an ACP CLI today) reports its own session modes and config
# options. The core relays those as opaque identifiers: it never reads their
# meaning, so a new CLI needs no mapping table here and an unknown value can
# never silently change an authorization decision. These are the values whose
# meaning belongs to the backend alone; naming one inside the neutral core or a
# permission/extension crate would be the core starting to interpret them.
OPAQUE_BACKEND_CONFIG_VALUES = {
    "acceptEdits",
    "bypassPermissions",
}
# Crates that decide authorization must not even be able to see the ACP
# vocabulary: the dependency is rejected before any code can name it.
OPAQUE_BACKEND_CONFIG_CRATES = {
    "awaken-agent-contract",
    "awaken-runtime-contract",
    "awaken-tool-contract",
    "awaken-runtime",
    "awaken-ext-permission",
}
FORBIDDEN_BACKEND_CONFIG_DEPS = {
    "awaken-acp-contract",
    "awaken-protocol-acp",
    "awaken-run-executor-acp",
}

# The runtime tier ships as an embeddable SDK: an application links
# `awaken-runtime` plus the extensions it wants and runs agents without any
# server, coordinator, or worker process. That only stays true while no runtime
# crate links a serving assembly, so what it may depend on is enumerated here
# rather than left to convention.
#
# The `shared` context is deliberately NOT a blanket allowance: it holds neutral
# values (`awaken-tool-pattern`) next to server assemblies (`awaken-session-executor`),
# so admitting it wholesale would let the host back into the SDK. Ports and
# values are admitted by layer instead, and each neutral shared library is named
# so adding another is a reviewed decision rather than a side effect.
SDK_CONTEXT = "runtime"
SDK_ALLOWED_DEPENDENCY_LAYERS = {"contract", "domain"}
SDK_ALLOWED_SHARED_LIBRARIES = {
    # Provider-neutral MCP codec and HTTP client. Both are plain client
    # libraries with no serving role, and `awaken-ext-mcp` needs them to speak
    # MCP from an embedded runtime.
    "awaken-mcp-wire",
    "awaken-outbound-http",
}
ACTIVE_BUILTIN_TOOL_IDS = {
    "bash", "read", "write", "edit", "glob", "grep", "web_fetch", "web_search",
    "agent_run", "list_agents", "send_message",
}
RETIRED_BUILTIN_TOOL_IDS = {
    "move", "delete", "repository_inspect", "repository_commit",
    "send_to_agent", "cancel_task", "recover_failed_messages",
}
FORBIDDEN_BUILTIN_TOOL_ID_PREFIXES = ("git_", "repository_")
BUILTIN_TOOL_IDS = ACTIVE_BUILTIN_TOOL_IDS | RETIRED_BUILTIN_TOOL_IDS
FORBIDDEN_NEUTRAL_TYPE_NAMES = {
    "TypedTool": "use Tool for the typed API and RawTool for the low-level adapter",
    "BackgroundTask": "detached-tool state belongs in awaken-ext-background-task",
    "ToolExecutionLocus": "ToolExecutor is the sole neutral tool port",
    "ExecutorAdapter": "the executing side implements ToolExecutor",
    "ExecutionBackend": "tool execution is in-process; agent attempts use RunAttemptExecutor",
}
FORBIDDEN_NEUTRAL_PHRASES = {
    "worker placement": "worker placement is outside runtime core",
    "sandbox placement": "sandbox placement is outside runtime core",
}
ACTIVE_BUILTIN_SYMBOLS = {
    "AgentRunArgs",
    "BashArgs", "BashTool",
    "EditArgs", "EditTool",
    "GlobArgs", "GlobTool",
    "GrepArgs", "GrepTool",
    "ListAgentsArgs", "ListAgentsTool",
    "ReadArgs", "ReadTool",
    "SendMessageArgs", "SendMessageTool",
    "ProviderServerWebFetchTool", "ProviderServerWebSearchTool",
    "RoutedWebFetchTool", "WebFetchArgs", "WebSearchArgs", "WebSearchTool",
    "WriteArgs", "WriteTool",
}
FORBIDDEN_ACTIVE_BUILTIN_ALIASES = {
    "AgentRun", "Bash", "Edit", "Glob", "Grep", "ListAgents", "Read",
    "WebFetch", "WebSearch", "Write",
}
RETIRED_BUILTIN_SYMBOLS = {
    "AgentRunTool", "CancelTask", "CancelTaskArgs", "CancelTaskTool",
    "DeleteArgs", "DeleteTool", "MoveArgs", "MoveTool",
    "RecoverFailedMessages", "RecoverFailedMessagesArgs",
    "RecoverFailedMessagesTool",
    "RepositoryCommitTool", "RepositoryInspectTool", "SendMessage", "WebFetchTool",
    "SendToAgent", "SendToAgentArgs", "SendToAgentTool",
}
FORBIDDEN_NEUTRAL_SYMBOLS = {
    **{name: f"{name} is a concrete active builtin owned by awaken-ext-builtin-tools"
       for name in ACTIVE_BUILTIN_SYMBOLS},
    **{name: f"{name} is a retired builtin and must not be reintroduced"
       for name in RETIRED_BUILTIN_SYMBOLS},
    "ConfigPublicationCoordinator": "configuration publication stays outside runtime core",
    "RegistryCompiler": "registry compilation stays outside runtime core",
}
FORBIDDEN_NEUTRAL_IMPL_TRAITS = {
    "Tool": "concrete tools belong in extensions/adapters",
    "RawTool": "concrete tools belong in extensions/adapters",
}


def _patterns(words: set[str] | dict[str, str], *, ignore_case: bool = False) -> dict[str, re.Pattern[str]]:
    flags = re.IGNORECASE if ignore_case else 0
    return {word: re.compile(rf"\b{re.escape(word)}\b", flags) for word in words}


def _rust_use_imports(content: str) -> list[tuple[bool, tuple[str, ...], str, bool]]:
    """Return public flag, source path, local name, and rename flag for Rust use trees."""
    content = _crate_boundary_workspace.rust_syntax(content)
    statements = re.compile(
        r"(?ms)^\s*(?P<visibility>pub(?:\([^)]*\))?\s+)?use\s+(?P<tree>[^;]+);"
    )
    token_pattern = re.compile(r"::|[A-Za-z_][A-Za-z0-9_]*|\{|\}|,|\*")
    imports: list[tuple[bool, tuple[str, ...], str, bool]] = []

    def parse_tree(
        tokens: list[str],
        position: int,
        prefix: tuple[str, ...],
        public: bool,
    ) -> int:
        segments: list[str] = []
        while position < len(tokens) and tokens[position] not in {"{", "}", ",", "as", "*"}:
            if tokens[position] != "::":
                segments.append(tokens[position])
            position += 1
        path = prefix + tuple(segments)
        if position < len(tokens) and tokens[position] == "as":
            if position + 1 < len(tokens):
                imports.append((public, path, tokens[position + 1], True))
                return position + 2
            return len(tokens)
        if position < len(tokens) and tokens[position] == "{":
            position += 1
            while position < len(tokens) and tokens[position] != "}":
                position = parse_tree(tokens, position, path, public)
                if position < len(tokens) and tokens[position] == ",":
                    position += 1
            return position + 1 if position < len(tokens) else position
        if position < len(tokens) and tokens[position] == "*":
            imports.append((public, path, "*", False))
            return position + 1
        if path:
            if path[-1] == "self" and len(path) > 1:
                imports.append((public, path[:-1], path[-2], False))
            elif path[-1] != "self":
                imports.append((public, path, path[-1], False))
        return position

    for statement in statements.finditer(content):
        tokens = token_pattern.findall(statement.group("tree"))
        position = 0
        while position < len(tokens):
            if tokens[position] == ",":
                position += 1
                continue
            if tokens[position] == "}":
                break
            next_position = parse_tree(
                tokens,
                position,
                (),
                statement.group("visibility") is not None,
            )
            position = next_position if next_position > position else position + 1
    return imports


def _declares_alias(content: str, alias: str) -> bool:
    content = _crate_boundary_workspace.rust_syntax(content)
    declaration = re.compile(
        rf"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?"
        rf"(?:(?:async|unsafe|const)\s+)*"
        rf"(?:struct|enum|type|trait|const|static|fn)\s+{re.escape(alias)}\b"
    )
    return declaration.search(content) is not None


def _imports_forbidden_builtin_alias(content: str, alias: str) -> bool:
    allowed_standard_traits = {
        ("Read", ("std", "io", "Read")),
        ("Write", ("std", "io", "Write")),
        ("Write", ("std", "fmt", "Write")),
    }
    for _, path, local_name, _ in _rust_use_imports(content):
        if local_name != alias and alias not in path:
            continue
        if (alias, path) in allowed_standard_traits:
            continue
        return True
    return False


def _implements_trait(content: str, trait: str) -> bool:
    content = _crate_boundary_workspace.rust_syntax(content)
    return re.search(
        rf"(?ms)\b(?:unsafe\s+)?impl\s*"
        rf"(?:<[^{{}};]*>\s*)?"
        rf"(?:::)?\s*"
        rf"(?:(?:[A-Za-z_][A-Za-z0-9_]*|r#[A-Za-z_][A-Za-z0-9_]*)\s*::\s*)*"
        rf"{re.escape(trait)}\b(?:\s*<[^{{}};]*>)?\s+for\b",
        content,
    ) is not None


def _forbidden_neutral_impl_errors(rel: Path, content: str) -> list[str]:
    return [
        f"{rel}: concrete {trait} implementation in neutral code; {reason}"
        for trait, reason in FORBIDDEN_NEUTRAL_IMPL_TRAITS.items()
        if _implements_trait(content, trait)
    ]


def _forbidden_neutral_symbol_errors(rel: Path, content: str) -> list[str]:
    errors: list[str] = []
    for name, pattern in _patterns(FORBIDDEN_NEUTRAL_SYMBOLS).items():
        if pattern.search(content):
            errors.append(
                f"{rel}: forbidden neutral symbol {name!r}; "
                f"{FORBIDDEN_NEUTRAL_SYMBOLS[name]}"
            )
    for alias in FORBIDDEN_ACTIVE_BUILTIN_ALIASES:
        if _declares_alias(content, alias) or _imports_forbidden_builtin_alias(
            content, alias
        ):
            errors.append(
                f"{rel}: forbidden active builtin compatibility alias {alias!r}"
            )
    return errors


def check_neutral_code_boundaries() -> list[str]:
    errors: list[str] = []
    terms = _patterns(FORBIDDEN_NEUTRAL_TERMS, ignore_case=True)
    tools = {tool: re.compile(rf'"{re.escape(tool)}"') for tool in BUILTIN_TOOL_IDS}
    types = _patterns(FORBIDDEN_NEUTRAL_TYPE_NAMES)
    phrases = _patterns(FORBIDDEN_NEUTRAL_PHRASES, ignore_case=True)
    for crate_name in NEUTRAL_CRATES:
        for path in text_files(crate_name):
            rel = path.relative_to(REPO_ROOT)
            if _session_state_ownership_fitness._is_test_module(str(rel)):
                continue
            content = _crate_boundary_workspace.production_rust(
                path.read_text(encoding="utf-8")
            )
            for term, pattern in terms.items():
                if pattern.search(content):
                    errors.append(f"{rel}: neutral crate uses product term {term!r}")
            for tool, pattern in tools.items():
                if pattern.search(content):
                    errors.append(f"{rel}: concrete builtin tool id {tool!r} leaked into neutral code")
            for name, pattern in types.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral type {name!r}; {FORBIDDEN_NEUTRAL_TYPE_NAMES[name]}")
            errors.extend(_forbidden_neutral_symbol_errors(rel, content))
            for phrase, pattern in phrases.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral phrase {phrase!r}; {FORBIDDEN_NEUTRAL_PHRASES[phrase]}")
            errors.extend(_forbidden_neutral_impl_errors(rel, content))
    return errors


def _forbidden_builtin_source_errors(rel: Path, content: str) -> list[str]:
    errors: list[str] = []
    retired_ids = {
        tool: re.compile(rf'"{re.escape(tool)}"')
        for tool in RETIRED_BUILTIN_TOOL_IDS
    }
    retired_symbols = _patterns(RETIRED_BUILTIN_SYMBOLS)
    string_literal = re.compile(r'"([^"\\]*(?:\\.[^"\\]*)*)"')
    for tool, pattern in retired_ids.items():
        if pattern.search(content):
            errors.append(f"{rel}: retired builtin tool id {tool!r} must not be reintroduced")
    for symbol, pattern in retired_symbols.items():
        if pattern.search(content):
            errors.append(f"{rel}: retired builtin tool symbol {symbol!r} must not be reintroduced")
    for alias in FORBIDDEN_ACTIVE_BUILTIN_ALIASES:
        if _declares_alias(content, alias) or _imports_forbidden_builtin_alias(
            content, alias
        ):
            errors.append(
                f"{rel}: active builtin compatibility alias {alias!r} must not be introduced"
            )
    for literal in string_literal.findall(content):
        if literal in RETIRED_BUILTIN_TOOL_IDS:
            continue
        if literal == "git" or literal.startswith(FORBIDDEN_BUILTIN_TOOL_ID_PREFIXES):
            errors.append(
                f"{rel}: Git/repository-specific builtin tool id {literal!r} "
                "must remain opaque Bash data"
            )
    return errors


def selftest_builtin_tool_ownership() -> None:
    # Cause/effect decision table: C1=safe Bash/protocol wording or neutral
    # domain Read/Write enum variants (not declarations/imports) => E1 accepted;
    # C2=retired exact ID, C3=Git/repository ID, C4=retired Tool/Args or active
    # compatibility alias in the builtin owner, C5=an actual active Tool/Args,
    # active alias, or retired symbol in neutral code, C6=a concrete Tool/RawTool
    # implementation in neutral code => E2 rejected.
    # C7=comments interrupt a multiline use tree or contain fake imports; C8=a
    # concrete trait path begins at the crate root with `::`; C9=a forbidden
    # source alias is renamed, grouped, or followed by another path segment;
    # C10=impl-like prose occurs only in a string literal. Effects: E1 accept
    # safe code and exact std trait imports in private/public direct/root/grouped
    # forms; E2 reject C2-C6/C8/C9; E3 parse only executable C7/C10 syntax.
    # These mutation probes call the same helpers used on source, so neither a
    # rename-away nor a commented/literal copy can satisfy the gate.
    rel = Path("crates/runtime/awaken-ext-builtin-tools/src/example.rs")
    assert not _forbidden_builtin_source_errors(
        rel,
        'const ID: &str = "bash"; async fn cancel_task() {}',
    ), "S1/E1"
    assert not _forbidden_builtin_source_errors(
        rel,
        "use std::io::{Read, Write};",
    ), "S1/E1 standard IO traits"
    for forbidden in (
        *(f'const ID: &str = "{tool}";' for tool in RETIRED_BUILTIN_TOOL_IDS),
        'const ID: &str = "git_status";',
        'const ID: &str = "repository_apply";',
        *(f"struct {symbol};" for symbol in RETIRED_BUILTIN_SYMBOLS),
        *(f"struct {alias};" for alias in FORBIDDEN_ACTIVE_BUILTIN_ALIASES),
        "type Bash = ();",
        "use crate::BashTool as Bash;",
        "use compat::Bash;",
        "use compat::{Bash, ReadTool};",
        "pub use compat::Bash;",
        "pub use compat::{Bash, ReadTool};",
        "pub use crate::BashTool as Bash;",
        "use compat::Bash as Shell;",
        "use compat::{Bash as Shell, ReadTool};",
        "use compat::Bash::Shell;",
    ):
        assert _forbidden_builtin_source_errors(rel, forbidden), f"S2/E2: {forbidden}"
    neutral_rel = Path("crates/contract/awaken-runtime-contract/src/example.rs")
    assert not _forbidden_neutral_symbol_errors(
        neutral_rel,
        "use std::{io::Read, fmt::Write}; use {std::io::Write};",
    ), "S1/E1 standard IO and formatting traits"
    for allowed_standard_import in (
        "use std::io::Read;",
        "pub use std::io::Read;",
        "pub use std::{io::Read, fmt::Write};",
        "pub(crate) use {std::io::Write};",
    ):
        assert not _forbidden_neutral_symbol_errors(
            neutral_rel, allowed_standard_import
        ), f"S1/E1 exact standard trait import: {allowed_standard_import}"
    commented_use_tree = """
use crate::{
    /* a semicolon ; and fake brace } stay inert */ BashTool as /* split */ Bash,
    // use compat::Write;
    ReadTool,
};
"""
    assert _forbidden_neutral_symbol_errors(neutral_rel, commented_use_tree), (
        "S3/C7 executable multiline alias survives comments"
    )
    assert not _imports_forbidden_builtin_alias(
        "// use compat::Write;\nuse std::io::Write;", "Write"
    ), "S3/C7 commented fake import is inert"
    assert not _imports_forbidden_builtin_alias(
        'const EXAMPLE: &str = "use compat::Write;";\nuse std::io::Write;',
        "Write",
    ), "S3/C7 string example is inert"
    for forbidden in (
        *(f"struct {symbol};" for symbol in ACTIVE_BUILTIN_SYMBOLS),
        *(f"struct {alias};" for alias in FORBIDDEN_ACTIVE_BUILTIN_ALIASES),
        *(f"struct {symbol};" for symbol in RETIRED_BUILTIN_SYMBOLS),
        "use compat::Bash;",
        "use compat::{Bash, ReadTool};",
        "use compat::Read;",
        "use compat::Bash as Shell;",
        "use compat::{Bash as Shell, ReadTool};",
        "use compat::Bash::Shell;",
    ):
        assert _forbidden_neutral_symbol_errors(neutral_rel, forbidden), (
            f"S2/E2: {forbidden}"
        )
    for forbidden in (
        "impl Tool for Concrete {}",
        "impl awaken_runtime_contract::tool::RawTool for Concrete {}",
        "impl<T: Bound<Vec<U>>> Tool for Concrete<T> {}",
        "impl ::awaken_runtime_contract::tool::Tool for Concrete {}",
        "impl ::awaken_runtime_contract::tool::RawTool for Concrete {}",
    ):
        assert _forbidden_neutral_impl_errors(neutral_rel, forbidden), (
            f"S2/E2 concrete neutral implementation: {forbidden}"
        )
    assert not _forbidden_neutral_impl_errors(
        neutral_rel,
        'const EXAMPLE: &str = "impl Tool for Fake {}";\n'
        'const RAW: &str = r#"impl RawTool for Fake {}"#;',
    ), "S3/C10 impl-like string literals are inert"
    resource_modes = Path("crates/contract/awaken-tool-contract/src/tool.rs")
    assert not _forbidden_neutral_symbol_errors(
        resource_modes,
        "enum ResourceAccessMode { Read, Write }",
    ), "S1/E1 domain access variants"
    for forbidden in (
        "pub struct Read;",
        "pub type Write = ();",
        "pub async fn Bash() {}",
    ):
        assert _forbidden_neutral_symbol_errors(resource_modes, forbidden), (
            f"S2/E2 declaration in former allowlisted path: {forbidden}"
        )


def check_builtin_tool_ownership() -> list[str]:
    """Keep retired and Git-specific commands out of the builtin owner.

    Cause/effect decision table: C1=an exact retired id literal is present;
    C2=a Git/repository-specific id literal is present; C3=a retired concrete
    tool symbol is present; C4=an active tool compatibility alias is declared or
    imported. R1=C1|C2|C3|C4 => fail before the command can enter the model
    catalog. The Rust catalog test independently fixes the complete active set;
    this static check catches the forbidden source even without compiling.
    """
    errors: list[str] = []
    for crate_name in EXTENSION_CRATES:
        if next(CRATES.glob(f"*/{crate_name}/Cargo.toml"), None) is None:
            errors.append(f"missing required extension crate {crate_name!r}")
    manifest = next(CRATES.glob("*/awaken-ext-builtin-tools/Cargo.toml"), None)
    if manifest is None:
        return errors
    for path in sorted((manifest.parent / "src").rglob("*.rs")):
        rel = path.relative_to(REPO_ROOT)
        content = _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8")
        )
        errors.extend(_forbidden_builtin_source_errors(rel, content))
    return errors


def check_background_task_is_state_only() -> list[str]:
    """Prevent the extension from growing a second persistence authority.

    Cause/effect rule: a normal SQL/store dependency or migration directory
    implies extension-owned durable storage and is rejected; dev-only test
    dependencies remain outside this check.
    """
    errors: list[str] = []
    spec = next(
        (spec for spec in dependency_fitness_specs() if spec.name == "awaken-ext-background-task"),
        None,
    )
    if spec is None:
        return ["missing required extension crate awaken-ext-background-task"]
    forbidden = sorted(
        dependency
        for dependency in spec.normal_deps
        if dependency in {"sqlx", "rusqlite"} or dependency.endswith("-store")
    )
    if forbidden:
        errors.append(
            "awaken-ext-background-task must persist only through Runtime State; "
            f"forbidden normal dependencies: {forbidden}"
        )
    manifest = next(CRATES.glob("*/awaken-ext-background-task/Cargo.toml"))
    if any(path.is_dir() and path.name == "migrations" for path in manifest.parent.rglob("*")):
        errors.append("awaken-ext-background-task must not own a migrations directory")
    return errors


def check_tests_are_not_arch_owners() -> list[str]:
    errors: list[str] = []
    for crate_name in NEUTRAL_CRATES:
        manifest = next(CRATES.glob(f"*/{crate_name}/Cargo.toml"), None)
        if manifest is None or not (manifest.parent / "tests").exists():
            continue
        for path in sorted((manifest.parent / "tests").rglob("*.rs")):
            if "TypedTool" in path.read_text(encoding="utf-8"):
                errors.append(f"{path.relative_to(REPO_ROOT)}: tests normalize forbidden TypedTool")
    return errors


# L1 guard for outbound transport construction. Selecting the process-wide
# rustls provider and deciding whether a channel may inherit an ambient
# `HTTP(S)_PROXY` were once obligations remembered at each client construction
# site, which is why they were honoured in two crates and forgotten in fifteen.
# `awaken-outbound-http` now owns both as properties of `InternalHttpClient` /
# `ExternalHttpClient`, so a crate that cannot name `reqwest` or `rustls` cannot
# express an unguarded client at all. Declaring one of those crates as a normal
# dependency therefore requires an entry here saying why this crate must own
# transport construction itself. Test-only dependencies are out of scope: a
# `[dev-dependencies]` client is a composition root exercising a real server,
# not a production egress path.
TRANSPORT_CRATES = ("reqwest", "rustls")
DIRECT_TRANSPORT_EXEMPTIONS = {
    "awaken-outbound-http": (
        "the transport authority itself: the one crate that selects the rustls "
        "provider and owns the internal/external proxy variants"
    ),
    "awaken-provider-genai": (
        "pins reqwest 0.13 because the `genai` SDK takes a 0.13 `Client` through "
        "`with_reqwest`; the authority's 0.12 client is a different type and "
        "cannot cross that semver island"
    ),
    "awaken-ext-mcp": (
        "streams MCP server-sent events off a live `Response` body and mirrors "
        "arbitrary server headers; a JSON request/reply pair cannot express it"
    ),
    "awaken-credential-materializer": (
        "re-signs and replays one prepared `RequestBuilder` after a credential "
        "rotation, so the retry seam is the request builder itself"
    ),
    "awaken-control": (
        "the Control-owned OAuth Vault refresher sends form-encoded token exchanges "
        "with Basic/client-secret authentication; the JSON-only external client "
        "cannot express that request, while construction still uses the one "
        "awaken-outbound-http rustls builder"
    ),
    "awaken-session-executor": (
        "the MCP relay is a reverse proxy: it forwards a caller-chosen method, "
        "header set and raw body, none of which the JSON client models"
    ),
    "awaken-worker-transport-security": (
        "second transport authority, for the Worker-to-Coordinator plane: owns "
        "the signed `RequestBuilder` seam and the private-CA client; it still "
        "takes its provider selection from awaken-outbound-http"
    ),
    "awaken-coordinator-client": (
        "implements the Worker-to-Coordinator signing seam, whose port is "
        "defined over `reqwest::RequestBuilder` by the transport authority above"
    ),
}

# Even a registered transport owner starts from the authority's builder, which
# has already installed the provider. Naming a bare `reqwest::Client`
# constructor in production code is what these two crates alone may do.
TRANSPORT_CONSTRUCTION_EXEMPTIONS = {
    "awaken-outbound-http": "the authority; `rustls_client_builder` is defined here",
    "awaken-provider-genai": (
        "builds a reqwest 0.13 client for the `genai` SDK; the 0.12 builder this "
        "authority hands out is a different type"
    ),
}
TRANSPORT_CONSTRUCTION = re.compile(r"\breqwest::Client::(?:new|builder)\s*\(")


def check_no_direct_transport_dependency(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES
) -> list[str]:
    """Transport construction lives in the authority crate or a stated exception."""
    errors: list[str] = []
    for manifest_path in sorted(crates.glob("*/*/Cargo.toml")):
        manifest = _crate_boundary_workspace.load_manifest(manifest_path)
        try:
            name = _crate_boundary_workspace.package_name(manifest)
        except KeyError:
            continue
        relative = manifest_path.relative_to(repo_root)
        declared = sorted(
            dependency
            for dependency in _crate_boundary_workspace.normal_dependency_names(manifest)
            if dependency in TRANSPORT_CRATES
        )
        justification = DIRECT_TRANSPORT_EXEMPTIONS.get(name)
        if declared and justification is None:
            errors.append(
                f"{relative}: {name} declares transport dependency "
                f"{', '.join(declared)} directly; depend on awaken-outbound-http and "
                "build an InternalHttpClient/ExternalHttpClient, or record in "
                "DIRECT_TRANSPORT_EXEMPTIONS why this crate must own transport "
                "construction itself"
            )
        if not declared and justification is not None:
            errors.append(
                f"{relative}: {name} no longer declares {' or '.join(TRANSPORT_CRATES)}; "
                "remove its stale DIRECT_TRANSPORT_EXEMPTIONS entry"
            )
        if name in TRANSPORT_CONSTRUCTION_EXEMPTIONS:
            continue
        for path in sorted(_crate_boundary_workspace.production_module_files([manifest_path])):
            production = _crate_boundary_workspace.production_rust(
                path.read_text(encoding="utf-8", errors="ignore")
            )
            if TRANSPORT_CONSTRUCTION.search(production):
                errors.append(
                    f"{path.relative_to(repo_root)}: constructs a reqwest client "
                    "directly, so the rustls provider and proxy policy become call-site "
                    "obligations again; use awaken_outbound_http::InternalHttpClient, "
                    "ExternalHttpClient, or rustls_client_builder()"
                )
    return errors


def selftest_no_direct_transport_dependency() -> None:
    """Cause/effect: an unregistered transport edge or bare constructor rejects."""
    # C1 the crate declares `reqwest`/`rustls` as a normal dependency;
    # C2 the crate has a DIRECT_TRANSPORT_EXEMPTIONS entry;
    # C3 production source names a bare `reqwest::Client` constructor;
    # C4 that constructor sits in test-only code;
    # C5 the crate has a TRANSPORT_CONSTRUCTION_EXEMPTIONS entry;
    # C6 the dependency is declared under `[dev-dependencies]` only.
    # Effects: E1 reject; E2 admit.
    #
    # | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
    # | T1   | T  | F  | F  | -  | -  | F  | E1 (unregistered transport edge) |
    # | T2   | T  | T  | F  | -  | -  | F  | E2 |
    # | T3   | F  | T  | F  | -  | -  | F  | E1 (stale registry entry)        |
    # | T4   | F  | F  | T  | F  | F  | F  | E1 (bare constructor)            |
    # | T5   | F  | F  | T  | T  | F  | F  | E2 |
    # | T6   | F  | F  | T  | F  | T  | F  | E2 |
    # | T7   | F  | F  | F  | -  | -  | T  | E2 (tests are composition roots) |
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        manifest = crates / "shared/example/Cargo.toml"
        manifest.parent.mkdir(parents=True)
        source = manifest.parent / "src/lib.rs"
        source.parent.mkdir(parents=True)
        source.write_text("pub fn nothing() {}\n", encoding="utf-8")

        with_dependency = (
            '[package]\nname = "example"\n\n[dependencies]\n'
            'reqwest = { version = "0.12" }\n'
        )
        without_dependency = '[package]\nname = "example"\n'
        dev_only = (
            '[package]\nname = "example"\n\n[dev-dependencies]\n'
            'reqwest = { version = "0.12" }\n'
        )

        manifest.write_text(with_dependency, encoding="utf-8")
        assert check_no_direct_transport_dependency(root, crates)  # T1
        DIRECT_TRANSPORT_EXEMPTIONS["example"] = "selftest"
        try:
            assert not check_no_direct_transport_dependency(root, crates)  # T2
            manifest.write_text(without_dependency, encoding="utf-8")
            assert check_no_direct_transport_dependency(root, crates)  # T3
        finally:
            del DIRECT_TRANSPORT_EXEMPTIONS["example"]

        source.write_text("pub fn f() { let _ = reqwest::Client::new(); }\n", encoding="utf-8")
        assert check_no_direct_transport_dependency(root, crates)  # T4
        source.write_text(
            "#[cfg(test)]\nmod tests { fn f() { let _ = reqwest::Client::builder(); } }\n",
            encoding="utf-8",
        )
        assert not check_no_direct_transport_dependency(root, crates)  # T5
        source.write_text("pub fn f() { let _ = reqwest::Client::new(); }\n", encoding="utf-8")
        TRANSPORT_CONSTRUCTION_EXEMPTIONS["example"] = "selftest"
        try:
            assert not check_no_direct_transport_dependency(root, crates)  # T6
        finally:
            del TRANSPORT_CONSTRUCTION_EXEMPTIONS["example"]

        source.write_text("pub fn nothing() {}\n", encoding="utf-8")
        manifest.write_text(dev_only, encoding="utf-8")
        assert not check_no_direct_transport_dependency(root, crates)  # T7


# L2 guard for the optimistic-concurrency witness (see
# `awaken_resource_contract::CompleteGeneration`). A type whose name ends in
# `Generation` is, by the repository's own vocabulary, the thing a conditional
# mutation compares against a durable head. Such a type must be a complete
# witness — immutable identity, monotonic version, observed content digest —
# so that a bare counter or a bare digest can never become a CAS precondition.
# Declaring one therefore requires either an `impl CompleteGeneration` beside
# it or an entry here recording, in prose at the type, why it never gates a
# conditional write.
INCOMPLETE_GENERATION_EXEMPTIONS = {
    # Assigned once and never rotated; its `id` is already the content digest
    # of every other coordinate; every consumer compares it as effect identity
    # on a receipt, never as a precondition on a durable head. The type carries
    # the full argument.
    "SandboxGeneration": "crates/contract/awaken-session-state-contract/src/environment/generation.rs",
}


def check_generation_witnesses_are_complete(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES
) -> list[str]:
    """Every `*Generation` value type is a complete CAS witness or a stated exception."""
    errors: list[str] = []
    declaration = re.compile(r"\bpub struct (\w*Generation)\b")
    for path in sorted(crates.glob("*/*/src/**/*.rs")):
        relative = path.relative_to(repo_root)
        production = _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        for name in declaration.findall(production):
            exemption = INCOMPLETE_GENERATION_EXEMPTIONS.get(name)
            if exemption is not None:
                if exemption == str(relative):
                    continue
                errors.append(
                    f"{relative}: {name} is exempted from CompleteGeneration at "
                    f"{exemption}, not here; move or re-justify the exemption"
                )
                continue
            implemented = f"CompleteGeneration for {name}" in production
            if not implemented:
                errors.append(
                    f"{relative}: {name} is a compare-and-set witness but does not "
                    "implement awaken_resource_contract::CompleteGeneration; supply the "
                    "identity, version and digest coordinates, or record in "
                    "INCOMPLETE_GENERATION_EXEMPTIONS why it never gates a conditional write"
                )
    return errors


def selftest_generation_witnesses() -> None:
    """Cause/effect: an incomplete witness rejects; a complete or stated one admits."""
    # C1 a `*Generation` type is declared with no `CompleteGeneration` impl;
    # C2 the same file also implements the trait for it; C3 the name carries a
    # recorded exemption at this path; C4 it carries one at another path;
    # C5 the declaration is test-only. Effects: E1 reject; E2 admit.
    #
    # | Rule | impl | exempt here | exempt elsewhere | test-only | Effect |
    # | G1   | F    | F           | F                | F         | E1     |
    # | G2   | T    | F           | F                | F         | E2     |
    # | G3   | F    | T           | F                | F         | E2     |
    # | G4   | F    | F           | T                | F         | E1     |
    # | G5   | F    | F           | F                | T         | E2     |
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        source = crates / "contract/example/src/lib.rs"
        source.parent.mkdir(parents=True)
        source.write_text("pub struct WidgetGeneration { version: u64 }\n", encoding="utf-8")
        assert check_generation_witnesses_are_complete(root, crates)  # G1
        source.write_text(
            "pub struct WidgetGeneration { version: u64 }\n"
            "impl CompleteGeneration for WidgetGeneration {}\n",
            encoding="utf-8",
        )
        assert not check_generation_witnesses_are_complete(root, crates)  # G2
        source.write_text("pub struct ExemptGeneration { id: String }\n", encoding="utf-8")
        INCOMPLETE_GENERATION_EXEMPTIONS["ExemptGeneration"] = str(
            source.relative_to(root)
        )
        try:
            assert not check_generation_witnesses_are_complete(root, crates)  # G3
            INCOMPLETE_GENERATION_EXEMPTIONS["ExemptGeneration"] = "crates/other/src/lib.rs"
            assert check_generation_witnesses_are_complete(root, crates)  # G4
        finally:
            del INCOMPLETE_GENERATION_EXEMPTIONS["ExemptGeneration"]
        source.write_text(
            "#[cfg(test)] pub struct FixtureGeneration { version: u64 }\n",
            encoding="utf-8",
        )
        assert not check_generation_witnesses_are_complete(root, crates)  # G5
# The MCP tool identity rule lives in its own module: see
# _mcp_tool_identity_fitness.py. It is re-exported here so this file stays the
# one entry point that runs every boundary rule.
from _mcp_tool_identity_fitness import (  # noqa: E402
    MCP_TOOL_IDENTITY_EXEMPTIONS,
    MCP_TOOL_IDENTITY_GRAMMAR,
    MCP_TOOL_IDENTITY_OWNER,
    MCP_TOOL_IDENTITY_TS_EXEMPTIONS,
    MCP_TOOL_IDENTITY_TS_GRAMMAR,
    MCP_TOOL_IDENTITY_TS_OWNER,
    MCP_TOOL_IDENTITY_TS_ROOT,
    check_mcp_tool_identity_is_owned,
    check_mcp_tool_identity_is_owned_in_console,
    selftest_mcp_tool_identity_ownership,
    selftest_mcp_tool_identity_ownership_in_console,
)





# ---------------------------------------------------------------------------
# Tenancy is a required ingress coordinate (ADR-0051).
#
# The recurring defect is one shape: the tenant handle arrives as an OPTIONAL
# request-local value and each handler decides what `None` means. Nine handlers
# independently decided it meant the seeded Workspace, so an unstamped request
# silently became a write owned by a phantom tenant. Two gates make that shape
# unrepresentable rather than merely absent:
#
#   * `check_tenancy_is_not_optional` — a handler may not accept
#     `Option<Extension<WorkspaceScope | ResourceWorkspace | ExecutionWorkspace>>`.
#     With a required extractor there is no `None` arm to give a meaning to.
#   * `check_no_ambient_tenant_default` — no code may fall back to the default
#     tenant. The anonymous single-machine composition gets its Workspace from
#     exactly one composition-root layer, never from a handler.
#
# Both scan the whole repository. A file allow-list would only freeze today's
# offenders; an exemption registry forces each survivor to state its router, why
# that router provably carries no tenancy edge, and what absence does instead.
TENANCY_HANDLE_TYPES = ("WorkspaceScope", "ResourceWorkspace", "ExecutionWorkspace")

OPTIONAL_TENANCY_EXTRACTOR = re.compile(
    r"Option<\s*(?:[A-Za-z_]\w*\s*::\s*)*Extension<\s*(?:[A-Za-z_]\w*\s*::\s*)*"
    r"(" + "|".join(TENANCY_HANDLE_TYPES) + r")\s*>\s*>"
)

# key: "<path>::<handle type>" -> prose recording (a) the router that mounts the
# handler, (b) why that router provably cannot carry a tenancy edge, and (c) what
# the absent-scope behaviour is INSTEAD of a default.
OPTIONAL_TENANCY_EXEMPTIONS: dict[str, str] = {
    # Mounted by `awaken_config_service::config_router` — the same router whose
    # WorkspaceScope IS required. `ExecutionWorkspace` is not the tenant handle:
    # it is a second coordinate that only the reserved admin authoring scope
    # needs, so the trusted edge legitimately supplies it for some requests and
    # not others. Absence is already fail-closed, not defaulted:
    # `publication_workspace` returns `None` and the handler answers 400
    # "publication execution workspace is required" without touching the store.
    "crates/control/awaken-config-service/src/config_routes.rs::ExecutionWorkspace": (
        "secondary publication coordinate; absence answers 400, never a default"
    ),
}

# The one place the anonymous single-machine composition may name a tenant it
# did not receive from the edge. It stamps the *provisioned, persisted*
# installation Workspace, not a compiled-in constant, and only when no edge
# already resolved one.
TENANCY_COMPOSITION_ROOT = Path("crates/server/awaken-coordinator/src/workspace_path.rs")

AMBIENT_FALLBACK_CALL = re.compile(
    r"\b(?:unwrap_or|unwrap_or_else|unwrap_or_default|map_or|map_or_else)\b"
)

NAMED_TENANT_DEFAULT = re.compile(r"\bDEFAULT_(?:SCOPE|WORKSPACE_ID)\b")

LITERAL_TENANT_DEFAULT = re.compile(r'"default"')

TENANCY_VOCABULARY = re.compile(
    r"\b(?:WorkspaceScope|ResourceWorkspace|ExecutionWorkspace|workspace_id|owner_scope"
    r"|scope_id)\b"
)

AMBIENT_DEFAULT_WINDOW = 240

# key: "<path>::<enclosing fn>" -> prose. Same burden of proof as above.
AMBIENT_TENANT_DEFAULT_EXEMPTIONS: dict[str, str] = {}

def _is_rust_test_file(relative: Path) -> bool:
    """Whole-file test modules the production projection cannot blank."""
    name = relative.name
    return (
        name in {"tests.rs"}
        or name.endswith(("_tests.rs", "_test.rs"))
        or "tests" in relative.parts[:-1]
        or "test_support" in name
    )


def _enclosing_rust_fn(source: str, offset: int) -> str:
    """Name of the nearest `fn` declared before `offset`, for a stable key."""
    last = "<file>"
    for match in re.finditer(r"\bfn\s+([A-Za-z_]\w*)", source[:offset]):
        last = match.group(1)
    return last


def check_tenancy_is_not_optional(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES
) -> list[str]:
    """No ingress handler may take the tenant handle as an `Option`."""
    errors: list[str] = []
    for path in sorted(crates.glob("*/*/src/**/*.rs")):
        relative = path.relative_to(repo_root)
        if _is_rust_test_file(relative):
            continue
        production = _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        for match in OPTIONAL_TENANCY_EXTRACTOR.finditer(production):
            handle = match.group(1)
            if f"{relative}::{handle}" in OPTIONAL_TENANCY_EXEMPTIONS:
                continue
            line = production[: match.start()].count("\n") + 1
            errors.append(
                f"{relative}:{line}: `{match.group(0)}` makes the tenant handle "
                "optional, so this handler owns a `None` arm that will acquire a "
                "silent default. Take the required extractor instead (see "
                "`RequiredWorkspaceScope`), or register the site in "
                "OPTIONAL_TENANCY_EXEMPTIONS with the router that mounts it, why "
                "that router cannot carry a tenancy edge, and what absence does"
            )
    return errors


def check_no_ambient_tenant_default(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES
) -> list[str]:
    """Only the composition root may name a tenant the edge did not resolve."""
    errors: list[str] = []
    for path in sorted(crates.glob("*/*/src/**/*.rs")):
        relative = path.relative_to(repo_root)
        if relative == TENANCY_COMPOSITION_ROOT or _is_rust_test_file(relative):
            continue
        production = _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        for match in AMBIENT_FALLBACK_CALL.finditer(production):
            window = production[
                max(0, match.start() - AMBIENT_DEFAULT_WINDOW) : match.end()
                + AMBIENT_DEFAULT_WINDOW
            ]
            named = NAMED_TENANT_DEFAULT.search(window)
            literal = LITERAL_TENANT_DEFAULT.search(window) and TENANCY_VOCABULARY.search(
                window
            )
            if not (named or literal):
                continue
            owner = _enclosing_rust_fn(production, match.start())
            if f"{relative}::{owner}" in AMBIENT_TENANT_DEFAULT_EXEMPTIONS:
                continue
            line = production[: match.start()].count("\n") + 1
            errors.append(
                f"{relative}:{line}: `{match.group(0)}` in `{owner}` falls back to "
                "the default tenant. Ownership is resolved once, at the edge; a "
                "handler that invents it misattributes the write. Fail closed, or "
                "register the site in AMBIENT_TENANT_DEFAULT_EXEMPTIONS with the "
                "router that mounts it, why that router cannot carry a tenancy "
                "edge, and what absence does"
            )
    return errors


def selftest_tenancy_is_required() -> None:
    """Cause/effect: an optional handle rejects; required or registered admits."""
    # C1 a handler takes `Option<Extension<WorkspaceScope>>`; C2 the path
    # qualifier is spelled out (`Option<axum::Extension<awaken_tenancy::..>>`);
    # C3 the handler takes the bare `Extension<..>`; C4 the site is registered in
    # OPTIONAL_TENANCY_EXEMPTIONS; C5 the declaration is test-only; C6 the file
    # is a whole-file test module. Effects: E1 reject; E2 admit.
    #
    # | Rule | optional | qualified | registered | test-only | test file | Effect |
    # | T1   | T        | F         | F          | F         | F         | E1     |
    # | T2   | T        | T         | F          | F         | F         | E1     |
    # | T3   | F        | -         | F          | F         | F         | E2     |
    # | T4   | T        | F         | T          | F         | F         | E2     |
    # | T5   | T        | F         | F          | T         | F         | E2     |
    # | T6   | T        | F         | F          | F         | T         | E2     |
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        source = crates / "server/example/src/routes.rs"
        source.parent.mkdir(parents=True)

        source.write_text(
            "async fn handler(scope: Option<Extension<WorkspaceScope>>) {}\n",
            encoding="utf-8",
        )
        assert check_tenancy_is_not_optional(root, crates)  # T1
        source.write_text(
            "async fn handler(s: Option<axum::Extension<awaken_tenancy::WorkspaceScope>>) {}\n",
            encoding="utf-8",
        )
        assert check_tenancy_is_not_optional(root, crates)  # T2
        source.write_text(
            "async fn handler(RequiredWorkspaceScope(s): RequiredWorkspaceScope) {}\n",
            encoding="utf-8",
        )
        assert not check_tenancy_is_not_optional(root, crates)  # T3
        source.write_text(
            "async fn handler(scope: Option<Extension<WorkspaceScope>>) {}\n",
            encoding="utf-8",
        )
        key = f"{source.relative_to(root)}::WorkspaceScope"
        OPTIONAL_TENANCY_EXEMPTIONS[key] = "selftest"
        try:
            assert not check_tenancy_is_not_optional(root, crates)  # T4
        finally:
            del OPTIONAL_TENANCY_EXEMPTIONS[key]
        source.write_text(
            "#[cfg(test)] async fn f(s: Option<Extension<WorkspaceScope>>) {}\n",
            encoding="utf-8",
        )
        assert not check_tenancy_is_not_optional(root, crates)  # T5
        source.unlink()
        fixture = crates / "server/example/src/routes_tests.rs"
        fixture.write_text(
            "async fn handler(scope: Option<Extension<WorkspaceScope>>) {}\n",
            encoding="utf-8",
        )
        assert not check_tenancy_is_not_optional(root, crates)  # T6


def selftest_no_ambient_tenant_default() -> None:
    """Cause/effect: a defaulted tenant rejects; the composition root admits."""
    # C1 a fallback names DEFAULT_SCOPE/DEFAULT_WORKSPACE_ID; C2 it uses the bare
    # literal "default" beside tenancy vocabulary; C3 the fallback is unrelated
    # to tenancy; C4 the file is the composition root; C5 the site is registered.
    # Effects: E1 reject; E2 admit.
    #
    # | Rule | named | literal+vocab | unrelated | root | registered | Effect |
    # | A1   | T     | F             | F         | F    | F          | E1     |
    # | A2   | F     | T             | F         | F    | F          | E1     |
    # | A3   | F     | F             | T         | F    | F          | E2     |
    # | A4   | T     | F             | F         | T    | F          | E2     |
    # | A5   | T     | F             | F         | F    | T          | E2     |
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        source = crates / "server/example/src/handlers.rs"
        source.parent.mkdir(parents=True)

        source.write_text(
            "fn owner(s: Option<String>) -> String {\n"
            "    s.unwrap_or_else(|| DEFAULT_SCOPE.to_string())\n}\n",
            encoding="utf-8",
        )
        assert check_no_ambient_tenant_default(root, crates)  # A1
        source.write_text(
            "fn owner(workspace_id: Option<&str>) -> &str {\n"
            '    workspace_id.unwrap_or("default")\n}\n',
            encoding="utf-8",
        )
        assert check_no_ambient_tenant_default(root, crates)  # A2
        source.write_text(
            "fn retries(limit: Option<u32>) -> u32 { limit.unwrap_or(3) }\n",
            encoding="utf-8",
        )
        assert not check_no_ambient_tenant_default(root, crates)  # A3
        source.unlink()
        composition_root = root / TENANCY_COMPOSITION_ROOT
        composition_root.parent.mkdir(parents=True)
        composition_root.write_text(
            "fn stamp(s: Option<String>) -> String {\n"
            "    s.unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string())\n}\n",
            encoding="utf-8",
        )
        assert not check_no_ambient_tenant_default(root, crates)  # A4
        source.write_text(
            "fn owner(s: Option<String>) -> String {\n"
            "    s.unwrap_or_else(|| DEFAULT_SCOPE.to_string())\n}\n",
            encoding="utf-8",
        )
        key = f"{source.relative_to(root)}::owner"
        AMBIENT_TENANT_DEFAULT_EXEMPTIONS[key] = "selftest"
        try:
            assert not check_no_ambient_tenant_default(root, crates)  # A5
        finally:
            del AMBIENT_TENANT_DEFAULT_EXEMPTIONS[key]


def check_cloud_placement_boundary(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES
) -> list[str]:
    """Keep Cloud DataShard/Cell policy inside the one hosting ACL."""
    errors: list[str] = []
    for path in sorted(crates.glob("*/*/src/**/*.rs")):
        relative = path.relative_to(repo_root)
        if relative == CLOUD_HOSTING_ACL or path.name.endswith("_test.rs"):
            continue
        production = _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        leaked = sorted(token for token in CLOUD_PLACEMENT_TOKENS if token in production)
        if leaked:
            errors.append(
                f"{relative}: Cloud placement coordinate(s) {', '.join(leaked)} "
                "escape the hosted Workspace data anti-corruption adapter"
            )
    return errors


def selftest_cloud_placement_boundary() -> None:
    """Cause/effect: product leak rejects; sole hosting ACL admits foreign wire."""
    # C1 a Cloud coordinate appears outside the ACL; C2 it is provably
    # test-only; C3 it can compile through a non-test feature; C4 it appears in
    # the sole hosting ACL. Effects: E1 reject C1/C3; E2 ignore only C2; E3
    # admit C4. This reuses the canonical production-Rust projection so Cloud
    # ownership cannot drift from the other crate-boundary gates.
    #
    # | Rule | outside ACL | test-only | feature-capable | Effect |
    # | R1   | T           | F         | F               | E1     |
    # | R2   | T           | T         | F               | E2     |
    # | R3   | T           | F         | T               | E1     |
    # | R4   | F           | F         | F               | E3     |
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        leak = crates / "domain/example/src/lib.rs"
        leak.parent.mkdir(parents=True)
        leak.write_text("struct DomainLeak { data_shard_id: String }\n", encoding="utf-8")
        assert any(
            "data_shard_id" in error
            for error in check_cloud_placement_boundary(root, crates)
        )  # R1
        leak.write_text(
            "#[cfg(test)] struct Fixture { data_shard_id: String }\n",
            encoding="utf-8",
        )
        assert not check_cloud_placement_boundary(root, crates)  # R2
        leak.write_text(
            '#[cfg(any(test, feature = "support"))] '
            "struct FeatureWire { data_shard_id: String }\n",
            encoding="utf-8",
        )
        assert check_cloud_placement_boundary(root, crates)  # R3
        leak.unlink()
        adapter = root / CLOUD_HOSTING_ACL
        adapter.parent.mkdir(parents=True)
        adapter.write_text("struct ForeignWire { data_shard_id: String }\n", encoding="utf-8")
        assert not check_cloud_placement_boundary(root, crates)  # R4


def _opaque_backend_config_errors(rel: Path, content: str) -> list[str]:
    """Reject any authorization-deciding crate that names a backend config value.

    Cause/effect graph: C1=a retired/leaf permission-mode identifier appears in
    production Rust of a crate that decides authorization. E1=reject, because
    reading that value is the core interpreting backend configuration instead of
    relaying it. Comments and string-free prose are stripped before the scan by
    the shared production-Rust filter, so documentation that merely mentions a
    mode stays legal.
    """
    errors: list[str] = []
    for value in sorted(OPAQUE_BACKEND_CONFIG_VALUES):
        if re.search(rf'\b{re.escape(value)}\b', content):
            errors.append(
                f"{rel}: authorization crate names backend config value {value!r}; "
                "relay backend session modes and config options opaquely"
            )
    return errors


def check_backend_configuration_is_opaque() -> list[str]:
    """Keep leaf backend configuration out of the authorization vocabulary.

    Two independent gates: a dependency gate (an authorization crate cannot
    depend on an ACP crate, so the types are unreachable) and a source gate (it
    cannot spell a backend mode identifier, so a literal cannot leak in through
    a plain string either).
    """
    errors: list[str] = []
    specs = {spec.name: spec for spec in dependency_fitness_specs()}
    for crate_name in sorted(OPAQUE_BACKEND_CONFIG_CRATES):
        spec = specs.get(crate_name)
        if spec is None:
            errors.append(f"missing required authorization crate {crate_name!r}")
            continue
        for dependency in sorted(spec.normal_deps & FORBIDDEN_BACKEND_CONFIG_DEPS):
            errors.append(
                f"{crate_name}: depends on {dependency!r}; authorization must not "
                "reach backend protocol configuration"
            )
        for path in text_files(crate_name):
            rel = path.relative_to(REPO_ROOT)
            if _session_state_ownership_fitness._is_test_module(str(rel)):
                continue
            content = _crate_boundary_workspace.production_rust(
                path.read_text(encoding="utf-8")
            )
            errors.extend(_opaque_backend_config_errors(rel, content))
    return errors


def selftest_backend_configuration_is_opaque() -> None:
    # Decision table over the source gate. C1=a backend mode identifier appears
    # in executable Rust => E1 reject. C2=it appears only in a comment (stripped
    # by the shared production filter) => E2 accept. C3=no identifier => E2.
    rel = Path("crates/runtime/awaken-ext-permission/src/lib.rs")
    for value in OPAQUE_BACKEND_CONFIG_VALUES:
        assert _opaque_backend_config_errors(
            rel, f'let mode = "{value}";'
        ), f"S1/E1 {value}"
    assert not _opaque_backend_config_errors(
        rel,
        _crate_boundary_workspace.production_rust(
            "// acceptEdits and bypassPermissions are retired modes.\nlet x = 1;\n"
        ),
    ), "S1/E2 comments"
    assert not _opaque_backend_config_errors(rel, "let x = 1;"), "S1/E2 clean"
# L2 guard for the composite identity (see `awaken_runtime_contract::resolved::
# ModelBinding`). A struct with two or more bare `String` coordinates — fields
# whose names end in `_id`, `_ref`, `_key` or `_name` — is, by this
# repository's own vocabulary, a value with several identity coordinates. When
# such a struct also *derives* `PartialEq`, the derive silently answers a
# question the domain never asked: "are all of these fields part of this
# identity, and is every one of them required?" It answers "yes" to both, and
# the two halves of that answer fail in opposite directions.
#
# The producer half: `ModelBinding::new("", "m-first", "")` used to compile, so
# the empty string was a representable wildcard. Two candidates on *different*
# provider accounts then compared equal, a pinned model resolved onto the wrong
# account, and inference was billed and routed there.
#
# The consumer half: a derived `PartialEq` re-answers the question every time a
# field is added. A coordinate that joins the struct joins every comparison
# fence that value gates — a dedup set, an idempotency replay check, a
# conditional-write precondition — with no edit at the fence and no review.
#
# The rule is therefore: such a struct states its identity, rather than letting
# the derive state it. Stating it means either a hand-written
# `impl PartialEq for <Name>` (so the significant coordinates are written down)
# or an inherent `fn identity(&self)` projection that every comparison goes
# through — the shape `ManagementAuditRecord` and `ModelBinding` already use.
# A struct that is *not* an identity says so here instead.
#
# Whole-repo scan plus a registry, never a file allow-list: a list freezes
# today's instances and governs nothing written tomorrow.
COMPOSITE_IDENTITY_COORDINATE = re.compile(
    r"\n\s*pub (\w*(?:_ref|_id|_name|_key))\s*:\s*String\s*,"
)
COMPOSITE_IDENTITY_STRUCT = re.compile(
    r"#\[derive\([^)]*\bPartialEq\b[^)]*\)\][^\n]*\n(?:#\[[^\n]*\]\n)*pub struct (\w+) \{(.*?)\n\}",
    re.S,
)

# Key: the struct's name. Value: why its coordinates are not an identity that a
# production decision compares. Every entry is a triage result someone can
# argue with; a struct whose value *does* gate a comparison belongs in neither
# this registry nor the ratchet, but in the stated form.
#
# Two kinds are recorded here:
#   * plain records — carried, serialized, logged, persisted, but never
#     compared to decide anything in production;
#   * foreign-shaped values — mirrors of an external system's open structure,
#     where whole-value equality is unwinnable by construction because the
#     foreign system, not this repository, owns the field set.
NON_IDENTITY_COMPOSITES: dict[str, str] = {
    # Plain records.
    "AgentListRequest": "coordinates are emptiness-checked and forwarded to the listing port",
    "AgentMessageRequest": "destructured straight into SessionAgentMessageCommand, never compared",
    "BrokeredInferenceRequest": "serialized to the gateway; the key becomes an HTTP header only",
    "CheckpointObjectMetadata": "byte-custody metadata handed to the object store; no production impl compares it",
    "ControlSessionCreationInputs": "destructured field-by-field into SessionBaseline at creation",
    "CoordinatedRunCommand": "destructured on arrival; the id routes, it is never compared",
    "DreamPreparation": "copied into the durable job record and read back to build ids",
    "EventAdmissionReceipt": "a response-only proof; the idempotency decision happens earlier on the key",
    "ExecutableModelOption": "a sorted admin-API projection; selection happens in select_offering",
    "InferenceTriple": "resolution output rendered into a preview; select_offering owns the decision",
    "PlacementContext": "placement reads requirements and attributes; the two ids are carried for identification",
    "SessionExecutionEvidence": "a read-only wire projection of SessionExecutionObservation",
    "SessionExecutionObservation": "a read-only projection handed straight to wire projection",
    "SessionToolUseObservation": "its coordinates are only formatted into a public tool-event id",
    "SessionUserRunCommand": "forwarded into AdmitSessionRun, which owns the reservation fence",
    "WorkDispatchFailure": "a reconciliation report entry; consumers count and log it",
    # Foreign-shaped.
    "TaskArtifactUpdateEvent": "A2A v0.3 spec-owned streaming event; the foreign system owns the field set",
    "TaskStatusUpdateEvent": "A2A v0.3 spec-owned streaming event; the foreign system owns the field set",
}

# The shape predates the gate. Converting every remaining comparison witness at
# once would be a migration, not a fix, so the untriaged remainder is ratcheted:
# the count may fall and may not rise. That is what makes this gate refuse a
# *new* unstated composite identity today rather than only after the migration
# finishes. Lower this number as witnesses are converted or recorded above;
# never raise it.
UNSTATED_COMPOSITE_IDENTITY_BASELINE = 51


def _unstated_composite_identities(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES
) -> tuple[list[str], set[str]]:
    """Locate every derived-equality struct with two or more String coordinates."""
    found: list[str] = []
    registered: set[str] = set()
    for path in sorted(crates.glob("*/*/src/**/*.rs")):
        if "/tests/" in str(path) or path.name.endswith(("tests.rs", "test_support.rs")):
            continue
        relative = path.relative_to(repo_root)
        production = _crate_boundary_workspace.production_rust(
            path.read_text(encoding="utf-8", errors="ignore")
        )
        for match in COMPOSITE_IDENTITY_STRUCT.finditer(production):
            name, body = match.group(1), match.group(2)
            if len(COMPOSITE_IDENTITY_COORDINATE.findall(body)) < 2:
                continue
            if name in NON_IDENTITY_COMPOSITES:
                registered.add(name)
                continue
            found.append(f"{relative}: {name}")
    return found, registered


def check_composite_identity_is_stated(
    repo_root: Path = REPO_ROOT, crates: Path = CRATES, baseline: int | None = None
) -> list[str]:
    """A multi-coordinate value states its identity, or says it is not one."""
    baseline = UNSTATED_COMPOSITE_IDENTITY_BASELINE if baseline is None else baseline
    found, registered = _unstated_composite_identities(repo_root, crates)
    errors = [
        f"{name} is recorded in NON_IDENTITY_COMPOSITES but no longer derives "
        "PartialEq over two or more String coordinates; drop the stale entry"
        for name in sorted(set(NON_IDENTITY_COMPOSITES) - registered)
    ]
    if len(found) > baseline:
        errors.extend(
            f"{entry} derives PartialEq over two or more bare String coordinates, so the "
            "derive decides which of them are required and which are compared. Write the "
            "identity down — a hand-written `impl PartialEq` or an inherent `fn identity(&self)` "
            "every comparison goes through — or record in NON_IDENTITY_COMPOSITES why this "
            "value never gates a decision"
            for entry in found
        )
        errors.append(
            f"{len(found)} unstated composite identities, baseline {baseline}: "
            f"{len(found) - baseline} new one(s)."
        )
    elif len(found) < baseline:
        errors.append(
            f"{len(found)} unstated composite identities, below the baseline of {baseline}. "
            f"Lower UNSTATED_COMPOSITE_IDENTITY_BASELINE to {len(found)} so the ground gained "
            "is held."
        )
    return errors


def selftest_composite_identity_is_stated() -> None:
    """Cause/effect: only an unstated multi-coordinate identity raises the count."""
    # Causes: C1 two String coordinates with a derived PartialEq; C2 the derive
    # is replaced by a hand-written impl; C3 only one coordinate; C4 the
    # coordinates are not bare Strings; C5 no PartialEq at all; C6 the struct is
    # recorded as a non-identity; C7 the declaration is test-only; C8 a recorded
    # name no longer matches anything.
    # Effects: E1 counted toward the ratchet, E2 not counted, E3 an error.
    #
    # | Rule | derive PartialEq | >=2 String coords | registered | test-only | Effect |
    # | I1   | T                | T                 | F          | F         | E1     |
    # | I2   | F (hand-written) | T                 | F          | F         | E2     |
    # | I3   | T                | F (one)           | F          | F         | E2     |
    # | I4   | T                | F (typed)         | F          | F         | E2     |
    # | I5   | F (no eq)        | T                 | F          | F         | E2     |
    # | I6   | T                | T                 | T          | F         | E2     |
    # | I7   | T                | T                 | F          | T         | E2     |
    # | I8   | -                | -                 | stale      | -         | E3     |
    global NON_IDENTITY_COMPOSITES
    original = NON_IDENTITY_COMPOSITES
    two = (
        "#[derive(Debug, Clone, PartialEq, Eq)]\n"
        "pub struct Widget {\n"
        "    pub tenant_id: String,\n"
        "    pub widget_ref: String,\n"
        "}\n"
    )
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        scan = root / "crates"
        source = scan / "contract/example/src/lib.rs"
        source.parent.mkdir(parents=True)

        def count() -> int:
            return len(_unstated_composite_identities(root, scan)[0])

        NON_IDENTITY_COMPOSITES = {}
        source.write_text(two, encoding="utf-8")
        assert count() == 1, "I1"
        source.write_text(
            "pub struct Widget {\n"
            "    pub tenant_id: String,\n"
            "    pub widget_ref: String,\n"
            "}\n"
            "impl PartialEq for Widget {\n"
            "    fn eq(&self, other: &Self) -> bool { self.identity() == other.identity() }\n"
            "}\n",
            encoding="utf-8",
        )
        assert count() == 0, "I2"
        source.write_text(
            "#[derive(PartialEq)]\npub struct Widget {\n    pub tenant_id: String,\n}\n",
            encoding="utf-8",
        )
        assert count() == 0, "I3"
        source.write_text(
            "#[derive(PartialEq)]\n"
            "pub struct Widget {\n"
            "    pub tenant_id: TenantId,\n"
            "    pub widget_ref: WidgetRef,\n"
            "}\n",
            encoding="utf-8",
        )
        assert count() == 0, "I4"
        source.write_text(
            "#[derive(Debug, Clone)]\n"
            "pub struct Widget {\n"
            "    pub tenant_id: String,\n"
            "    pub widget_ref: String,\n"
            "}\n",
            encoding="utf-8",
        )
        assert count() == 0, "I5"
        source.write_text(two, encoding="utf-8")
        NON_IDENTITY_COMPOSITES = {"Widget": "a report row nothing compares"}
        assert count() == 0, "I6"
        assert not check_composite_identity_is_stated(root, scan, baseline=0), "I6 admits"
        NON_IDENTITY_COMPOSITES = {}
        source.write_text("#[cfg(test)]\n" + two, encoding="utf-8")
        assert count() == 0, "I7"
        NON_IDENTITY_COMPOSITES = {"Absent": "no longer declared anywhere"}
        assert check_composite_identity_is_stated(root, scan, baseline=0), "I8"
        # The ratchet itself: above the baseline is a new instance, below it is
        # ground gained that must be recorded.
        NON_IDENTITY_COMPOSITES = {}
        source.write_text(two, encoding="utf-8")
        assert check_composite_identity_is_stated(root, scan, baseline=0), "ratchet rises"
        assert not check_composite_identity_is_stated(root, scan, baseline=1), "ratchet holds"
        assert check_composite_identity_is_stated(root, scan, baseline=2), "ratchet falls"
    NON_IDENTITY_COMPOSITES = original


# The producer half of the same class, ratcheted the same way. A
# `ModelBinding` has one admission owner, `ModelBinding::parse`, which
# refuses the coordinates that make two bindings on different provider accounts
# compare equal. `ModelBinding::new` delegates to it and panics on refusal.
# The count may fall and may not rise, so a new producer must use the fallible
# entry even while the old fixture assertions are still being moved.
#
# The ingress and mutation surfaces are now closed by
# the compiler rather than by this count: the durable ingress decodes through
# `#[serde(try_from)]` (proved by
# `the_durable_ingress_refuses_every_under_specified_binding`), and a
# `ModelBinding` literal and its admitted coordinates are unwritable outside
# its module (four `compile_fail` doctests on the type). What this count still
# guards is the remaining panic-based producers
# themselves -- and a 2026-09-19 sweep of every one of them found no production
# call site whose coordinates come from runtime data: each is a compile-time
# literal in a fixture, a Kani proof, a doc example or a test, where a panic is
# the intended assertion. That is why they are being collapsed to one fixture
# door per module rather than deprecated: `#[deprecated]` would turn several
# hundred fixture literals into build failures for no reachable refusal.
UNVALIDATED_MODEL_BINDING_PRODUCER = re.compile(r"\bModelBinding::new\s*\(")
UNVALIDATED_MODEL_BINDING_BASELINE = 404
# The one place `ModelBinding::new` is the subject rather than a producer: the
# round-trip property asserts that every binding `new` builds also decodes,
# which is the invariant whose absence let a constructible binding stall a run
# at the wire. Rewriting it to `parse` would delete the property. The key is
# `<path>::<fn>`, so the exemption covers one function, not a file: a second
# producer added elsewhere in the same file is still counted.
UNVALIDATED_MODEL_BINDING_EXEMPTIONS: dict[str, str] = {
    "crates/contract/awaken-runtime-contract/src/resolved/model_binding.rs::constructible": (
        "the round-trip property observes whether `new` admits what `parse` admits; "
        "it must call the constructor under test"
    ),
    "crates/contract/awaken-runtime-contract/src/resolved/model_binding.rs"
    "::the_documented_shapes_are_admitted_or_refused_exactly": (
        "the named rows of the admission table are built through `new` to prove "
        "each admitted shape round-trips through the wire"
    ),
}
ENCLOSING_FN = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def _enclosing_fn(text: str, offset: int) -> str:
    """Name of the nearest `fn` declared before `offset`, or "" at file scope."""
    names = ENCLOSING_FN.findall(text, 0, offset)
    return names[-1] if names else ""


def check_model_binding_producers_are_ratcheted(repo_root: Path = REPO_ROOT) -> list[str]:
    """Panic-based `ModelBinding::new` producers may only shrink; parse is fallible."""
    sites: list[str] = []
    for root in (repo_root / "crates", repo_root / "e2e"):
        if not root.is_dir():
            continue
        for path in sorted(root.rglob("*.rs")):
            text = path.read_text(encoding="utf-8", errors="ignore")
            relative = path.relative_to(repo_root).as_posix()
            for match in UNVALIDATED_MODEL_BINDING_PRODUCER.finditer(text):
                key = f"{relative}::{_enclosing_fn(text, match.start())}"
                if key in UNVALIDATED_MODEL_BINDING_EXEMPTIONS:
                    continue
                line = text[: match.start()].count("\n") + 1
                sites.append(f"{relative}:{line}")
    if len(sites) > UNVALIDATED_MODEL_BINDING_BASELINE:
        return [
            f"{len(sites)} ModelBinding::new call sites, baseline "
            f"{UNVALIDATED_MODEL_BINDING_BASELINE}: "
            f"{len(sites) - UNVALIDATED_MODEL_BINDING_BASELINE} new producer(s) use "
            "panic-based admission. Use ModelBinding::parse, which returns "
            "Result<_, InvalidModelBinding>."
        ]
    if len(sites) < UNVALIDATED_MODEL_BINDING_BASELINE:
        return [
            f"{len(sites)} ModelBinding::new call sites, below the baseline of "
            f"{UNVALIDATED_MODEL_BINDING_BASELINE}. Lower "
            f"UNVALIDATED_MODEL_BINDING_BASELINE to {len(sites)} so the ground gained is held."
        ]
    return []


def selftest_model_binding_producers_are_ratcheted() -> None:
    """Cause/effect: a producer above the baseline rejects unless its fn is registered."""
    # C1 a `ModelBinding::new(` site exists beyond the baseline; C2 the site's
    # enclosing fn is registered; C3 another fn in the same file also calls
    # `new`. Effects: E1 reject (count rose); E2 admit.
    #
    # | Rule | site | registered | sibling fn | Effect |
    # | M1   | T    | F          | F          | E1     |
    # | M2   | T    | T          | F          | E2     |
    # | M3   | T    | T          | T          | E1     |
    global UNVALIDATED_MODEL_BINDING_BASELINE
    saved = UNVALIDATED_MODEL_BINDING_BASELINE
    UNVALIDATED_MODEL_BINDING_BASELINE = 0
    try:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/example/src/lib.rs"
            source.parent.mkdir(parents=True)
            source.write_text(
                "fn proves() { let _ = ModelBinding::new(a, b, c); }\n", encoding="utf-8"
            )
            assert check_model_binding_producers_are_ratcheted(root), "M1/E1"
            key = f"{source.relative_to(root).as_posix()}::proves"
            UNVALIDATED_MODEL_BINDING_EXEMPTIONS[key] = "selftest"
            try:
                assert not check_model_binding_producers_are_ratcheted(root), "M2/E2"
                source.write_text(
                    "fn proves() { let _ = ModelBinding::new(a, b, c); }\n"
                    "fn produces() { let _ = ModelBinding::new(a, b, c); }\n",
                    encoding="utf-8",
                )
                assert check_model_binding_producers_are_ratcheted(root), "M3/E1"
            finally:
                del UNVALIDATED_MODEL_BINDING_EXEMPTIONS[key]
    finally:
        UNVALIDATED_MODEL_BINDING_BASELINE = saved


def _sdk_tier_dependency_errors(
    crate: str, dependencies: dict[str, tuple[str, str]]
) -> list[str]:
    """Reject a runtime-tier dependency on a serving assembly.

    Cause/effect graph: C1=the dependency is itself in the runtime tier;
    C2=its layer is a port or a neutral value (`contract`/`domain`), whatever
    its context; C3=it is a reviewed neutral shared library. E1=accept C1|C2|C3;
    E2=reject anything else, because linking it would drag a serving process
    into the embeddable SDK.
    """
    errors: list[str] = []
    for dependency, (context, layer) in sorted(dependencies.items()):
        if context == SDK_CONTEXT:
            continue
        if layer in SDK_ALLOWED_DEPENDENCY_LAYERS:
            continue
        if dependency in SDK_ALLOWED_SHARED_LIBRARIES:
            continue
        errors.append(
            f"{crate}: depends on {dependency!r} ({context}/{layer}); the runtime "
            "tier must stay embeddable without a server, so depend on a port, a "
            "neutral value, or a reviewed shared library instead"
        )
    return errors


def check_sdk_tier_is_standalone() -> list[str]:
    """Keep `awaken-runtime` and its extensions linkable without a server."""
    specs = {spec.name: spec for spec in architecture_fitness_specs()}
    coordinates = {name: (spec.context, spec.layer) for name, spec in specs.items()}
    errors: list[str] = []
    for name, spec in sorted(specs.items()):
        if spec.context != SDK_CONTEXT:
            continue
        errors.extend(
            _sdk_tier_dependency_errors(
                name,
                {
                    dependency: coordinates[dependency]
                    for dependency in spec.normal_deps
                    if dependency in coordinates
                },
            )
        )
    return errors


def selftest_sdk_tier_is_standalone() -> None:
    # Decision table. S1=another runtime crate => accept. S2=a port or neutral
    # value in any context => accept. S3=a reviewed shared library => accept.
    # S4=a serving assembly, even in the same `shared` context as S2 => reject;
    # this is the case a context-only rule would have admitted.
    assert not _sdk_tier_dependency_errors(
        "awaken-runtime", {"awaken-ext-goal": ("runtime", "infrastructure")}
    ), "S1/E1"
    assert not _sdk_tier_dependency_errors(
        "awaken-runtime-contract",
        {
            "awaken-credential-contract": ("control", "contract"),
            "awaken-tool-pattern": ("shared", "domain"),
        },
    ), "S2/E1"
    assert not _sdk_tier_dependency_errors(
        "awaken-ext-mcp", {"awaken-mcp-wire": ("shared", "infrastructure")}
    ), "S3/E1"
    assert _sdk_tier_dependency_errors(
        "awaken-ext-permission", {"awaken-session-executor": ("shared", "infrastructure")}
    ), "S4/E2 a serving assembly is not admitted by its context"
    assert _sdk_tier_dependency_errors(
        "awaken-runtime", {"awaken-coordinator": ("coordinator", "application")}
    ), "S4/E2"


def main() -> int:
    selftest_cloud_placement_boundary()
    selftest_generation_witnesses()
    selftest_mcp_tool_identity_ownership()
    selftest_mcp_tool_identity_ownership_in_console()
    _crate_boundary_workspace.selftest()
    _crate_dependency_fitness.selftest()
    _arch_fitness.selftest()
    _coordinator_authority_fitness.selftest()
    _migration_fitness.selftest()
    _session_effect_fitness.selftest()
    _session_state_ownership_fitness.selftest()
    _service_data_ownership_fitness.selftest()
    _sqlite_scheduler_fitness.selftest()
    _execution_ownership_fitness.selftest()
    _managed_protocol_boundary.selftest()
    _provider_env_fitness.selftest()
    _realization_permit_fitness.selftest()
    _ownership_fence_fitness.selftest()
    _wire_closure_fitness.selftest()
    _cancel_safety_fitness.selftest()
    selftest_builtin_tool_ownership()
    selftest_no_direct_transport_dependency()
    selftest_tenancy_is_required()
    selftest_no_ambient_tenant_default()
    selftest_backend_configuration_is_opaque()
    selftest_composite_identity_is_stated()
    selftest_model_binding_producers_are_ratcheted()
    selftest_sdk_tier_is_standalone()
    errors = (
        check_no_direct_transport_dependency()
        +
        _crate_dependency_fitness.check_all(dependency_fitness_specs())
        + check_neutral_code_boundaries()
        + check_builtin_tool_ownership()
        + check_background_task_is_state_only()
        + check_backend_configuration_is_opaque()
        + check_sdk_tier_is_standalone()
        + check_tests_are_not_arch_owners()
        + check_cloud_placement_boundary()
        + check_generation_witnesses_are_complete()
        + check_mcp_tool_identity_is_owned()
        + check_tenancy_is_not_optional()
        + check_no_ambient_tenant_default()
        + check_composite_identity_is_stated()
        + check_model_binding_producers_are_ratcheted()
        + _resource_plane_fitness.check_all(REPO_ROOT, CRATES)
        + _runtime_secret_boundary.check_all(REPO_ROOT, CRATES)
        + _wire_closure_fitness.check_all(REPO_ROOT, CRATES)
        + _cancel_safety_fitness.check_all(REPO_ROOT, CRATES)
        + _provider_env_fitness.check_all(REPO_ROOT, CRATES)
        + _realization_permit_fitness.check_all(REPO_ROOT)
        + _ownership_fence_fitness.check_all(REPO_ROOT)
        + _arch_fitness.check_all(architecture_fitness_specs())
        + _coordinator_authority_fitness.check_all(REPO_ROOT, CRATES)
        + _migration_fitness.check_all(REPO_ROOT)
        + _session_effect_fitness.check_all(REPO_ROOT)
        + _session_state_ownership_fitness.check_all(REPO_ROOT)
        + _service_data_ownership_fitness.check_all(REPO_ROOT)
        + _sqlite_scheduler_fitness.check_all(REPO_ROOT)
        + _execution_ownership_fitness.check_all(REPO_ROOT)
        + _managed_protocol_boundary.check_managed_route_inventory(REPO_ROOT)
        + _managed_protocol_boundary.check_session_admission_ownership(REPO_ROOT)
        + _managed_protocol_boundary.check_projection_input_boundary(REPO_ROOT)
    )
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("OK - metadata-derived crate boundaries hold.")
    return 0


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_crate_boundaries.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("paths", nargs="*", help="ignored; boundaries are derived from Cargo metadata")
    return parser.parse_args(argv)


if __name__ == "__main__":
    parse(sys.argv[1:])
    raise SystemExit(main())
