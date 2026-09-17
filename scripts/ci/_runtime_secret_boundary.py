"""Keep runtime crates free of credential storage and resolution vocabulary."""

from __future__ import annotations

import re
import tomllib
from pathlib import Path


def _runtime_context_dirs(crates: Path) -> list[Path]:
    """crates/runtime plus the runtime-context contract crates that live under crates/contract."""
    dirs: list[Path] = []
    runtime_dir = crates / "runtime"
    if runtime_dir.exists():
        dirs.append(runtime_dir)
    for manifest in sorted((crates / "contract").glob("*/Cargo.toml")):
        package = tomllib.loads(manifest.read_text(encoding="utf-8")).get("package", {})
        if package.get("metadata", {}).get("awaken", {}).get("context") == "runtime":
            dirs.append(manifest.parent)
    return dirs


def check_all(repo_root: Path, crates: Path) -> list[str]:
    """Runtime receives resolved values and never names credential lifecycle ports."""
    banned = ("SecretHandle", "SecretResolver", "SecretStore", "CredentialBinding", "SecretRef")
    errors: list[str] = []
    for runtime_dir in _runtime_context_dirs(crates):
        for path in runtime_dir.glob("**/*.rs"):
            text = path.read_text(encoding="utf-8")
            for token in banned:
                if re.search(rf"\b{re.escape(token)}\b", text):
                    errors.append(
                        f"{path.relative_to(repo_root)}: runtime must be secret-resolution-free "
                        f"(D6/D9): found `{token}` — resolution lives in the host, not the runtime"
                    )
    return errors
