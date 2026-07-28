"""Keep ambient product configuration outside shipped execution paths."""

from __future__ import annotations

import re
from pathlib import Path


PROVIDER_ENV_READ = re.compile(
    r"std::env::var(?:_os)?\(\s*\"(?:"
    r"ANTHROPIC_|OPENAI_|KIMI_|GEMINI_|MINIMAX_|"
    r"AWAKEN_MODEL_FALLBACKS|AWAKEN_ACP_GATEWAY_|"
    r"AWAKEN_ACP_LEASE_|AWAKEN_ACP_CREDENTIAL_FILE"
    r"|PODMAN_BIN|AWAKEN_BASH|AWAKEN_SANDBOX_AGENT_STDERR"
    r"|AWAKEN_MEMORY_(?:STORE_ID|MOUNT_PATH|STORE_DIR|MODE|SHUTDOWN_)"
    r"|AWAKEN_LOG_FORMAT|AWAKEN_TRACE_FILE|RUST_LOG"
    r"|OTEL_EXPORTER_|OTEL_SERVICE_|OTEL_METRIC_EXPORT_INTERVAL"
    r")"
)

AMBIENT_SDK_CONSTRUCTOR = re.compile(
    r"(?:GenaiExecutor::(?:new|default)|genai::Client::default)\s*\("
)


def check_all(repo_root: Path, crates: Path) -> list[str]:
    """Reject direct provider-environment reads in shipped Rust code.

    Devtools and test targets remain explicit fixtures and are never product
    composition roots.
    """

    errors: list[str] = []
    for path in crates.glob("**/*.rs"):
        relative = path.relative_to(crates)
        if relative.parts[0] == "devtools" or "tests" in relative.parts:
            continue
        if PROVIDER_ENV_READ.search(path.read_text(encoding="utf-8")):
            errors.append(
                f"{path.relative_to(repo_root)}: provider execution configuration "
                "must come from persisted catalog/credential/deployment policy"
            )
        if AMBIENT_SDK_CONSTRUCTOR.search(path.read_text(encoding="utf-8")):
            errors.append(
                f"{path.relative_to(repo_root)}: ambient provider SDK defaults are "
                "forbidden; construct the adapter from published endpoint and credential facts"
            )
    return errors
