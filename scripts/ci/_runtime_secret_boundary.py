"""Keep runtime crates free of credential storage and resolution vocabulary."""

from __future__ import annotations

import re
from pathlib import Path


def check_all(repo_root: Path, crates: Path) -> list[str]:
    """Runtime receives resolved values and never names credential lifecycle ports."""
    banned = ("SecretHandle", "SecretResolver", "SecretStore", "CredentialBinding", "SecretRef")
    errors: list[str] = []
    runtime_dir = crates / "runtime"
    if not runtime_dir.exists():
        return errors
    for path in runtime_dir.glob("**/*.rs"):
        text = path.read_text(encoding="utf-8")
        for token in banned:
            if re.search(rf"\b{re.escape(token)}\b", text):
                errors.append(
                    f"{path.relative_to(repo_root)}: runtime must be secret-resolution-free "
                    f"(D6/D9): found `{token}` — resolution lives in the host, not the runtime"
                )
    return errors
