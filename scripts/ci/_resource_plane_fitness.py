"""Resource-plane / authorization-plane architecture fitness checks.

The resource plane owns state and intrinsic consistency. IAM is a sibling
bounded context: a PEP authorizes at the application edge, then passes a trusted
Workspace coordinate plus a typed operation inward. This module inspects Cargo
dependencies, production Rust vocabulary, and resource SQL schemas so a future
allowlist edit cannot silently merge those planes.
"""

from __future__ import annotations

import re
import tomllib
from pathlib import Path

RESOURCE_PLANE_CRATES = {
    "awaken-resource-contract",
    "awaken-file-store",
    "awaken-memory-store",
    "awaken-skill-store",
    "awaken-resource-store",
    "awaken-resource-reclaimer",
}

# Resource application/adapters that live in a mixed-role crate. Their owning
# crate may have broader dependencies for sibling modules, so scan these files
# directly in addition to the dedicated resource crates above.
RESOURCE_APPLICATION_SOURCES = (
    "crates/control/awaken-admin-config-api/src/postgres_resource_catalog.rs",
    "crates/control/awaken-admin-config-api/src/sqlite_resource_catalog.rs",
    "crates/control/awaken-config-resolver/src/resource_catalog.rs",
    "crates/server/awaken-managed-routers/src/files.rs",
    "crates/server/awaken-protocol-managed/src/state/resource.rs",
    "crates/server/awaken-protocol-managed/src/state/resources.rs",
    "crates/server/awaken-runtime-host/src/memory_store_api.rs",
    "crates/server/awaken-runtime-host/src/memory_stores.rs",
    "crates/server/awaken-runtime-host/src/provisioning.rs",
    "crates/server/awaken-runtime-host/src/resource_lifecycle.rs",
    "crates/server/awaken-runtime-host/src/resource_reclamation.rs",
    "crates/server/awaken-runtime-host/src/resource_scope.rs",
    "crates/server/awaken-runtime-host/src/skill_catalog.rs",
    "crates/server/awaken-runtime-host/src/skills_api.rs",
)

# HTTP adapters are PEP consumers, not Workspace selectors. Every handler must
# extract the Workspace stamp installed by the outer composition edge; none may
# fall back to a Host-local tenant.
RESOURCE_HTTP_SOURCES = (
    "crates/server/awaken-managed-routers/src/files.rs",
    "crates/server/awaken-runtime-host/src/memory_store_api.rs",
    "crates/server/awaken-runtime-host/src/skills_api.rs",
)

# Recovery already reads a durably persisted Workspace envelope. Re-selecting a
# local/default scope here would turn missing routing state into cross-Workspace
# access instead of failing closed.
RESOURCE_RECOVERY_SOURCES = (
    "crates/server/awaken-protocol-managed/src/state/sessions.rs",
)

FORBIDDEN_DEPENDENCY_PREFIXES = ("awaken-authz", "awaken-iam")

FORBIDDEN_TYPE_NAMES = {
    "Principal",
    "ApiKey",
    "ApiToken",
    "Role",
    "RoleId",
    "ScopeRef",
    "AuthorizationDecision",
    "PermissionDecision",
    "OrganizationId",
    "OrgId",
    "ProjectId",
    "WorkUnitId",
}

# Resource policies (recall_policy/retention_policy) and external-service
# credential bindings are valid. Target only authorization-plane concepts.
FORBIDDEN_FIELD_NAMES = {
    "principal",
    "principal_id",
    "api_key",
    "api_token",
    "role",
    "role_id",
    "authorization_policy",
    "permission_policy",
    "pdp_decision",
    "authorization_decision",
    "permission_decision",
    "organization_id",
    "org_id",
    "project_id",
    "work_unit_id",
}


def _load_manifest(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def _dependency_package_names(manifest: dict) -> set[str]:
    """Return dependency keys and explicit targets, catching Cargo aliases."""

    names: set[str] = set()
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        for name, value in manifest.get(section, {}).items():
            names.add(name)
            if isinstance(value, dict) and isinstance(value.get("package"), str):
                names.add(value["package"])
    return names


def _without_cfg_test_module(content: str) -> str:
    """Return the production prefix before a conventional trailing test module."""

    marker = re.search(
        r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*\n\s*mod\s+tests\s*\{",
        content,
    )
    return content if marker is None else content[: marker.start()]


def _without_rust_comments_and_strings(content: str) -> str:
    content = re.sub(r"/\*.*?\*/", "", content, flags=re.DOTALL)
    content = re.sub(r"//[^\n]*", "", content)
    return re.sub(r'"(?:\\.|[^"\\])*"', '""', content)


def _rust_violations(content: str) -> list[str]:
    production = _without_cfg_test_module(content)
    code = _without_rust_comments_and_strings(production)
    violations: list[str] = []

    if re.search(r"\bawaken_(?:iam|authz)\b", code):
        violations.append("imports an IAM/authz module")

    for type_name in sorted(FORBIDDEN_TYPE_NAMES):
        if re.search(rf"\b{re.escape(type_name)}\b", code):
            violations.append(f"uses authorization-plane type {type_name!r}")

    uncommented = re.sub(r"/\*.*?\*/", "", production, flags=re.DOTALL)
    uncommented = re.sub(r"//[^\n]*", "", uncommented)
    if re.search(r"\b(?:DEFAULT|FIXED|HOST)_\w*WORKSPACE\w*\b", code):
        violations.append("declares or uses a fixed Workspace selector")
    if re.search(
        r'std\s*::\s*env\s*::\s*var\s*\(\s*"[A-Z0-9_]*WORKSPACE[A-Z0-9_]*"',
        uncommented,
    ):
        violations.append("selects a resource Workspace from process environment")
    for field_name in sorted(FORBIDDEN_FIELD_NAMES):
        field = re.escape(field_name)
        if re.search(rf"\b{field}\s*:", code) or re.search(rf'"{field}"', uncommented):
            violations.append(f"persists/carries authorization-plane field {field_name!r}")
    return violations


def selftest() -> None:
    allowed = "pub struct Config { pub workspace_id: String, pub recall_policy: RecallPolicy }"
    assert not _rust_violations(allowed)
    workspace_context = "fn open(workspace_id: &str, resource_id: &str) {}"
    assert not _rust_violations(workspace_context)
    denied = "pub struct Row { pub principal_id: String, pub decision: PermissionDecision }"
    violations = _rust_violations(denied)
    assert any("principal_id" in violation for violation in violations)
    assert any("PermissionDecision" in violation for violation in violations)
    test_only = '#[cfg(test)]\nmod tests { const FIELD: &str = "api_key"; }'
    assert not _rust_violations(test_only)
    fixed_workspace = 'const HOST_SKILL_WORKSPACE: &str = "workspace-a";'
    assert any("fixed Workspace" in item for item in _rust_violations(fixed_workspace))
    environment_workspace = (
        'fn scope() { let _ = std::env::var("HOST_SKILL_WORKSPACE"); }'
    )
    assert any(
        "process environment" in item
        for item in _rust_violations(environment_workspace)
    )


def check_all(repo_root: Path, crates: Path) -> list[str]:
    """Keep resource state/consistency orthogonal to authorization judgment.

    Workspace is intentionally permitted: it is the resource partition and
    ownership coordinate stamped by the PEP, not a grant or PDP decision.
    """

    selftest()
    errors: list[str] = []
    for crate_name in sorted(RESOURCE_PLANE_CRATES):
        manifest_path = next(crates.glob(f"*/{crate_name}/Cargo.toml"), None)
        if manifest_path is None:
            errors.append(f"missing resource-plane crate {crate_name!r}")
            continue

        manifest = _load_manifest(manifest_path)
        for dependency in sorted(_dependency_package_names(manifest)):
            if dependency.startswith(FORBIDDEN_DEPENDENCY_PREFIXES):
                errors.append(
                    f"{manifest_path.relative_to(repo_root)}: resource plane depends on "
                    f"authorization plane package {dependency!r}"
                )

        for path in sorted((manifest_path.parent / "src").rglob("*.rs")):
            for violation in _rust_violations(path.read_text(encoding="utf-8")):
                errors.append(f"{path.relative_to(repo_root)}: {violation}")

        for path in sorted(manifest_path.parent.rglob("*.sql")):
            sql = re.sub(r"--[^\n]*", "", path.read_text(encoding="utf-8"))
            for field_name in sorted(FORBIDDEN_FIELD_NAMES):
                if re.search(rf"\b{re.escape(field_name)}\b", sql, flags=re.IGNORECASE):
                    errors.append(
                        f"{path.relative_to(repo_root)}: resource schema persists "
                        f"authorization-plane field {field_name!r}"
                    )

    for relative in RESOURCE_APPLICATION_SOURCES:
        path = repo_root / relative
        if not path.is_file():
            errors.append(f"missing resource application source {relative!r}")
            continue
        for violation in _rust_violations(path.read_text(encoding="utf-8")):
            errors.append(f"{relative}: {violation}")

    for relative in RESOURCE_HTTP_SOURCES:
        path = repo_root / relative
        if not path.is_file():
            continue
        production = _without_cfg_test_module(path.read_text(encoding="utf-8"))
        code = _without_rust_comments_and_strings(production)
        if "RequiredWorkspaceScope" not in code:
            errors.append(
                f"{relative}: resource HTTP adapter does not require the edge-stamped "
                "Workspace scope"
            )
        if re.search(r"\blocal_workspace\s*\(", code):
            errors.append(
                f"{relative}: resource HTTP adapter falls back to the Host-local Workspace"
            )
    for relative in RESOURCE_RECOVERY_SOURCES:
        path = repo_root / relative
        if not path.is_file():
            errors.append(f"missing resource recovery source {relative!r}")
            continue
        production = _without_cfg_test_module(path.read_text(encoding="utf-8"))
        code = _without_rust_comments_and_strings(production)
        recovery = re.search(
            r"pub\s+async\s+fn\s+reconcile_resource_activations\b(?P<body>.*?)(?:\n\s*///|\n\s*pub(?:\([^)]*\))?\s+fn)",
            code,
            flags=re.DOTALL,
        )
        if recovery is None:
            errors.append(f"{relative}: resource recovery entry point is missing")
        elif "DEFAULT_SCOPE" in recovery.group("body"):
            errors.append(
                f"{relative}: resource recovery re-selects DEFAULT_SCOPE instead of "
                "consuming the durable Workspace envelope"
            )
    return errors
