"""Keep ambient provider configuration outside product execution paths.

Environment variables may feed a secret-free management proposal, but persisted
catalog/credential publication remains the only executable provider truth.
"""

from __future__ import annotations

import re
from pathlib import Path


PROVIDER_ENV_READ = re.compile(
    r"std::env::var(?:_os)?\(\s*\"(?:"
    r"ANTHROPIC_|OPENAI_|KIMI_|GEMINI_|MINIMAX_|"
    r"AWAKEN_MODEL_FALLBACKS|AWAKEN_ACP_GATEWAY_|"
    r"AWAKEN_ACP_LEASE_|AWAKEN_ACP_CREDENTIAL_FILE"
    r")"
)


def check_all(repo_root: Path, crates: Path) -> list[str]:
    """Reject direct provider-environment reads in shipped Rust code.

    The management proposal adapter uses an injected dynamic-key reader. Devtools
    and test targets remain explicit fixtures and are never product composition roots.
    """

    errors: list[str] = []
    for path in crates.glob("**/*.rs"):
        relative = path.relative_to(crates)
        if relative.parts[0] == "devtools" or "tests" in relative.parts:
            continue
        if PROVIDER_ENV_READ.search(path.read_text(encoding="utf-8")):
            errors.append(
                f"{path.relative_to(repo_root)}: provider execution configuration "
                "must come from persisted catalog/credential publication; environment "
                "variables are proposal inputs only"
            )
    return errors
