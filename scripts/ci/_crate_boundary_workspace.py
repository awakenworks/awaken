"""Workspace discovery helpers for the crate-boundary fitness check."""

from __future__ import annotations

import tomllib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CRATES = REPO_ROOT / "crates"


def load_manifest(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def package_name(manifest: dict) -> str:
    return str(manifest["package"]["name"])


def iter_crate_manifests() -> list[Path]:
    if not CRATES.exists():
        return []
    return sorted(CRATES.glob("*/*/Cargo.toml"))


def text_files(crate_name: str) -> list[Path]:
    manifest = next(CRATES.glob(f"*/{crate_name}/Cargo.toml"), None)
    if manifest is None:
        return []
    src = manifest.parent / "src"
    if not src.exists():
        return []
    return sorted(path for path in src.rglob("*.rs") if path.is_file())
