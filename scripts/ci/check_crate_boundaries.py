#!/usr/bin/env python3
"""Enforce Awaken crate dependency, vocabulary, and core/extension boundaries."""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CRATES = REPO_ROOT / "crates"

# Async runtime infrastructure (not domain or provider types) is permitted in
# neutral crates: async-trait makes the ports dyn-safe, tokio drives execution,
# tokio-util carries the cancellation token. A model/provider SDK such as genai
# is deliberately NOT in this set for any neutral crate; it lives only in the
# provider adapter so G2/G10 hold.
ALLOWED_DEPS: dict[str, set[str]] = {
    "awaken-agent-contract": {"serde", "serde_json", "thiserror", "async-trait", "tokio"},
    "awaken-runtime-contract": {
        "awaken-agent-contract",
        "serde",
        "serde_json",
        "thiserror",
        "async-trait",
        "tokio",
        "tokio-util",
    },
    "awaken-runtime": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "serde",
        "serde_json",
        "thiserror",
        "async-trait",
        "tokio",
        "tokio-util",
    },
    "awaken-ext-builtin-tools": {
        "awaken-runtime-contract",
        "serde",
        "serde_json",
    },
    # Provider adapter: the only crate allowed to name the model SDK. It also
    # consumes the SDK's async response stream, so `futures` (StreamExt) is
    # permitted here and nowhere else.
    "awaken-provider-genai": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "genai",
        "async-trait",
        "futures",
        "tokio",
        "serde",
        "serde_json",
        "thiserror",
    },
}

NEUTRAL_CRATES = {
    "awaken-agent-contract",
    "awaken-runtime-contract",
    "awaken-runtime",
}

EXTENSION_CRATES = {
    "awaken-ext-builtin-tools",
}

# Adapter crates may name an external SDK; they are not bound by the neutral
# vocabulary rules but still have an explicit dependency allowlist above.
ADAPTER_CRATES = {
    "awaken-provider-genai",
}

FORBIDDEN_NEUTRAL_TERMS = {
    "managed",
}

BUILTIN_TOOL_IDS = {
    "bash",
    "read",
    "write",
    "edit",
    "glob",
    "grep",
    "web_fetch",
    "web_search",
    "send_message",
    "cancel_task",
    "recover_failed_messages",
    "agent_run",
}

FORBIDDEN_NEUTRAL_TYPE_NAMES = {
    "TypedTool": "use Tool for the preferred typed API and RawTool for the low-level adapter",
    "BackgroundTask": "use ScheduledAction, RunWaitingState, or durable run dispatch by authority",
    # Removed roles: the executing side implements ToolExecutor; where a tool
    # runs is not a runtime role. (ExecutionBackend is NOT banned: it is the
    # local/A2A agent-execution seam and may live in the neutral contract.)
    "ToolExecutionLocus": "removed role; ToolExecutor is the sole neutral tool port",
    "ExecutorAdapter": "removed role; the executing side implements ToolExecutor, not a runtime adapter",
}

# Phrases that must not appear in neutral crate source (case-insensitive,
# whole-word). Execution placement is owned by the orchestration layer above this
# repository and never named by the runtime core.
FORBIDDEN_NEUTRAL_PHRASES = {
    "worker placement": "worker placement is owned by the orchestration layer above; runtime core never names it",
    "sandbox placement": "sandbox placement is owned by the orchestration layer above; runtime core never names it",
}

FORBIDDEN_NEUTRAL_SYMBOLS = {
    "AgentRun": "agent_run is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Bash": "bash is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "CancelTask": "cancel_task is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Edit": "edit is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Glob": "glob is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Grep": "grep is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Read": "read is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "RecoverFailedMessages": "recover_failed_messages is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "SendMessage": "send_message is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "WebFetch": "web_fetch is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "WebSearch": "web_search is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Write": "write is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "ConfigPublicationCoordinator": "config publication coordination stays outside runtime core",
    "RegistryCompiler": "registry compilation stays outside runtime core",
}

FORBIDDEN_NEUTRAL_IMPL_TRAITS = {
    "Tool": "neutral crates may define the Tool mechanism, but concrete implementations belong in extensions/adapters",
    "RawTool": "neutral crates may define the RawTool mechanism, but concrete implementations belong in extensions/adapters",
}


def load_manifest(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def package_name(manifest: dict) -> str:
    return str(manifest["package"]["name"])


def dependency_names(manifest: dict) -> set[str]:
    deps: set[str] = set()
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        deps.update(manifest.get(section, {}).keys())
    return deps


def iter_crate_manifests() -> list[Path]:
    if not CRATES.exists():
        return []
    return sorted(CRATES.glob("*/Cargo.toml"))


def check_dependencies() -> list[str]:
    errors: list[str] = []
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        name = package_name(manifest)
        allowed = ALLOWED_DEPS.get(name)
        if allowed is None:
            errors.append(f"{manifest_path.relative_to(REPO_ROOT)}: unknown crate boundary")
            continue

        unexpected = dependency_names(manifest) - allowed
        if unexpected:
            errors.append(
                f"{manifest_path.relative_to(REPO_ROOT)}: disallowed dependencies: "
                + ", ".join(sorted(unexpected))
            )
    return errors


def text_files(crate_name: str) -> list[Path]:
    src = CRATES / crate_name / "src"
    if not src.exists():
        return []
    return sorted(path for path in src.rglob("*.rs") if path.is_file())


def compile_word_patterns(words: set[str] | dict[str, str], *, ignore_case: bool = False) -> dict[str, re.Pattern[str]]:
    flags = re.IGNORECASE if ignore_case else 0
    return {
        word: re.compile(rf"\b{re.escape(word)}\b", flags)
        for word in words
    }


def check_neutral_code_boundaries() -> list[str]:
    errors: list[str] = []
    term_re = compile_word_patterns(FORBIDDEN_NEUTRAL_TERMS, ignore_case=True)
    tool_re = {
        tool_id: re.compile(rf'"{re.escape(tool_id)}"')
        for tool_id in BUILTIN_TOOL_IDS
    }
    type_re = compile_word_patterns(FORBIDDEN_NEUTRAL_TYPE_NAMES)
    symbol_re = compile_word_patterns(FORBIDDEN_NEUTRAL_SYMBOLS)
    phrase_re = compile_word_patterns(FORBIDDEN_NEUTRAL_PHRASES, ignore_case=True)
    impl_re = {
        trait_name: re.compile(rf"\bimpl(?:\s*<[^>]+>)?\s+(?:[\w:<>]+\s+for\s+)?{re.escape(trait_name)}\s+for\b")
        for trait_name in FORBIDDEN_NEUTRAL_IMPL_TRAITS
    }

    for crate_name in NEUTRAL_CRATES:
        for path in text_files(crate_name):
            content = path.read_text(encoding="utf-8")
            rel = path.relative_to(REPO_ROOT)
            for term, pattern in term_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: neutral crate uses product term {term!r}")
            for tool_id, pattern in tool_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: concrete builtin tool id {tool_id!r} leaked into neutral crate")
            for type_name, pattern in type_re.items():
                if pattern.search(content):
                    errors.append(
                        f"{rel}: forbidden neutral type name {type_name!r}; "
                        f"{FORBIDDEN_NEUTRAL_TYPE_NAMES[type_name]}"
                    )
            for symbol, pattern in symbol_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral symbol {symbol!r}; {FORBIDDEN_NEUTRAL_SYMBOLS[symbol]}")
            for phrase, pattern in phrase_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral phrase {phrase!r}; {FORBIDDEN_NEUTRAL_PHRASES[phrase]}")
            for trait_name, pattern in impl_re.items():
                if pattern.search(content):
                    errors.append(
                        f"{rel}: concrete {trait_name} implementation in neutral crate; "
                        f"{FORBIDDEN_NEUTRAL_IMPL_TRAITS[trait_name]}"
                    )
    return errors


def check_builtin_tool_ownership() -> list[str]:
    errors: list[str] = []
    for crate_name in EXTENSION_CRATES:
        crate_dir = CRATES / crate_name
        if not crate_dir.exists():
            errors.append(f"missing required extension crate {crate_name!r} for concrete builtin tool ids")
    return errors


def check_tests_are_not_arch_owners() -> list[str]:
    """Catch accidental arch-hook fixtures hidden outside the hook itself.

    Test crates may define fake tools, but production architecture ownership
    still lives in this hook and the design docs. This guard keeps future
    negative fixtures from being checked into neutral src paths by mistake.
    """

    errors: list[str] = []
    for crate_name in NEUTRAL_CRATES:
        tests = CRATES / crate_name / "tests"
        if not tests.exists():
            continue
        for path in sorted(tests.rglob("*.rs")):
            content = path.read_text(encoding="utf-8")
            if "TypedTool" in content:
                errors.append(
                    f"{path.relative_to(REPO_ROOT)}: tests should not normalize the forbidden TypedTool name"
                )
    return errors


def main() -> int:
    errors = (
        check_dependencies()
        + check_neutral_code_boundaries()
        + check_builtin_tool_ownership()
        + check_tests_are_not_arch_owners()
    )
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("OK - crate boundaries hold.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
