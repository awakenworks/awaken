"""Workspace discovery helpers for the crate-boundary fitness check."""

from __future__ import annotations

import re
import tomllib
from collections.abc import Iterable
from pathlib import Path

import _arch_fitness
import _crate_dependency_fitness


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


def normal_dependency_names(manifest: dict) -> set[str]:
    """Normal + build deps only; tests/examples are composition roots."""
    deps: set[str] = set()
    for section in ("dependencies", "build-dependencies"):
        deps.update(manifest.get(section, {}).keys())
    return deps


def public_first_party_reexports(sources: Iterable[str]) -> frozenset[str]:
    return frozenset(
        owner
        for source in sources
        for owner in re.findall(r"(?m)^\s*pub\s+use\s+(awaken_[A-Za-z0-9_]+)", source)
    )


def selftest() -> None:
    """Cause/effect table for first-party facade discovery.

    Causes: C1 crate-root source; C2 nested-module source; C3 private import.
    Effects: E1 public first-party owner is reported; E2 private use is ignored.

    | Rule | root public | nested public | private | effect |
    | R1   | yes         | no            | no      | E1     |
    | R2   | no          | yes           | no      | E1     |
    | R3   | no          | no            | yes     | E2     |
    """
    assert public_first_party_reexports(["pub use awaken_root::Thing;"]) == {
        "awaken_root"
    }, "R1"
    assert public_first_party_reexports(["", "pub use awaken_nested::{Thing};"]) == {
        "awaken_nested"
    }, "R2"
    assert public_first_party_reexports(["use awaken_private::Thing;"]) == set(), "R3"


def _awaken_metadata(manifest: dict) -> dict:
    return manifest.get("package", {}).get("metadata", {}).get("awaken", {})


def dependency_fitness_specs() -> list[_crate_dependency_fitness.CrateSpec]:
    """Build the one metadata-derived workspace dependency model."""
    specs: list[_crate_dependency_fitness.CrateSpec] = []
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        metadata = _awaken_metadata(manifest)
        specs.append(
            _crate_dependency_fitness.CrateSpec(
                name=package_name(manifest),
                context=str(metadata.get("context", "")),
                layer=str(metadata.get("layer", "")),
                authority=str(metadata.get("authority", "")),
                normal_deps=frozenset(normal_dependency_names(manifest)),
            )
        )
    return specs


def architecture_fitness_specs() -> list[_arch_fitness.CrateSpec]:
    """Build filesystem-free architecture-fitness specs for every workspace crate."""
    specs: list[_arch_fitness.CrateSpec] = []
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        awaken_metadata = _awaken_metadata(manifest)
        public_reexports = public_first_party_reexports(
            path.read_text(encoding="utf-8")
            for path in text_files(package_name(manifest))
        )
        specs.append(
            _arch_fitness.CrateSpec(
                name=package_name(manifest),
                normal_deps=frozenset(normal_dependency_names(manifest)),
                context=str(awaken_metadata.get("context", "")),
                layer=str(awaken_metadata.get("layer", "")),
                authority=str(awaken_metadata.get("authority", "")),
                public_first_party_reexports=public_reexports,
            )
        )
    return specs
