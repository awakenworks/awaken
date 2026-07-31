"""ADR-0071 service-data-ownership fitness rules.

Process co-location does not transfer data ownership.  In particular, the
production Worker is an authority-store-free executor: it may depend on neutral
contracts, the Runtime Host, and network adapters, but never on a concrete
Control, Coordinator, Credential, or Resource store implementation.
"""

from __future__ import annotations

import re
import tomllib
from pathlib import Path


WORKER_MANIFEST = "crates/bin/awaken-worker/Cargo.toml"
WORKER_SOURCE = "crates/bin/awaken-worker/src"
CONTROL_SOURCE = "crates/control/awaken-control/src"
CLI_SOURCE = "crates/bin/awaken-cli/src"

# Exact packages are used instead of broad words such as "resource" or
# "session": the Worker legitimately consumes the neutral contracts carrying
# those values.  These packages acquire durable authority or a database driver.
FORBIDDEN_WORKER_DEPENDENCIES = {
    "awaken-admin-config-api",
    "awaken-config-store",
    "awaken-credential-vault",
    "awaken-executable-agent-catalog",
    "awaken-file-store",
    "awaken-memory-store",
    "awaken-model-catalog",
    "awaken-resource-store",
    "awaken-session-store",
    "awaken-skill-store",
    "rusqlite",
    "sqlx",
}

# Constructors and key vocabulary are checked in production Worker source as a
# second line of defence.  A future transitive dependency therefore cannot be
# used to reopen authority state without tripping the fitness check.
FORBIDDEN_WORKER_SOURCE = re.compile(
    r"\b(?:Sqlite|Postgres)(?:ManagedSessionRepository|ResourceStore|FileStore|"
    r"MemoryRepository|SkillStore|CatalogRepo|CredentialRepo)\b|"
    r"\b(?:ControlStoreConfig|SealKeySource|SecretStore)\b|"
    r"\b(?:rusqlite|sqlx)\s*::",
)

# Deployment and Environment are one Managed Execution composition. Reopening
# either aggregate from the authoring service would recreate the former
# Control/Coordinator parallel state path.
FORBIDDEN_CONTROL_EXECUTION_SOURCE = re.compile(
    r"\b(?:DeploymentState|deployments_router|environments_router)\b"
)

# The retired private launch boundary must not return beside the local
# Coordinator application port.
FORBIDDEN_RETIRED_LAUNCH_SOURCE = re.compile(
    r"\b(?:HttpDeploymentSessionLauncher|DEPLOYMENT_SESSION_LAUNCH_PATH|"
    r"deployment_session_launch_router|DeploymentSessionLaunchConfig)\b"
)


def dependency_violations(dependencies: set[str]) -> list[str]:
    """Return the durable-authority packages accidentally linked by Worker."""

    return sorted(dependencies & FORBIDDEN_WORKER_DEPENDENCIES)


def source_violations(source: str) -> list[str]:
    """Return forbidden production authority constructors/vocabulary."""

    return sorted({match.group(0) for match in FORBIDDEN_WORKER_SOURCE.finditer(source)})


def control_execution_violations(source: str) -> list[str]:
    """Return Managed Execution owners accidentally reconstructed by Control."""

    return sorted(
        {match.group(0) for match in FORBIDDEN_CONTROL_EXECUTION_SOURCE.finditer(source)}
    )


def retired_launch_violations(source: str) -> list[str]:
    """Return vocabulary from the deleted remote Deployment launch path."""

    return sorted({match.group(0) for match in FORBIDDEN_RETIRED_LAUNCH_SOURCE.finditer(source)})


def _normal_dependencies(manifest: dict) -> set[str]:
    dependencies: set[str] = set()
    for section in ("dependencies", "build-dependencies"):
        for name, value in manifest.get(section, {}).items():
            dependencies.add(name)
            if isinstance(value, dict) and isinstance(value.get("package"), str):
                dependencies.add(value["package"])
    return dependencies


def selftest() -> None:
    """Cause/effect decision table.

    O1 neutral Worker ports/adapters -> accepted; O2 a direct authority-store
    dependency (including a Cargo alias) -> rejected; O3 an authority-store
    constructor reached through a transitive dependency -> rejected; O4 ordinary
    HTTP adapter construction -> accepted.  Together the rules cover both
    compile-time acquisition and production call-path acquisition.
    """

    assert dependency_violations({"awaken-runtime-host", "awaken-runtime-contract"}) == []  # O1
    assert dependency_violations({"awaken-session-store"}) == ["awaken-session-store"]  # O2
    assert source_violations("let store = PostgresMemoryRepository::connect(url).await?;")  # O3
    assert source_violations("let client = HttpMemoryRepository::new(url, token);") == []  # O4
    aliased = {"dependencies": {"session_backend": {"package": "awaken-session-store"}}}
    assert dependency_violations(_normal_dependencies(aliased)) == ["awaken-session-store"]  # O2
    assert control_execution_violations("let x = DeploymentState::new();") == [
        "DeploymentState"
    ]  # O5
    assert control_execution_violations("let x = ConfigPlane::new();") == []  # O6
    assert retired_launch_violations("HttpDeploymentSessionLauncher::new(url, token)") == [
        "HttpDeploymentSessionLauncher"
    ]  # O7


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    manifest_path = repo_root / WORKER_MANIFEST
    with manifest_path.open("rb") as handle:
        dependencies = _normal_dependencies(tomllib.load(handle))
    for package in dependency_violations(dependencies):
        errors.append(
            f"{WORKER_MANIFEST}: Worker links authority-store dependency `{package}`; "
            "use the existing claim-fenced boundary adapter"
        )

    source_root = repo_root / WORKER_SOURCE
    for path in sorted(source_root.rglob("*.rs")):
        source = path.read_text(encoding="utf-8")
        # Worker tests are inline and may use fakes, but the production portion
        # conventionally precedes the trailing cfg(test) module.
        source = re.split(r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*]", source, maxsplit=1)[0]
        for token in source_violations(source):
            errors.append(
                f"{path.relative_to(repo_root)}: Worker production code acquires "
                f"authority-store vocabulary `{token}`"
            )

    control_root = repo_root / CONTROL_SOURCE
    for path in sorted(control_root.rglob("*.rs")):
        source = path.read_text(encoding="utf-8")
        source = re.split(r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*]", source, maxsplit=1)[0]
        for token in control_execution_violations(source):
            errors.append(
                f"{path.relative_to(repo_root)}: Control reconstructs Coordinator-owned "
                f"Managed Execution vocabulary `{token}`"
            )

    cli_root = repo_root / CLI_SOURCE
    for path in sorted(cli_root.rglob("*.rs")):
        source = path.read_text(encoding="utf-8")
        source = re.split(r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*]", source, maxsplit=1)[0]
        for token in retired_launch_violations(source):
            errors.append(
                f"{path.relative_to(repo_root)}: retired remote Deployment launch "
                f"vocabulary `{token}` reappeared"
            )
    return errors
