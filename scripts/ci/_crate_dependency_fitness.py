"""Dependency allowlist fitness rules shared by the crate-boundary entrypoint."""

from __future__ import annotations

from collections.abc import Callable, Iterable
from pathlib import Path


# Test targets may assemble an outer adapter to exercise an inward port without
# making that adapter a production dependency. Keeping these exceptions separate
# means moving one into `[dependencies]` still fails the production boundary.
DEV_ONLY_ALLOWED_DEPS = {
    "awaken-protocol-managed-resources": {
        "awaken-file-store",
        "awaken-resource-application",
        "awaken-resource-store",
    },
    "awaken-protocol-managed": {"awaken-admin-config-api"},
    "awaken-runtime-host": {
        "awaken-admin-config-api",
        "awaken-file-application",
    },
}


def check_allowed_dependencies(
    *,
    repo_root: Path,
    manifest_paths: Iterable[Path],
    load_manifest: Callable[[Path], dict],
    allowed_deps: dict[str, set[str]],
) -> list[str]:
    """Reject unknown crates and dependency edges outside the explicit policy."""
    errors: list[str] = []
    for manifest_path in manifest_paths:
        manifest = load_manifest(manifest_path)
        name = str(manifest["package"]["name"])
        allowed = allowed_deps.get(name)
        if allowed is None:
            errors.append(f"{manifest_path.relative_to(repo_root)}: unknown crate boundary")
            continue

        production = _dependency_names(manifest, "dependencies", "build-dependencies")
        dev = _dependency_names(manifest, "dev-dependencies")
        unexpected = production - allowed
        unexpected.update(dev - allowed - DEV_ONLY_ALLOWED_DEPS.get(name, set()))
        if unexpected:
            errors.append(
                f"{manifest_path.relative_to(repo_root)}: disallowed dependencies: "
                + ", ".join(sorted(unexpected))
            )
    return errors


def _dependency_names(manifest: dict, *sections: str) -> set[str]:
    deps: set[str] = set()
    for section in sections:
        deps.update(manifest.get(section, {}).keys())
    return deps
