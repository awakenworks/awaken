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
    "agent_run", "list_agents", "send_to_agent",
}
RETIRED_BUILTIN_TOOL_IDS = {
    "move", "delete", "repository_inspect", "repository_commit",
    "send_message", "cancel_task", "recover_failed_messages",
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
    "AgentRun", "Bash", "Edit", "Glob", "Grep", "ListAgents", "Read",
    "SendToAgent", "WebFetch", "WebSearch", "Write",
}
RETIRED_BUILTIN_SYMBOLS = {
    "CancelTask", "DeleteTool", "MoveTool", "RecoverFailedMessages",
    "RepositoryCommitTool", "RepositoryInspectTool", "SendMessage",
}
FORBIDDEN_NEUTRAL_SYMBOLS = {
    **{name: f"{name} is a concrete active builtin owned by awaken-ext-builtin-tools"
       for name in ACTIVE_BUILTIN_SYMBOLS},
    **{name: f"{name} is a retired builtin and must not be reintroduced"
       for name in RETIRED_BUILTIN_SYMBOLS},
    "ConfigPublicationCoordinator": "configuration publication stays outside runtime core",
    "RegistryCompiler": "registry compilation stays outside runtime core",
}
NEUTRAL_SYMBOL_ALLOWLIST = {
    # Resource access modes are domain concurrency claims, not the builtin
    # filesystem Read/Write tool implementations.
    "Read": {"crates/runtime/awaken-runtime-contract/src/tool.rs"},
    "Write": {"crates/runtime/awaken-runtime-contract/src/tool.rs"},
}
FORBIDDEN_NEUTRAL_IMPL_TRAITS = {
    "Tool": "concrete tools belong in extensions/adapters",
    "RawTool": "concrete tools belong in extensions/adapters",
}


def _patterns(words: set[str] | dict[str, str], *, ignore_case: bool = False) -> dict[str, re.Pattern[str]]:
    flags = re.IGNORECASE if ignore_case else 0
    return {word: re.compile(rf"\b{re.escape(word)}\b", flags) for word in words}


def check_neutral_code_boundaries() -> list[str]:
    errors: list[str] = []
    terms = _patterns(FORBIDDEN_NEUTRAL_TERMS, ignore_case=True)
    tools = {tool: re.compile(rf'"{re.escape(tool)}"') for tool in BUILTIN_TOOL_IDS}
    types = _patterns(FORBIDDEN_NEUTRAL_TYPE_NAMES)
    symbols = _patterns(FORBIDDEN_NEUTRAL_SYMBOLS)
    phrases = _patterns(FORBIDDEN_NEUTRAL_PHRASES, ignore_case=True)
    impls = {
        trait: re.compile(rf"\bimpl(?:\s*<[^>]+>)?\s+(?:[\w:<>]+\s+for\s+)?{trait}\s+for\b")
        for trait in FORBIDDEN_NEUTRAL_IMPL_TRAITS
    }
    for crate_name in NEUTRAL_CRATES:
        for path in text_files(crate_name):
            rel = path.relative_to(REPO_ROOT)
            if _session_state_ownership_fitness._is_test_module(str(rel)):
                continue
            content = path.read_text(encoding="utf-8")
            for term, pattern in terms.items():
                if pattern.search(content):
                    errors.append(f"{rel}: neutral crate uses product term {term!r}")
            for tool, pattern in tools.items():
                if pattern.search(content):
                    errors.append(f"{rel}: concrete builtin tool id {tool!r} leaked into neutral code")
            for name, pattern in types.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral type {name!r}; {FORBIDDEN_NEUTRAL_TYPE_NAMES[name]}")
            for name, pattern in symbols.items():
                allowed = str(rel) in NEUTRAL_SYMBOL_ALLOWLIST.get(name, set())
                if pattern.search(content) and not allowed:
                    errors.append(f"{rel}: forbidden neutral symbol {name!r}; {FORBIDDEN_NEUTRAL_SYMBOLS[name]}")
            for phrase, pattern in phrases.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral phrase {phrase!r}; {FORBIDDEN_NEUTRAL_PHRASES[phrase]}")
            for trait, pattern in impls.items():
                if pattern.search(content):
                    errors.append(f"{rel}: concrete {trait} implementation in neutral code; {FORBIDDEN_NEUTRAL_IMPL_TRAITS[trait]}")
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
    # Cause/effect decision table: C1=safe Bash/protocol wording => E1 accepted;
    # C2=retired exact ID, C3=Git/repository ID, C4=retired tool type => E2 rejected.
    # Rules S1=C1=>E1; S2=C2|C3|C4=>E2. This tests the same helper used on source.
    rel = Path("crates/runtime/awaken-ext-builtin-tools/src/example.rs")
    assert not _forbidden_builtin_source_errors(
        rel,
        'const ID: &str = "bash"; async fn cancel_task() {}',
    ), "S1/E1"
    for forbidden in (
        'const ID: &str = "send_message";',
        'const ID: &str = "git_status";',
        'const ID: &str = "repository_apply";',
        "struct RepositoryInspectTool;",
    ):
        assert _forbidden_builtin_source_errors(rel, forbidden), f"S2/E2: {forbidden}"


def check_builtin_tool_ownership() -> list[str]:
    """Keep retired and Git-specific commands out of the builtin owner.

    Cause/effect decision table: C1=an exact retired id literal is present;
    C2=a Git/repository-specific id literal is present; C3=a retired concrete
    tool symbol is present. R1=C1|C2|C3 => fail before the command can enter the
    model catalog. The Rust catalog test independently fixes the complete active
    set; this static check catches the forbidden source even without compiling.
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
        content = path.read_text(encoding="utf-8")
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
        production = path.read_text(encoding="utf-8", errors="ignore").split(
            "#[cfg(test)]", 1
        )[0]
        leaked = sorted(token for token in CLOUD_PLACEMENT_TOKENS if token in production)
        if leaked:
            errors.append(
                f"{relative}: Cloud placement coordinate(s) {', '.join(leaked)} "
                "escape the hosted Workspace data anti-corruption adapter"
            )
    return errors


def selftest_cloud_placement_boundary() -> None:
    """Cause/effect: product leak rejects; sole hosting ACL admits foreign wire."""
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        leak = crates / "domain/example/src/lib.rs"
        leak.parent.mkdir(parents=True)
        leak.write_text("struct DomainLeak { data_shard_id: String }\n", encoding="utf-8")
        assert any(
            "data_shard_id" in error
            for error in check_cloud_placement_boundary(root, crates)
        )
        leak.unlink()
        adapter = root / CLOUD_HOSTING_ACL
        adapter.parent.mkdir(parents=True)
        adapter.write_text("struct ForeignWire { data_shard_id: String }\n", encoding="utf-8")
        assert not check_cloud_placement_boundary(root, crates)


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
