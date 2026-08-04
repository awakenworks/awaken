"""Workspace discovery helpers for the crate-boundary fitness check."""

from __future__ import annotations

import re
import tomllib
from pathlib import Path

import _arch_fitness


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


BUCKET_ALLOWED_DEPS = {
    "contract": {"contract"},
    "runtime": {"contract", "runtime"},
    "stores": {"contract", "runtime", "stores"},
    "resources": {"contract", "runtime", "resources"},
    # Resources are the shared foundation config defines and workers materialize.
    # Worker must still never link the durable commit-log store tier.
    "worker": {"contract", "runtime", "resources", "worker"},
    # Control assembles Managed wire and runtime-host ports without depending on
    # the coordinator binary; the bin composition root weaves both planes.
    "control": {"contract", "runtime", "stores", "resources", "control", "server"},
    "server": {"contract", "runtime", "stores", "resources", "worker", "control", "server"},
    "bin": {"contract", "runtime", "stores", "resources", "server", "worker", "control", "bin"},
    # Dev tooling is a test/example composition root and may name every plane.
    "devtools": {
        "contract", "runtime", "stores", "resources", "server", "worker",
        "control", "bin", "devtools",
    },
}


def check_bucket_direction() -> list[str]:
    """Check the product-bucket order over the discovered workspace graph."""
    errors: list[str] = []
    bucket: dict[str, str] = {}
    for manifest_path in iter_crate_manifests():
        name = package_name(load_manifest(manifest_path))
        bucket[name] = manifest_path.parent.parent.name
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        name = package_name(manifest)
        allowed = BUCKET_ALLOWED_DEPS.get(bucket.get(name, ""), set())
        for dep in normal_dependency_names(manifest):
            dep_bucket = bucket.get(dep)
            if dep_bucket is None or dep_bucket in allowed:
                continue
            errors.append(
                f"{bucket.get(name)}/{name} depends on {dep_bucket}/{dep} "
                f"(a {bucket.get(name)} crate may depend only on {sorted(allowed)})"
            )
    return errors


def architecture_fitness_specs() -> list[_arch_fitness.CrateSpec]:
    """Build filesystem-free architecture-fitness specs for every workspace crate."""
    specs: list[_arch_fitness.CrateSpec] = []
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        awaken_metadata = manifest.get("package", {}).get("metadata", {}).get("awaken", {})
        lib_path = manifest_path.parent / "src" / "lib.rs"
        lib_source = lib_path.read_text(encoding="utf-8") if lib_path.exists() else ""
        public_first_party_reexports = frozenset(
            re.findall(r"(?m)^\s*pub\s+use\s+(awaken_[A-Za-z0-9_]+)", lib_source)
        )
        specs.append(
            _arch_fitness.CrateSpec(
                name=package_name(manifest),
                normal_deps=frozenset(normal_dependency_names(manifest)),
                package_class=str(awaken_metadata.get("package-class", "")),
                authority=str(awaken_metadata.get("authority", "")),
                bucket=manifest_path.parent.parent.name,
                public_first_party_reexports=public_first_party_reexports,
            )
        )
    return specs
