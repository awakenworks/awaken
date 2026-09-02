#!/usr/bin/env python3
"""Enforce metadata-derived dependency direction and semantic ownership rules."""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path

import _arch_fitness
import _coordinator_authority_fitness
import _crate_dependency_fitness
import _execution_ownership_fitness
import _managed_protocol_boundary
import _migration_fitness
import _provider_env_fitness
import _resource_plane_fitness
import _runtime_secret_boundary
import _session_effect_fitness
import _session_state_ownership_fitness
import _service_data_ownership_fitness
import _sqlite_scheduler_fitness
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

NEUTRAL_CRATES = {"awaken-agent-contract", "awaken-runtime-contract", "awaken-runtime"}
EXTENSION_CRATES = {
    "awaken-ext-background-task",
    "awaken-ext-builtin-tools",
    "awaken-ext-permission",
}
FORBIDDEN_NEUTRAL_TERMS = {"managed"}
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
    neutral_rel = Path("crates/runtime/awaken-runtime-contract/src/example.rs")
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
    resource_modes = Path("crates/runtime/awaken-runtime-contract/src/tool.rs")
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


def main() -> int:
    selftest_cloud_placement_boundary()
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
    selftest_builtin_tool_ownership()
    errors = (
        _crate_dependency_fitness.check_all(dependency_fitness_specs())
        + check_neutral_code_boundaries()
        + check_builtin_tool_ownership()
        + check_background_task_is_state_only()
        + check_tests_are_not_arch_owners()
        + check_cloud_placement_boundary()
        + _resource_plane_fitness.check_all(REPO_ROOT, CRATES)
        + _runtime_secret_boundary.check_all(REPO_ROOT, CRATES)
        + _provider_env_fitness.check_all(REPO_ROOT, CRATES)
        + _arch_fitness.check_all(architecture_fitness_specs())
        + _coordinator_authority_fitness.check_all(REPO_ROOT, CRATES)
        + _migration_fitness.check_all(REPO_ROOT)
        + _session_effect_fitness.check_all(REPO_ROOT)
        + _session_state_ownership_fitness.check_all(REPO_ROOT)
        + _service_data_ownership_fitness.check_all(REPO_ROOT)
        + _sqlite_scheduler_fitness.check_all(REPO_ROOT)
        + _execution_ownership_fitness.check_all(REPO_ROOT)
        + _managed_protocol_boundary.check_managed_route_inventory(REPO_ROOT)
        + _managed_protocol_boundary.check_managed_application_boundary(REPO_ROOT)
        + _managed_protocol_boundary.check_session_admission_ownership(REPO_ROOT)
    )
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("OK - metadata-derived crate boundaries hold.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
