"""ADR-0071 service-data-ownership fitness rules.

Process co-location does not transfer data ownership.  In particular, the
production Worker is an authority-store-free executor: it may depend on neutral
contracts, the Runtime Host, and network adapters, but never on a concrete
Control, Coordinator, Credential, or Resource store implementation.
"""

from __future__ import annotations

import re
import subprocess
import tomllib
from pathlib import Path


WORKER_MANIFEST = "crates/bin/awaken-worker/Cargo.toml"
WORKER_SOURCE = "crates/bin/awaken-worker/src"
CONTROL_SOURCE = "crates/control/awaken-control/src"
CLI_SOURCE = "crates/bin/awaken-cli/src"
CLI_LIB_SOURCE = "crates/bin/awaken-cli/src/lib.rs"
COORDINATOR_COMPONENT = "crates/server/awaken-coordinator/src/coordinator_component.rs"
SERVICE_BOUNDARY = "crates/bin/awaken-cli/src/config/service_boundary.rs"
RESOURCE_AUTHORITIES = "crates/resources/awaken-resource-application/src/authorities.rs"
RESOURCE_CONTRACT = "crates/contract/awaken-resource-contract/src/lib.rs"
RESOURCE_PERSISTENCE = "crates/resources/awaken-resource-persistence/src/lib.rs"
RUNTIME_HOST_BUILD = "crates/server/awaken-runtime-host/src/host/build.rs"
PROCESS_STORES = "crates/bin/awaken-cli/src/process_stores.rs"
RUNTIME_HOST_MANIFEST = "crates/server/awaken-runtime-host/Cargo.toml"
COORDINATOR_MANIFEST = "crates/server/awaken-coordinator/Cargo.toml"
PROTOCOL_MANAGED_MANIFEST = "crates/server/awaken-protocol-managed/Cargo.toml"
CLI_MANIFEST = "crates/bin/awaken-cli/Cargo.toml"
CLI_SERVICE = "crates/bin/awaken-cli/src/service.rs"
CONTROL_BIN = "crates/bin/awaken-cli/src/bin/awaken-control.rs"
COORDINATOR_BIN = "crates/bin/awaken-cli/src/bin/awaken-coordinator.rs"
FILE_STORE_SOURCE = "crates/resources/awaken-file-store/src/lib.rs"
COORDINATOR_SOURCE = "crates/server/awaken-coordinator/src/lib.rs"
MEMORY_STORE_SOURCE = "crates/resources/awaken-memory-store/src/repository.rs"
MEMORY_SQLITE_SOURCE = "crates/resources/awaken-memory-store/src/sqlite.rs"
SKILL_STORE_SOURCE = "crates/resources/awaken-skill-store/src/lib.rs"
SKILL_SQLITE_SOURCE = "crates/resources/awaken-skill-store/src/sqlite.rs"
RESOURCE_STORE_SOURCE = "crates/stores/awaken-resource-store/src/lib.rs"
RESOURCE_POSTGRES_REGISTRY = "crates/stores/awaken-resource-store/src/postgres_registry.rs"
RUNTIME_MEMORY_STORES = "crates/server/awaken-runtime-host/src/memory_stores.rs"
ENV_STORE_SQLITE_SOURCE = "crates/stores/awaken-env-store/src/lib.rs"
SANDBOX_POLICY_STORE_SOURCE = "crates/server/awaken-sandbox-policy-store/src/lib.rs"
ENV_IMAGE_BUILD_SOURCE = "crates/server/awaken-environment-image-build/src/lib.rs"
ENV_IMAGE_BUILD_SQLITE_SOURCE = "crates/server/awaken-environment-image-build/src/sqlite.rs"
SESSION_STORE_SQLITE_SOURCE = "crates/stores/awaken-session-store/src/sqlite.rs"
WORK_STORE_SOURCE = "crates/stores/awaken-work-store/src/lib.rs"
COMMIT_SQLITE_SOURCE = "crates/stores/awaken-store-sqlite/src/lib.rs"
FILE_SQLITE_SOURCE = "crates/resources/awaken-file-store/src/sqlite.rs"
WORKER_REGISTRY_SQLITE_SOURCE = "crates/server/awaken-worker-registry/src/sqlite.rs"
WORKER_REGISTRY_LIB_SOURCE = "crates/server/awaken-worker-registry/src/lib.rs"
DREAM_APPLICATION_SOURCE = "crates/server/awaken-dream-application/src/lib.rs"
TOOL_RELAY_SOURCE = "crates/worker/awaken-tool-relay/src/lib.rs"
RUN_INGRESS_ANY_SOURCE = "crates/server/awaken-run-ingress/src/any.rs"
RUN_INGRESS_LIB_SOURCE = "crates/server/awaken-run-ingress/src/lib.rs"
RUN_INGRESS_SQLITE_SOURCE = "crates/server/awaken-run-ingress/src/sqlite.rs"
RUNTIME_AUTHORITY_SOURCE = "crates/server/awaken-coordinator/src/runtime_authority.rs"
RUNTIME_STORE_SOURCE = "crates/server/awaken-runtime-host/src/store.rs"
RUNTIME_LIB_SOURCE = "crates/runtime/awaken-runtime/src/lib.rs"
STORE_FS_MANIFEST = "crates/stores/awaken-store-fs/Cargo.toml"
CAPTURE_STORE_SOURCE = "crates/stores/awaken-captured-content-store/src/lib.rs"
CAPTURE_SQLITE_SOURCE = "crates/stores/awaken-captured-content-store/src/sqlite.rs"
DATA_SUBJECT_SOURCE = "crates/stores/awaken-data-subject-store/src/memory.rs"
DATA_SUBJECT_SQLITE_SOURCE = "crates/stores/awaken-data-subject-store/src/sqlite.rs"
CONFIG_STORE_SQLITE_SOURCE = "crates/control/awaken-config-store/src/sqlite.rs"
ADMIN_CONFIG_SOURCE = "crates/control/awaken-admin-config-api/src/lib.rs"
ADMIN_CONFIG_SQLITE_SOURCE = "crates/control/awaken-admin-config-api/src/sqlite.rs"
MODEL_CATALOG_REPO_SOURCE = "crates/control/awaken-model-catalog/src/repo.rs"
MODEL_CATALOG_SQLITE_SOURCE = "crates/stores/awaken-model-catalog-store/src/sqlite.rs"
CONFIG_RESOLVER_SOURCE = "crates/control/awaken-config-resolver/src/lib.rs"
CONFIG_RESOLVER_STORES_SOURCE = "crates/control/awaken-config-resolver/src/stores.rs"
CREDENTIAL_REPO_SOURCE = "crates/control/awaken-credential-vault/src/repo.rs"
CREDENTIAL_VAULT_SOURCE = "crates/control/awaken-credential-vault/src/lib.rs"
CREDENTIAL_SQLITE_SOURCE = "crates/stores/awaken-credential-store/src/sqlite.rs"
CREDENTIAL_SEALED_SOURCE = "crates/stores/awaken-credential-store/src/sealed.rs"
WEBHOOK_DISPATCH_SOURCE = "crates/server/awaken-webhook/src/dispatch.rs"
WEBHOOK_MANAGED_SOURCE = "crates/server/awaken-webhook-managed/src/lib.rs"
WEBHOOK_CONTROL_PLANE_SOURCE = (
    "crates/server/awaken-webhook-managed/src/control_plane.rs"
)
EXECUTABLE_AGENT_CONTRACT_SOURCE = (
    "crates/server/awaken-executable-agent-contract/src/lib.rs"
)
PROTOCOL_MODEL_SOURCE = "crates/server/awaken-protocol-managed/src/control/models.rs"
RUNTIME_PROCESS_ROUTER = "crates/bin/awaken-cli/src/runtime_process_router.rs"
CREDENTIAL_INFERENCE_SOURCE = (
    "crates/server/awaken-credential-materializer/src/inference.rs"
)
CONTROL_MODEL_PUBLICATION = "crates/control/awaken-control/src/model_publication.rs"
CREDENTIAL_REFRESH_SOURCES = (
    "crates/server/awaken-credential-materializer/src/lib.rs",
    "crates/server/awaken-credential-materializer/src/oauth_refresh.rs",
)

# Exact packages are used instead of broad words such as "resource" or
# "session": the Worker legitimately consumes the neutral contracts carrying
# those values.  These packages acquire durable authority or a database driver.
FORBIDDEN_WORKER_DEPENDENCIES = {
    "awaken-admin-config-api",
    "awaken-config-store",
    "awaken-credential-store",
    "awaken-credential-vault",
    "awaken-data-subject-application",
    "awaken-data-subject-store",
    "awaken-executable-agent-catalog",
    "awaken-file-store",
    "awaken-memory-store",
    "awaken-model-catalog",
    "awaken-model-catalog-store",
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

# Deployment and Environment are one Managed Execution authority. Reopening
# either aggregate from the authoring service would recreate the former
# Control/Coordinator parallel state path.
FORBIDDEN_CONTROL_EXECUTION_SOURCE = re.compile(
    r"\b(?:DeploymentApplication|DeploymentState|deployments_router|environments_router)\b"
)

# The retired private launch boundary must not return beside the local
# Coordinator application contract.
FORBIDDEN_RETIRED_LAUNCH_SOURCE = re.compile(
    r"\b(?:HttpDeploymentSessionLauncher|DEPLOYMENT_SESSION_LAUNCH_PATH|"
    r"deployment_session_launch_router|DeploymentSessionLaunchConfig)\b"
)

REDUNDANT_ADMIN_STORE_REEXPORT = re.compile(
    r"\bpub\s+use\s+awaken_config_resolver::(?:"
    r"\{[^}]*\b(?:AgentInputBindingRepository|InferenceProfileStore|WebhookStore|"
    r"InMemoryAgentInputBindingRepository|InMemoryProfileStore|InMemoryWebhookStore)\b[^}]*\}|"
    r"(?:AgentInputBindingRepository|InferenceProfileStore|WebhookStore|"
    r"InMemoryAgentInputBindingRepository|InMemoryProfileStore|InMemoryWebhookStore)\b)",
    re.DOTALL,
)

VOLATILE_RUNTIME_HOST_APIS = (
    "new",
    "new_with_deployment",
    "with_deployment_config",
    "new_with_resources",
    "with_resource_reclamation",
    "with_skill_store",
    "with_skill_store_backend",
    "with_store_dir",
    "with_file_content_source",
    "with_memory_repository",
)

TEST_SUPPORT_GATE = (
    r'#\s*\[\s*cfg\s*\(\s*any\s*\(\s*test\s*,\s*feature\s*=\s*"test-support"\s*\)\s*\)\s*\]'
)
FEATURE_TEST_SUPPORT_GATE = (
    r'#\s*\[\s*cfg\s*\(\s*feature\s*=\s*"test-support"\s*\)\s*\]'
)
NON_PRODUCT_APIS = (
    (
        "awaken_cli::exact_host_model",
        CLI_LIB_SOURCE,
        r"\bmod\s+exact_host_model\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "awaken_cli::local_process_stores",
        CLI_LIB_SOURCE,
        r"\bmod\s+local_process_stores\b",
        TEST_SUPPORT_GATE,
    ),
    *(
        (
            f"awaken_cli::{name}",
            CLI_LIB_SOURCE,
            rf"\bpub\s+async\s+fn\s+{name}\b",
            TEST_SUPPORT_GATE,
        )
        for name in (
            "build_ephemeral_all_in_one_router",
            "build_all_in_one_router_with_scenario_model",
            "build_all_in_one_router_with_host_customizer",
            "build_durable_all_in_one_router_with_host_customizer",
            "build_all_in_one_router_with_model",
            "build_durable_all_in_one_router",
            "build_secured_all_in_one_router",
        )
    ),
    (
        "InMemoryFileStore",
        FILE_STORE_SOURCE,
        r"\bpub\s+struct\s+InMemoryFileStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "VolatileMemoryRepository",
        MEMORY_STORE_SOURCE,
        r"\bpub\s+struct\s+VolatileMemoryRepository\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteMemoryRepository::open_in_memory",
        MEMORY_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemorySkillStore",
        SKILL_STORE_SOURCE,
        r"\bpub\s+struct\s+InMemorySkillStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteSkillStore::open_in_memory",
        SKILL_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteResourceStore::in_memory",
        RESOURCE_STORE_SOURCE,
        r"\bpub\s+fn\s+in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "MemoryStores::open",
        RUNTIME_MEMORY_STORES,
        r"\bpub\s*\(\s*crate\s*\)\s+fn\s+open\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "awaken_resource_persistence::ephemeral",
        RESOURCE_PERSISTENCE,
        r"\bpub\s+fn\s+ephemeral\b",
        FEATURE_TEST_SUPPORT_GATE,
    ),
    (
        "awaken_coordinator::test_worker_directory",
        COORDINATOR_SOURCE,
        r"\bpub\s+use\s+worker_registry::test_directory\s+as\s+test_worker_directory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryEnvRegistry",
        ENV_STORE_SQLITE_SOURCE,
        r"\bpub\s+use\s+inmem::InMemoryEnvRegistry\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteEnvRegistry::open_in_memory",
        ENV_STORE_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemorySandboxExecutionPolicyStore",
        SANDBOX_POLICY_STORE_SOURCE,
        r"\bpub\s+struct\s+InMemorySandboxExecutionPolicyStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryEnvironmentImageBuildStore",
        ENV_IMAGE_BUILD_SOURCE,
        r"\bpub\s+use\s+in_memory::InMemoryEnvironmentImageBuildStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "open_in_memory_environment_image_build_store",
        ENV_IMAGE_BUILD_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory_environment_image_build_store\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteManagedSessionRepository::open_in_memory",
        SESSION_STORE_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryWorkQueue",
        WORK_STORE_SOURCE,
        r"\bpub\s+use\s+inmem::InMemoryWorkQueue\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteWorkQueue::open_in_memory",
        WORK_STORE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteCommitCoordinator::open_in_memory",
        COMMIT_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteFileStore::open_in_memory",
        FILE_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteWorkerDirectory::open_in_memory",
        WORKER_REGISTRY_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "awaken_worker_registry::memory",
        WORKER_REGISTRY_LIB_SOURCE,
        r"\bmod\s+memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "MemoryWorkerDirectory",
        WORKER_REGISTRY_LIB_SOURCE,
        r"\bpub\s+use\s+memory::MemoryWorkerDirectory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryDreamProcessStore",
        DREAM_APPLICATION_SOURCE,
        r"\bpub\s+struct\s+InMemoryDreamProcessStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryOperationLedger",
        TOOL_RELAY_SOURCE,
        r"\bpub\s+use\s+ledger::InMemoryOperationLedger\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "AnyDispatchStore::open_sqlite_in_memory",
        RUN_INGRESS_ANY_SOURCE,
        r"\bpub\s+fn\s+open_sqlite_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "awaken_run_ingress::memory",
        RUN_INGRESS_LIB_SOURCE,
        r"\bpub\s+mod\s+memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "MemoryDispatchStore",
        RUN_INGRESS_LIB_SOURCE,
        r"\bpub\s+use\s+memory::MemoryDispatchStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteDispatchStore::open_in_memory",
        RUN_INGRESS_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryCapturedContentStore",
        CAPTURE_STORE_SOURCE,
        r"\bpub\s+use\s+capture_store::\{CapturedRecord,\s*InMemoryCapturedContentStore\}",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteCapturedContentStore::open_in_memory",
        CAPTURE_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryDataSubjectRepo",
        DATA_SUBJECT_SOURCE,
        r"\bpub\s+struct\s+InMemoryDataSubjectRepo\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteDataSubjectRepo::open_in_memory",
        DATA_SUBJECT_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteConfigStore::open_in_memory",
        CONFIG_STORE_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteAdminStore::open_in_memory",
        ADMIN_CONFIG_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryCatalogRepo",
        MODEL_CATALOG_REPO_SOURCE,
        r"\bpub\s+struct\s+InMemoryCatalogRepo\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteCatalogRepo::open_in_memory",
        MODEL_CATALOG_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "Config Resolver in-memory reference stores",
        CONFIG_RESOLVER_SOURCE,
        r"\bpub\s+use\s+reference_stores::\{\s*"
        r"InMemoryAgentInputBindingRepository,\s*InMemoryProfileStore,\s*"
        r"InMemoryWebhookStore,?\s*\}",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryCredentialRepo",
        CREDENTIAL_REPO_SOURCE,
        r"\bpub\s+struct\s+InMemoryCredentialRepo\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "ReqwestSender::Default permissive transport",
        WEBHOOK_DISPATCH_SOURCE,
        r"\bimpl\s+Default\s+for\s+ReqwestSender\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "ReqwestSender::with_timeout permissive transport",
        WEBHOOK_DISPATCH_SOURCE,
        r"\bpub\s+fn\s+with_timeout\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "webhook loopback_lifecycle_delivery",
        WEBHOOK_CONTROL_PLANE_SOURCE,
        r"\bpub\s+fn\s+loopback_lifecycle_delivery\b",
        FEATURE_TEST_SUPPORT_GATE,
    ),
    (
        "webhook webhook_config_router_loopback",
        WEBHOOK_CONTROL_PLANE_SOURCE,
        r"\bpub\s+fn\s+webhook_config_router_loopback\b",
        FEATURE_TEST_SUPPORT_GATE,
    ),
    (
        "InMemorySealedBlobStore",
        CREDENTIAL_VAULT_SOURCE,
        r"\bpub\s+struct\s+InMemorySealedBlobStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemorySecretStore",
        CREDENTIAL_VAULT_SOURCE,
        r"\bpub\s+struct\s+InMemorySecretStore\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SQLite credential open_in_memory methods",
        CREDENTIAL_SQLITE_SOURCE,
        r"\bpub\s+fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SealedAeadSecretStore::with_key",
        CREDENTIAL_SEALED_SOURCE,
        r"\bpub\s+fn\s+with_key\b",
        TEST_SUPPORT_GATE,
    ),
)

def dependency_violations(dependencies: set[str]) -> list[str]:
    """Return the durable-authority packages accidentally linked by Worker."""

    return sorted(dependencies & FORBIDDEN_WORKER_DEPENDENCIES)


def dependency_closure(root: str, graph: dict[str, set[str]]) -> set[str]:
    """Return every normal dependency reachable from one deployable package."""

    reached: set[str] = set()
    pending = list(graph.get(root, ()))
    while pending:
        package = pending.pop()
        if package in reached:
            continue
        reached.add(package)
        pending.extend(graph.get(package, ()))
    return reached


def resolved_product_dependencies(
    repo_root: Path, package: str, boundary: str
) -> set[str]:
    """Read Cargo's feature-resolved normal closure for one product boundary."""
    result = subprocess.run(
        [
            "cargo",
            "tree",
            "-p",
            package,
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ],
        cwd=repo_root,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"resolve {boundary} dependency closure: {result.stderr.strip()}"
        )
    return {
        match.group(1)
        for line in result.stdout.splitlines()
        if (match := re.match(r"^([A-Za-z0-9_-]+)\s+v", line))
    }
def runtime_host_feature_violations(manifest: dict) -> list[str]:
    """Keep authority acquisition outside the reusable Runtime Host."""

    features = manifest.get("features", {})
    errors: list[str] = []
    if features.get("default", []) != []:
        errors.append("Runtime Host default features must be authority-free")
    if "coordinator" in features:
        errors.append("Runtime Host must not retain a Coordinator compatibility feature")
    return errors


def service_binary_violations(
    manifest: dict,
    service_source: str,
    control_source: str,
    coordinator_source: str,
) -> list[str]:
    """Keep one lifecycle implementation behind the role-named executables."""

    errors: list[str] = []
    bins = {
        entry.get("name"): entry.get("path")
        for entry in manifest.get("bin", [])
        if isinstance(entry, dict)
    }
    expected = {
        "awaken": "src/main.rs",
        "awaken-control": "src/bin/awaken-control.rs",
        "awaken-coordinator": "src/bin/awaken-coordinator.rs",
    }
    for name, path in expected.items():
        if bins.get(name) != path:
            errors.append(f"missing canonical `{name}` executable at `{path}`")
    if manifest.get("package", {}).get("default-run") != "awaken":
        errors.append("aggregate operator launcher must remain Cargo's default executable")
    if "awaken-worker" in bins:
        errors.append("aggregate CLI package must not recreate the Worker executable")
    if service_source.count("pub async fn run_service(") != 1:
        errors.append("CLI must own exactly one shared service lifecycle")
    if service_source.count("async fn migrate_service_for_role(") != 1:
        errors.append("role executables must share one role-fenced migration lifecycle")
    for name, source in (
        ("awaken-control", control_source),
        ("awaken-coordinator", coordinator_source),
    ):
        if "run_service_binary(" not in source:
            errors.append(f"`{name}` bypasses the shared service lifecycle")
        if "build_" in source:
            errors.append(f"`{name}` reconstructs applications inside its thin entrypoint")
    return errors


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


def _declaration_is_gated(
    source: str, declaration_pattern: str, gate_pattern: str
) -> bool | None:
    """Whether the declaration exists and its own attribute prefix has the gate."""

    declarations = list(re.finditer(declaration_pattern, source))
    if not declarations:
        return None
    return all(
        re.search(
            rf"{gate_pattern}(?:\s*(?:#\s*\[[^]]*\]|//[^\n]*))*\s*$",
            source[: declaration.start()],
        )
        is not None
        for declaration in declarations
    )


def volatile_runtime_host_surface_violations(source: str) -> list[str]:
    """Return volatile Host APIs that are reachable without test-support."""

    errors: list[str] = []
    for name in VOLATILE_RUNTIME_HOST_APIS:
        gated = _declaration_is_gated(
            source, rf"\bpub\s+fn\s+{re.escape(name)}\b", TEST_SUPPORT_GATE
        )
        if gated is None:
            errors.append(f"missing volatile Host API `{name}`")
            continue
        if not gated:
            errors.append(f"volatile Host API `{name}` is not test-support gated")
    return errors


def non_product_surface_violations(sources: dict[str, str]) -> list[str]:
    """Return volatile authority APIs reachable from a default product build."""

    errors: list[str] = []
    for label, path, declaration, gate in NON_PRODUCT_APIS:
        gated = _declaration_is_gated(sources[path], declaration, gate)
        if gated is None:
            errors.append(f"missing non-product authority API `{label}`")
        elif not gated:
            errors.append(f"non-product authority API `{label}` is not test-support gated")
    return errors


def control_component_violations(
    cli_source: str, component_source: str, control_process_source: str
) -> list[str]:
    """Enforce one Control application builder and process-only startup.

    AllInOne and standalone Control may each call the CLI adapter helper, but
    that helper must have exactly one call into the authoritative domain builder.
    Coordinator/runtime startup must never reconstruct ConfigService or invoke
    the lower-level Control router directly.
    """

    errors: list[str] = []
    calls = cli_source.count("awaken_control::build_control_component(")
    if calls != 1:
        errors.append(
            "awaken-cli must contain exactly one call to "
            f"awaken_control::build_control_component (found {calls})"
        )
    for forbidden in ("ConfigService::new(", "awaken_control::control_router("):
        if forbidden in cli_source:
            errors.append(f"awaken-cli reconstructs Control through `{forbidden}`")
    for required in (
        "pub async fn build_control_component(",
        "ConfigService::new(",
        "control_router(ControlRouterInput",
    ):
        if required not in component_source:
            errors.append(f"awaken-control component is missing `{required}`")
    if "prepare_runtime_routers(" in control_process_source:
        errors.append("standalone Control delegates to the runtime process starter")
    if "prepare_control_routers(" not in control_process_source:
        errors.append("standalone Control does not use its dedicated process starter")
    if "ephemeral_resource_component(" in control_process_source:
        errors.append("standalone Control constructs the Resources component")
    return errors


def domain_component_violations(
    cli_source: str,
    control_source: str,
    coordinator_source: str,
    resource_source: str,
    resource_contract_source: str,
    runtime_host_source: str,
    worker_source: str,
) -> list[str]:
    """Enforce the four domain component owners without compatibility tracks."""

    errors: list[str] = []
    coordinator_calls = cli_source.count("awaken_coordinator::build_coordinator_component(")
    if coordinator_calls != 1:
        errors.append(
            "awaken-cli must contain exactly one call to "
            f"awaken_coordinator::build_coordinator_component (found {coordinator_calls})"
        )
    for forbidden in (
        "DeploymentApplication::from_repository(",
        "mount_with_managed_application_access_models_and_dreams(",
        "ResourcePlane::new(",
    ):
        if forbidden in cli_source:
            errors.append(f"awaken-cli reconstructs a domain component through `{forbidden}`")
    for required in (
        "pub async fn build_coordinator_component(",
        "DeploymentApplication::from_repository(",
        "mount_with_managed_application_access_models_and_dreams(",
    ):
        if required not in coordinator_source:
            errors.append(f"awaken-coordinator Coordinator component is missing `{required}`")
    for required in (
        "pub struct ResourceAuthorities",
        "impl ResourceAuthorities",
        "pub fn new(",
    ):
        if required not in resource_source:
            errors.append(f"awaken-resource-application authorities are missing `{required}`")
    for forbidden in (
        "mod component;",
        "ResourceComponent",
        "ResourceDependencies",
        "build_resource_component",
    ):
        if forbidden in resource_contract_source:
            errors.append(
                "awaken-resource-contract contains application ownership "
                f"vocabulary `{forbidden}`"
            )
    if "ResourcePlane" in runtime_host_source:
        errors.append("Runtime Host retains the retired ResourcePlane component owner")
    if "pub struct WorkerNodeBuilder" not in worker_source:
        errors.append("awaken-worker lost its canonical WorkerNodeBuilder component boundary")
    if "build_worker_component" in worker_source:
        errors.append("awaken-worker added a second component builder beside WorkerNodeBuilder")
    if "ManagedSessionRepository" in control_source:
        errors.append("awaken-control still acquires Coordinator's Session repository")
    return errors


def coordinator_resource_access_violations(
    coordinator_manifest: dict,
    cli_manifest: dict,
    source: str,
    component_source: str,
) -> list[str]:
    """Keep Resources persistence/application construction outside Coordinator."""

    errors: list[str] = []
    for consumer, manifest in (
        ("Coordinator", coordinator_manifest),
        ("CLI", cli_manifest),
    ):
        dependencies = manifest.get("dependencies", {})
        for package in (
            "awaken-file-store",
            "awaken-memory-store",
            "awaken-resource-store",
            "awaken-skill-store",
        ):
            if package in dependencies:
                errors.append(
                    f"{consumer} directly selects Resources persistence `{package}`"
                )
    for forbidden in (
        "embedded_resource_component",
        "embedded_resources_application",
        "ephemeral_resources_application",
        "awaken_resource_store::",
        "awaken_file_store::",
        "awaken_memory_store::",
        "awaken_skill_store::",
    ):
        if forbidden in source:
            errors.append(f"Coordinator reconstructs Resources through `{forbidden}`")
    for required in (
        "pub memory_stores: Arc<dyn awaken_resource_contract::MemoryStoreApplicationService>",
        "memory_stores,",
    ):
        if required not in component_source:
            errors.append(
                "Coordinator component does not propagate the exact Resources service "
                f"through `{required}`"
            )
    return errors


def resource_registry_authority_violations(source: str) -> list[str]:
    """The Resources Registry must never discover or import a Control table."""

    forbidden = (
        "admin_memory_store",
        "LegacyMemoryStoreDefinition",
        "migrate_legacy_memory_stores",
    )
    return [
        f"Resources Registry retains Control compatibility path `{token}`"
        for token in forbidden
        if token in source
    ]


def worker_listener_partition_violations(
    coordinator_source: str, component_source: str, boundary_source: str
) -> list[str]:
    """Keep every Worker-facing route on a process-private listener."""

    errors: list[str] = []
    managed_match = re.search(
        r"let managed = managed\.merge\(resource_management_router\)\.merge\(models\);",
        coordinator_source,
    )
    application_match = re.search(
        r"let application = ai_sdk\.merge\(ag_ui\);", coordinator_source
    )
    public_match = re.search(
        r"let router = a2a\.merge\(durable_ops\);(.*?)"
        r"Ok\(\(\s*managed,\s*router,\s*application,\s*worker_transport,",
        coordinator_source,
        re.S,
    )
    if managed_match is None or application_match is None or public_match is None:
        errors.append("Coordinator Managed/public route partition is missing")
    elif ".merge(worker_transport)" in public_match.group(1):
        errors.append("Coordinator public router merges the Worker transport")
    for required in (
        "application_router: application",
        ".merge(worker_transport)",
    ):
        if required not in component_source:
            errors.append(f"private Worker surface is missing `{required}`")
    warmup_merge = ".merge(environment_warmups)"
    if coordinator_source.count(warmup_merge) != 1:
        errors.append(
            "Coordinator Worker transport must merge environment warmups exactly once"
        )
    if warmup_merge in component_source:
        errors.append("private Worker component duplicates the environment warmup merge")
    if 'Some("127.0.0.1:0".to_owned())' not in boundary_source:
        errors.append("AllInOne lacks a loopback-only private Worker listener default")
    return errors


def _struct_body(source: str, name: str) -> str:
    match = re.search(rf"\bstruct\s+{re.escape(name)}\s*\{{(.*?)\n\}}", source, re.S)
    return match.group(1) if match else ""


def process_store_ownership_violations(
    process_stores_source: str, cli_source: str, resource_source: str
) -> list[str]:
    """Enforce physical store acquisition at the four domain boundaries."""

    errors: list[str] = []
    process_body = _struct_body(process_stores_source, "ProcessStores")
    control_body = _struct_body(process_stores_source, "ControlStores")
    coordinator_body = _struct_body(process_stores_source, "CoordinatorStores")
    for required in ("Option<ControlStores>", "Option<CoordinatorStores>"):
        if required not in process_body:
            errors.append(f"ProcessStores is missing role-owned group `{required}`")
    for forbidden in (
        "ManagedSessionRepository",
        "DeploymentRepository",
        "DreamRepository",
        "MemoryExtractionRepository",
        "ResourceAuthorities",
        "ResourceComponent",
        "ResourceRegistry",
    ):
        if forbidden in control_body:
            errors.append(f"ControlStores acquires foreign authority `{forbidden}`")
    for forbidden in (
        "CatalogRepo",
        "CredentialRepo",
        "SecretStore",
        "ScopedConfigRegistry",
        "InferenceProfileStore",
        "WebhookStore",
    ):
        if forbidden in coordinator_body:
            errors.append(f"CoordinatorStores acquires Control authority `{forbidden}`")
    for required in (
        "let opens_control = role_owns_control_component(role);",
        "let coordinator = if role_owns_managed_execution(role)",
        "let manifest = migration_manifest(deployment.role);",
        "let resources = if manifest.contains(&MigrationComponent::Resources)",
    ):
        if required not in cli_source:
            errors.append(f"role-aware store selection is missing `{required}`")
    for required in (
        "resource_registry: Arc<dyn ResourceRegistry>",
        "pub fn resource_registry(&self) -> Arc<dyn ResourceRegistry>",
    ):
        if required not in resource_source:
            errors.append(f"Resources authorities do not own `{required}`")
    return errors


def _normal_dependency_tables(manifest: dict):
    """Yield every non-dev Cargo edge, including target-conditioned tables."""

    for section in ("dependencies", "build-dependencies"):
        yield section, manifest.get(section, {})
    for target, target_manifest in manifest.get("target", {}).items():
        for section in ("dependencies", "build-dependencies"):
            yield f"target.{target}.{section}", target_manifest.get(section, {})


def _normal_dependencies(manifest: dict) -> set[str]:
    dependencies: set[str] = set()
    for _, table in _normal_dependency_tables(manifest):
        for name, value in table.items():
            dependencies.add(name)
            if isinstance(value, dict) and isinstance(value.get("package"), str):
                dependencies.add(value["package"])
    return dependencies


def product_test_support_violations(manifest: dict) -> list[str]:
    """Return default or normal/build edges that enable test-only capabilities."""

    errors: list[str] = []
    defaults = manifest.get("features", {}).get("default", [])
    for feature in defaults:
        if feature == "test-support" or feature.endswith("/test-support"):
            errors.append(f"default feature enables `{feature}`")
    for section, table in _normal_dependency_tables(manifest):
        for name, value in table.items():
            if isinstance(value, dict) and "test-support" in value.get("features", []):
                errors.append(f"{section} dependency `{name}` enables test-support")
    return sorted(errors)


def product_inmem_dependency_violations(manifest_path: str, manifest: dict) -> list[str]:
    """Keep the volatile backend off normal product edges.

    The sole non-optional exception is `awaken-store-fs`: it uses the coordinator
    as a deterministic projection rebuilt from its fsync append log, never as the
    selected persistence authority. Optional dependencies remain legal only when
    an explicit non-default test-support feature enables them.
    """

    if manifest_path == STORE_FS_MANIFEST:
        return []
    test_feature = set(manifest.get("features", {}).get("test-support", []))
    errors: list[str] = []
    for section, table in _normal_dependency_tables(manifest):
        for name, value in table.items():
            package = value.get("package") if isinstance(value, dict) else None
            if name != "awaken-store-inmem" and package != "awaken-store-inmem":
                continue
            explicitly_test_only = (
                isinstance(value, dict)
                and value.get("optional") is True
                and (f"dep:{name}" in test_feature or name in test_feature)
            )
            if not explicitly_test_only:
                errors.append(
                    f"{section} dependency `{name}` links the selectable in-memory backend"
                )
    return sorted(errors)


def redundant_runtime_memory_reexport_violations(source: str) -> list[str]:
    """The reference backend has one public owner: `awaken-store-inmem`."""

    if re.search(r"\bpub\s+mod\s+memory\b|\bawaken_store_inmem\b", source):
        return ["Runtime recreates the awaken-store-inmem public API path"]
    return []


def coordinator_persistence_ownership_violations(cli_source: str) -> list[str]:
    """Keep backend initialization on the one runtime-process startup path."""

    errors: list[str] = []
    for call in (
        "awaken_coordinator::open_coordinator_persistence(",
        "awaken_coordinator::open_existing_coordinator_persistence(",
    ):
        count = cli_source.count(call)
        if count != 1:
            errors.append(
                f"awaken-cli must contain exactly one `{call}` call (found {count})"
            )
    for retired in (
        "init_postgres_coordinator",
        "init_existing_postgres_coordinator",
        "init_worker_registry",
        "worker_directory",
    ):
        if re.search(rf"\b{re.escape(retired)}\s*\(", cli_source):
            errors.append(f"awaken-cli retains retired Worker authority path `{retired}`")
    return errors


def product_dispatch_fallback_violations(source: str) -> list[str]:
    """Require missing SQLite durability to fail closed outside test support."""

    coordinator_owned = (
        "Coordinator SQLite dispatch requires runtime.storage_dir; refusing volatile dispatch authority"
        in source
        and "open_sqlite_in_memory" not in source
    )
    guarded = re.search(
        rf"None\s*=>\s*\{{.*?{TEST_SUPPORT_GATE}.*?"
        rf"AnyDispatchStore::open_sqlite_in_memory\(\).*?"
        rf"#\s*\[\s*cfg\s*\(\s*not\s*\(\s*any\s*\(\s*test\s*,\s*feature\s*=\s*\"test-support\"\s*\)\s*\)\s*\)\s*\].*?"
        r"return\s+Err\(.*?product SQLite dispatch requires a durable storage_dir",
        source,
        re.DOTALL,
    )
    return [] if coordinator_owned or guarded else [
        "SQLite dispatch missing-storage path does not fail closed"
    ]


def runtime_commit_authority_violations(source: str) -> list[str]:
    """Runtime Host may discriminate local/remote commits, never Store backends."""

    forbidden = (
        "CommitPlan",
        "thread_commit_path",
        "StoreKind",
        "SqliteCommitCoordinator",
        "FsCommitCoordinator",
        "PostgresCommitCoordinator",
    )
    return [
        f"Runtime Host retains commit backend selector `{name}`"
        for name in forbidden
        if name in source
    ]


def redundant_admin_store_reexport_violations(source: str) -> list[str]:
    """Keep resolver store contracts on their one authoritative public path."""

    return (
        ["Admin API re-exports Config Resolver store contracts or fixtures"]
        if REDUNDANT_ADMIN_STORE_REEXPORT.search(source)
        else []
    )


def webhook_mutation_authority_violations(source: str) -> list[str]:
    """Keep one recoverable Webhook aggregate mutation path."""

    errors: list[str] = []
    for retired in (
        r"fn\s+put\s*\(\s*&self\s*,\s*def\s*:\s*WebhookEndpointDef",
        r"fn\s+delete\s*\(\s*&self\s*,\s*id\s*:\s*&str",
    ):
        if re.search(retired, source):
            errors.append("WebhookStore exposes a retired direct put/delete mutation")
    for required in (
        "fn update_authored",
        "fn begin_mutation",
        "fn apply_mutation",
        "fn pending_mutations",
        "fn complete_mutation",
        "fn material_refs",
    ):
        if required not in source:
            errors.append(f"WebhookStore is missing recoverable authority `{required}`")
    return errors


def model_inventory_authority_violations(
    contract_source: str,
    coordinator_source: str,
    protocol_source: str,
    startup_source: str,
) -> list[str]:
    """Keep one model-reference rule over executable Agent inventory."""

    errors: list[str] = []
    if contract_source.count("pub async fn current_model_references(") != 1:
        errors.append(
            "executable Agent contract must own exactly one current-model rule"
        )
    for owner, source in (
        ("Coordinator Dream readiness", coordinator_source),
        ("Managed model projection", protocol_source),
    ):
        if "current_model_references(" not in source:
            errors.append(f"{owner} bypasses the executable Agent inventory rule")
    if startup_source.count("let model_inventory:") != 1:
        errors.append("runtime startup must select exactly one executable Agent inventory")
    for forbidden in (
        "ModelDirectory",
        "CatalogModelDirectory",
        "ExecutableAgentModelDirectory",
        "project_executable_models",
    ):
        if any(
            forbidden in source
            for source in (contract_source, coordinator_source, protocol_source, startup_source)
        ):
            errors.append(f"retired model projection path reappeared through `{forbidden}`")
    return errors


def inference_materializer_authority_violations(
    authoritative_source: str, coordinator_source: str
) -> list[str]:
    """Keep one publication-pinned candidate router outside Coordinator."""

    errors: list[str] = []
    count = authoritative_source.count("struct PinnedCandidateExecutor")
    if count != 1:
        errors.append(
            "Credential materializer must own exactly one pinned candidate router; "
            f"found {count}"
        )
    for forbidden in (
        "struct CredentialInferenceMaterializer",
        "struct PinnedCandidateExecutor",
        "impl InferenceExecutorMaterializer",
    ):
        if forbidden in coordinator_source:
            errors.append(
                f"Coordinator recreates credential inference authority `{forbidden}`"
            )
    return errors


def control_publication_authority_violations(
    control_source: str, coordinator_source: str
) -> list[str]:
    """Control alone resolves authored model facts into immutable candidates."""

    errors: list[str] = []
    count = control_source.count("struct CatalogModelPublicationResolver")
    if count != 1:
        errors.append(
            "Control must own exactly one catalog model publication resolver; "
            f"found {count}"
        )
    for forbidden in (
        "struct CatalogModelPublicationResolver",
        "impl ModelPublicationResolver for CatalogModelPublicationResolver",
        "CatalogRepo",
        "CredentialRepo",
    ):
        if forbidden in coordinator_source:
            errors.append(f"Coordinator reads Control publication authority `{forbidden}`")
    return errors


def coordinator_control_authority_dependency_violations(manifest: dict) -> list[str]:
    """Coordinator may consume narrow contracts, never Control authority adapters."""

    forbidden = {
        "awaken-admin-config-api",
        "awaken-config-resolver",
        "awaken-credential-store",
        "awaken-model-catalog",
        "awaken-model-catalog-store",
    }
    return [
        f"Coordinator directly depends on Control authority `{package}`"
        for package in sorted(_normal_dependencies(manifest) & forbidden)
    ]


def credential_refresh_authority_violations(
    materializer_source: str, coordinator_source: str
) -> list[str]:
    """Exact Vault refresh belongs to the existing credential adapter."""

    errors: list[str] = []
    for required in (
        "trait CredentialRefreshFactory",
        "struct VaultRefreshFactory",
        "struct VaultRefresher",
    ):
        if materializer_source.count(required) != 1:
            errors.append(f"Credential materializer must own one `{required}`")
    for forbidden in (
        "trait CredentialRefreshFactory",
        "struct VaultRefreshFactory",
        "struct VaultRefresher",
    ):
        if forbidden in coordinator_source:
            errors.append(f"Coordinator recreates credential refresh authority `{forbidden}`")
    return errors


def selftest() -> None:
    """Cause/effect decision table.

    O1 neutral Worker contracts/adapters -> accepted; O2 a direct authority-store
    dependency (including a Cargo alias) -> rejected; O3 an authority-store
    dependency reached through a transitive normal edge -> rejected; O4 ordinary
    HTTP adapter construction -> accepted. O5 Control creates Managed Execution
    -> rejected; O6 Control-only construction -> accepted; O7 retired launch path
    -> rejected; O8 one Control builder -> accepted; O9 duplicate/wrong Control
    path -> rejected; O10 the four canonical component owners -> accepted; O11
    cross-owner or parallel component construction -> rejected; O12 standalone
    Control constructs Resources -> rejected; O13 grouped role-owned stores and
    Resources catalog -> accepted; O14 a cross-domain store field or unconditional
    migration acquisition -> rejected; O15 every volatile Host API is test-support
    gated -> accepted; O16 one missing Host gate -> rejected; O17 all canonical
    CLI scenario/restart helpers, File/Memory/Skill/Resource/Environment volatile
    entrypoints, the ephemeral Resources fixture, and permissive webhook
    loopback transport are test-support gated ->
    accepted; O18 any one gate missing -> rejected; O19 product defaults/normal
    edges do not enable test-support ->
    accepted; O20 a product default, top-level normal edge, or target-conditioned
    normal edge enables it -> rejected while dev-dependencies and opt-in features
    remain accepted; O21 missing SQLite
    dispatch durability fails closed in product and selects memory only with test
    support -> accepted; O22 an unconditional in-memory fallback -> rejected;
    O23 one authoritative Config Resolver store path -> accepted; O24 an Admin
    compatibility re-export of that path -> rejected; O25 Runtime Host carries
    only local/remote commit semantics -> accepted; O26 any Store backend selector
    retained by Runtime Host -> rejected; O27 the
    FS log's rebuild projection and an optional test-support feature may depend on
    inmem -> accepted; O28 an ordinary, aliased, or target-conditioned product
    dependency on the selectable backend -> rejected; O29 Runtime has no
    compatibility re-export -> accepted; O30 a second Runtime public path for the
    backend -> rejected; O31 Coordinator persistence is initialized exactly once
    per schema mode in the canonical Runtime startup -> accepted; O32 a duplicate
    initializer or retired global Worker authority path -> rejected; O33 one
    journaled Webhook mutation authority -> accepted; O34 direct put/delete or a
    missing recovery edge -> rejected; O35 an empty Runtime Host default plus an
    explicit Coordinator capability -> accepted; O36 an implicit default or a
    detached Coordinator capability -> rejected; O37 all role-named executables
    terminate in one lifecycle -> accepted; O38 a missing target, Worker twin, or
    entrypoint-local application construction -> rejected; O39 one normalized
    model-reference rule shared by Dream and Managed projections -> accepted;
    O40 a retired directory or duplicate/missing inventory selection -> rejected; O41
    one candidate router in credential materialization -> accepted; O42 a
    missing/duplicate router or Coordinator-owned materializer -> rejected; O43
    one Control publication resolver -> accepted; O44 a missing/duplicate
    resolver or Coordinator Control-store reader -> rejected; O45 one credential
    refresh adapter -> accepted; O46 missing/duplicate or Coordinator-owned
    refresh mechanics -> rejected; O47 narrow Coordinator dependencies ->
    accepted; O48 a direct Control authority adapter -> rejected.
    Together the rules cover compile-time acquisition, production call paths,
    component ownership, and schema acquisition.
    """

    assert dependency_violations({"awaken-runtime-host", "awaken-runtime-contract"}) == []  # O1
    assert dependency_violations({"awaken-session-store"}) == ["awaken-session-store"]  # O2
    graph = {
        "awaken-worker": {"awaken-runtime-host"},
        "awaken-runtime-host": {"awaken-session-store"},
        "awaken-session-store": {"sqlx"},
    }
    assert dependency_violations(dependency_closure("awaken-worker", graph)) == [
        "awaken-session-store",
        "sqlx",
    ]  # O3
    assert source_violations("let store = PostgresMemoryRepository::connect(url).await?;")  # O3
    assert source_violations("let client = HttpMemoryRepository::new(url, token);") == []  # O4
    aliased = {"dependencies": {"session_backend": {"package": "awaken-session-store"}}}
    assert dependency_violations(_normal_dependencies(aliased)) == ["awaken-session-store"]  # O2
    assert control_execution_violations("let x = DeploymentApplication::new();") == [
        "DeploymentApplication"
    ]  # O5
    assert control_execution_violations("let x = ConfigPlane::new();") == []  # O6
    assert retired_launch_violations("HttpDeploymentSessionLauncher::new(url, token)") == [
        "HttpDeploymentSessionLauncher"
    ]  # O7
    component = (
        "pub async fn build_control_component("
        " ConfigService::new( control_router(ControlRouterInput"
    )
    assert control_component_violations(
        "awaken_control::build_control_component(",
        component,
        "prepare_control_routers(",
    ) == []  # O8 one canonical builder
    assert control_component_violations(
        "ConfigService::new( awaken_control::build_control_component( ",
        component,
        "prepare_runtime_routers(",
    )  # O9 duplicate CLI construction and wrong standalone path
    assert control_component_violations(
        "awaken_control::build_control_component(",
        component,
        "prepare_control_routers( ephemeral_resource_component(",
    )  # O12 Control must consume Resource services without constructing Resources
    coordinator = (
        "pub async fn build_coordinator_component("
        " DeploymentApplication::from_repository("
        " mount_with_managed_application_access_models_and_dreams("
    )
    resources = "pub struct ResourceAuthorities impl ResourceAuthorities pub fn new("
    # FMECA/cause-effect decision table for Resources authority ownership:
    # FM1 a partial authority selection can pair File/Memory/Skill repositories
    # from different backends; FM2 a second owner can reopen the same data.
    # R1 application owns ResourceAuthorities and contract owns none -> accept;
    # R2 any required application symbol is absent -> reject incomplete owner;
    # R3 a retired owner returns to contract -> reject a parallel owner;
    # R4 CLI/Runtime/Worker reconstruct another owner -> reject at its boundary.
    assert domain_component_violations(
        "awaken_coordinator::build_coordinator_component(",
        "",
        coordinator,
        resources,
        "",
        "",
        "pub struct WorkerNodeBuilder",
    ) == []  # O10 R1 four canonical application/authority owners
    assert domain_component_violations(
        "DeploymentApplication::from_repository(",
        "ManagedSessionRepository",
        "",
        "",
        "pub use component::ResourceComponent;",
        "ResourcePlane",
        "pub struct WorkerNodeBuilder build_worker_component",
    )  # O11 R2/R3/R4 parallel or cross-owner component construction
    # Resources authority causes/effects:
    # R1 no concrete store deps/factories + injected MemoryStore service -> accept;
    # R2 any concrete Resources dependency -> reject cross-context acquisition;
    # R3 any embedded/ephemeral factory -> reject a second backend selector;
    # R4 missing injected service -> reject HTTP/Dream parallel construction;
    # R5 CLI links a concrete store beside persistence bootstrap -> reject.
    coordinator_resources = (
        "pub memory_stores: Arc<dyn "
        "awaken_resource_contract::MemoryStoreApplicationService> memory_stores,"
    )
    assert coordinator_resource_access_violations(
        {"dependencies": {}}, {"dependencies": {}}, "", coordinator_resources
    ) == []  # O11a R1
    assert coordinator_resource_access_violations(
        {"dependencies": {"awaken-memory-store": {}}},
        {"dependencies": {"awaken-skill-store": {}}},
        "embedded_resource_component awaken_resource_store::",
        "",
    )  # O11b R2/R3/R4/R5
    # Resource Registry authority causes/effects: R1 the adapter reads only its
    # own physical tables -> accept; R2 it probes/imports any legacy Control table
    # -> reject the second source of truth and cross-database assumption.
    assert resource_registry_authority_violations("SELECT data FROM resource_catalog_entry") == []
    assert resource_registry_authority_violations(
        "LegacyMemoryStoreDefinition migrate_legacy_memory_stores admin_memory_store"
    )  # O11e R2
    # Worker listener causes/effects: R1 Coordinator merges warmups exactly once
    # into the returned Worker transport, the component merges that transport
    # only into private, and AllInOne has a loopback default -> accept. R2 any
    # missing/duplicate merge or public merge -> reject accidental exposure or
    # two composition owners.
    worker_partition = (
        "let managed = managed.merge(resource_management_router).merge(models); "
        "let application = ai_sdk.merge(ag_ui); "
        "let worker_transport = worker_transport.merge(environment_warmups); "
        "let router = a2a.merge(durable_ops); "
        "Ok((managed, router, application, worker_transport, dream_application))"
    )
    private_partition = "application_router: application .merge(worker_transport)"
    assert worker_listener_partition_violations(
        worker_partition,
        private_partition,
        'Some("127.0.0.1:0".to_owned())',
    ) == []  # O11c R1
    assert worker_listener_partition_violations(
        worker_partition.replace("a2a.merge(durable_ops)", "a2a.merge(worker_transport)"),
        "",
        "",
    )  # O11d R2
    assert worker_listener_partition_violations(
        worker_partition,
        private_partition + ".merge(environment_warmups)",
        'Some("127.0.0.1:0".to_owned())',
    )  # O11d R2 duplicate composition owner
    process_stores = (
        "struct ProcessStores { control: Option<ControlStores>, "
        "coordinator: Option<CoordinatorStores>\n}\n"
        "struct ControlStores { catalog: CatalogRepo\n}\n"
        "struct CoordinatorStores { sessions: ManagedSessionRepository\n}\n"
    )
    role_aware = (
        "let opens_control = role_owns_control_component(role); "
        "let coordinator = if role_owns_managed_execution(role) "
        "let manifest = migration_manifest(deployment.role); "
        "let resources = if manifest.contains(&MigrationComponent::Resources)"
    )
    resource_owner = (
        "resource_registry: Arc<dyn ResourceRegistry> "
        "pub fn resource_registry(&self) -> Arc<dyn ResourceRegistry>"
    )
    assert process_store_ownership_violations(
        process_stores, role_aware, resource_owner
    ) == []  # O13
    assert process_store_ownership_violations(
        process_stores.replace("catalog: CatalogRepo", "sessions: ManagedSessionRepository"),
        role_aware.replace(
            "let resources = if manifest.contains(&MigrationComponent::Resources)", ""
        ),
        resource_owner,
    )  # O14
    volatile_surface = "\n".join(
        f'#[cfg(any(test, feature = "test-support"))]\npub fn {name}() {{}}'
        for name in VOLATILE_RUNTIME_HOST_APIS
    )
    assert volatile_runtime_host_surface_violations(volatile_surface) == []  # O15
    assert volatile_runtime_host_surface_violations(
        volatile_surface.replace(
            '#[cfg(any(test, feature = "test-support"))]\npub fn with_store_dir',
            "pub fn with_store_dir",
        )
    ) == ["volatile Host API `with_store_dir` is not test-support gated"]  # O16
    any_gate = '#[cfg(any(test, feature = "test-support"))]\n'
    feature_gate = '#[cfg(feature = "test-support")]\n'
    volatile_surfaces = {
        CLI_LIB_SOURCE: any_gate
        + "mod exact_host_model;\n"
        + any_gate
        + "mod local_process_stores;\n"
        + "\n".join(
            any_gate + f"pub async fn {name}() {{}}"
            for name in (
                "build_ephemeral_all_in_one_router",
                "build_all_in_one_router_with_scenario_model",
                "build_all_in_one_router_with_host_customizer",
                "build_durable_all_in_one_router_with_host_customizer",
                "build_all_in_one_router_with_model",
                "build_durable_all_in_one_router",
                "build_secured_all_in_one_router",
            )
        ),
        FILE_STORE_SOURCE: any_gate + "pub struct InMemoryFileStore {}",
        MEMORY_STORE_SOURCE: any_gate + "pub struct VolatileMemoryRepository {}",
        MEMORY_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        SKILL_STORE_SOURCE: any_gate + "pub struct InMemorySkillStore {}",
        SKILL_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        RESOURCE_STORE_SOURCE: any_gate + "pub fn in_memory() {}",
        RUNTIME_MEMORY_STORES: any_gate + "pub(crate) fn open() {}",
        RESOURCE_PERSISTENCE: feature_gate
        + "pub fn ephemeral() {}\n",
        COORDINATOR_SOURCE: feature_gate
        + any_gate
        + "pub use worker_registry::test_directory as test_worker_directory;",
        ENV_STORE_SQLITE_SOURCE: any_gate
        + "pub use inmem::InMemoryEnvRegistry;\n"
        + any_gate
        + "pub fn open_in_memory() {}",
        SANDBOX_POLICY_STORE_SOURCE: any_gate
        + "pub struct InMemorySandboxExecutionPolicyStore {}",
        ENV_IMAGE_BUILD_SOURCE: any_gate
        + "pub use in_memory::InMemoryEnvironmentImageBuildStore;",
        ENV_IMAGE_BUILD_SQLITE_SOURCE: any_gate
        + "pub fn open_in_memory_environment_image_build_store() {}",
        SESSION_STORE_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        WORK_STORE_SOURCE: any_gate
        + "pub use inmem::InMemoryWorkQueue;\n"
        + any_gate
        + "pub fn open_in_memory() {}",
        COMMIT_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        FILE_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        WORKER_REGISTRY_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        WORKER_REGISTRY_LIB_SOURCE: any_gate
        + "mod memory;\n"
        + any_gate
        + "pub use memory::MemoryWorkerDirectory;",
        DREAM_APPLICATION_SOURCE: any_gate
        + "pub struct InMemoryDreamProcessStore;",
        TOOL_RELAY_SOURCE: any_gate
        + "pub use ledger::InMemoryOperationLedger;",
        RUN_INGRESS_ANY_SOURCE: any_gate + "pub fn open_sqlite_in_memory() {}",
        RUN_INGRESS_LIB_SOURCE: any_gate
        + "pub mod memory;\n"
        + any_gate
        + "pub use memory::MemoryDispatchStore;",
        RUN_INGRESS_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        CAPTURE_STORE_SOURCE: any_gate
        + "pub use capture_store::{CapturedRecord, InMemoryCapturedContentStore};",
        CAPTURE_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        DATA_SUBJECT_SOURCE: any_gate + "pub struct InMemoryDataSubjectRepo {}",
        DATA_SUBJECT_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        CONFIG_STORE_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        ADMIN_CONFIG_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        MODEL_CATALOG_REPO_SOURCE: any_gate + "pub struct InMemoryCatalogRepo {}",
        MODEL_CATALOG_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        CONFIG_RESOLVER_SOURCE: any_gate
        + "pub use reference_stores::{InMemoryAgentInputBindingRepository, "
        + "InMemoryProfileStore, InMemoryWebhookStore};",
        CREDENTIAL_REPO_SOURCE: any_gate + "pub struct InMemoryCredentialRepo {}",
        CREDENTIAL_VAULT_SOURCE: any_gate
        + "pub struct InMemorySealedBlobStore {}\n"
        + any_gate
        + "pub struct InMemorySecretStore {}",
        CREDENTIAL_SQLITE_SOURCE: any_gate
        + "pub fn open_in_memory() {}\n"
        + any_gate
        + "pub fn open_in_memory() {}",
        CREDENTIAL_SEALED_SOURCE: any_gate + "pub fn with_key() {}",
        WEBHOOK_DISPATCH_SOURCE: any_gate
        + "impl Default for ReqwestSender {}\n"
        + any_gate
        + "pub fn with_timeout() {}",
        WEBHOOK_CONTROL_PLANE_SOURCE: (
            feature_gate
            + "pub fn loopback_lifecycle_delivery() {}\n"
            + feature_gate
            + "pub fn webhook_config_router_loopback() {}"
        ),
    }
    assert non_product_surface_violations(volatile_surfaces) == []  # O17
    for label, path, declaration, gate in NON_PRODUCT_APIS:
        broken = volatile_surfaces.copy()
        broken[path] = re.sub(
            rf"{gate}\s*(?={declaration})", "", broken[path], count=1
        )
        assert non_product_surface_violations(broken) == [
            f"non-product authority API `{label}` is not test-support gated"
        ]  # O18 each independent gate-removal cause
    test_only_edges = {
        "features": {"test-support": ["store/test-support"]},
        "dev-dependencies": {
            "store": {"features": ["test-support"]},
        },
    }
    assert product_test_support_violations(test_only_edges) == []  # O19/O20 accepted
    assert product_test_support_violations(
        {"dependencies": {"store": {"features": ["test-support"]}}}
    ) == ["dependencies dependency `store` enables test-support"]  # O20 normal edge
    assert product_test_support_violations(
        {"features": {"default": ["store/test-support"]}}
    ) == ["default feature enables `store/test-support`"]  # O20 default edge
    assert product_test_support_violations(
        {
            "target": {
                "cfg(unix)": {
                    "dependencies": {"store": {"features": ["test-support"]}}
                }
            }
        }
    ) == [
        "target.cfg(unix).dependencies dependency `store` enables test-support"
    ]  # O20 target-conditioned normal edge
    guarded_dispatch = (
        'None => { #[cfg(any(test, feature = "test-support"))] '
        "AnyDispatchStore::open_sqlite_in_memory()?; "
        '#[cfg(not(any(test, feature = "test-support")))] '
        'return Err(internal("product SQLite dispatch requires a durable storage_dir")); }'
    )
    assert product_dispatch_fallback_violations(guarded_dispatch) == []  # O21
    assert product_dispatch_fallback_violations(
        "None => AnyDispatchStore::open_sqlite_in_memory()?"
    ) == ["SQLite dispatch missing-storage path does not fail closed"]  # O22
    assert redundant_admin_store_reexport_violations("pub struct AdminState;") == []  # O23
    assert redundant_admin_store_reexport_violations(
        "pub use awaken_config_resolver::{InferenceProfileStore, InMemoryProfileStore};"
    ) == ["Admin API re-exports Config Resolver store contracts or fixtures"]  # O24
    assert runtime_commit_authority_violations("enum HostCommit { Local, Remote }") == []  # O25
    assert runtime_commit_authority_violations("enum CommitPlan { Sqlite }")  # O26
    assert product_inmem_dependency_violations(
        STORE_FS_MANIFEST, {"dependencies": {"awaken-store-inmem": {}}}
    ) == []  # O27 rebuild projection
    assert product_inmem_dependency_violations(
        RUNTIME_HOST_MANIFEST,
        {
            "dependencies": {"awaken-store-inmem": {"optional": True}},
            "features": {"test-support": ["dep:awaken-store-inmem"]},
        },
    ) == []  # O27 explicit test feature
    assert product_inmem_dependency_violations(
        "crates/runtime/product/Cargo.toml",
        {"dependencies": {"awaken-store-inmem": {}}},
    ) == [
        "dependencies dependency `awaken-store-inmem` links the selectable in-memory backend"
    ]  # O28
    assert product_inmem_dependency_violations(
        "crates/runtime/product/Cargo.toml",
        {
            "target": {
                "cfg(unix)": {
                    "dependencies": {
                        "reference_store": {"package": "awaken-store-inmem"}
                    }
                }
            }
        },
    ) == [
        "target.cfg(unix).dependencies dependency `reference_store` links the selectable in-memory backend"
    ]  # O28 target-conditioned alias
    assert redundant_runtime_memory_reexport_violations("pub mod engine;") == []  # O29
    assert redundant_runtime_memory_reexport_violations(
        "pub mod memory; pub use awaken_store_inmem::*;"
    ) == ["Runtime recreates the awaken-store-inmem public API path"]  # O30
    canonical_persistence = (
        "awaken_coordinator::open_coordinator_persistence(\n"
        "awaken_coordinator::open_existing_coordinator_persistence("
    )
    assert coordinator_persistence_ownership_violations(canonical_persistence) == []  # O31
    assert coordinator_persistence_ownership_violations(
        canonical_persistence
        + " awaken_coordinator::open_coordinator_persistence( worker_directory("
    )  # O32
    webhook_store = " ".join(
        (
            "fn update_authored",
            "fn begin_mutation",
            "fn apply_mutation",
            "fn pending_mutations",
            "fn complete_mutation",
            "fn material_refs",
        )
    )
    assert webhook_mutation_authority_violations(webhook_store) == []  # O33
    assert webhook_mutation_authority_violations(
        webhook_store.replace("fn material_refs", "")
        + " fn put(&self, def: WebhookEndpointDef"
    )  # O34
    assert runtime_host_feature_violations(
        {"features": {"default": [], "test-support": []}}
    ) == []  # O35
    assert runtime_host_feature_violations(
        {"features": {"default": ["authority"], "coordinator": []}}
    ) == [
        "Runtime Host default features must be authority-free",
        "Runtime Host must not retain a Coordinator compatibility feature",
    ]  # O36
    service_bins = {
        "package": {"default-run": "awaken"},
        "bin": [
            {"name": "awaken", "path": "src/main.rs"},
            {"name": "awaken-control", "path": "src/bin/awaken-control.rs"},
            {
                "name": "awaken-coordinator",
                "path": "src/bin/awaken-coordinator.rs",
            },
        ]
    }
    assert service_binary_violations(
        service_bins,
        "pub async fn run_service( async fn migrate_service_for_role(",
        "run_service_binary(",
        "run_service_binary(",
    ) == []  # O37
    assert service_binary_violations(
        {
            "package": {"default-run": "awaken"},
            "bin": service_bins["bin"]
            + [{"name": "awaken-worker", "path": "worker.rs"}],
        },
        "pub async fn run_service( pub async fn run_service( async fn migrate_service_for_role(",
        "build_control()",
        "",
    )  # O38
    # Model inventory FMECA/cause-effect table: FM1 independent Dream/HTTP model
    # normalization diverges; FM2 a Control catalog reader returns beside the
    # executable publication inventory. R39 one rule + both consumers + one
    # startup inventory => accept; R40 any missing cause or retired directory => reject.
    assert model_inventory_authority_violations(
        "pub async fn current_model_references(",
        "current_model_references(",
        "current_model_references(",
        "let model_inventory:",
    ) == []  # O39
    assert model_inventory_authority_violations(
        "pub async fn current_model_references(",
        "ModelDirectory current_model_references(",
        "",
        "let model_inventory: let model_inventory:",
    )  # O40 retired path, missing consumer, and duplicate selection
    assert inference_materializer_authority_violations(
        "struct PinnedCandidateExecutor;",
        "pub mod coordinator_component;",
    ) == []  # O41
    assert inference_materializer_authority_violations(
        "struct PinnedCandidateExecutor; struct PinnedCandidateExecutor;",
        "struct CredentialInferenceMaterializer; impl InferenceExecutorMaterializer for X {}",
    )  # O42
    assert control_publication_authority_violations(
        "struct CatalogModelPublicationResolver;",
        "pub mod coordinator_component;",
    ) == []  # O43
    assert control_publication_authority_violations(
        "",
        "struct CatalogModelPublicationResolver; CredentialRepo",
    )  # O44
    assert credential_refresh_authority_violations(
        "trait CredentialRefreshFactory; struct VaultRefreshFactory; struct VaultRefresher;",
        "pub mod coordinator_component;",
    ) == []  # O45
    assert credential_refresh_authority_violations(
        "trait CredentialRefreshFactory;",
        "struct VaultRefresher;",
    )  # O46
    assert coordinator_control_authority_dependency_violations(
        {"dependencies": {"awaken-config-service": {"workspace": True}}}
    ) == []  # O47
    assert coordinator_control_authority_dependency_violations(
        {"dependencies": {"awaken-model-catalog": {"workspace": True}}}
    )  # O48


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    product_manifests: dict[str, dict] = {}
    product_manifest_paths = sorted(
        path
        for path in (repo_root / "crates").rglob("Cargo.toml")
        if "devtools" not in path.relative_to(repo_root / "crates").parts
    )
    for manifest_path in product_manifest_paths:
        product_manifest = manifest_path.relative_to(repo_root).as_posix()
        with (repo_root / product_manifest).open("rb") as handle:
            product_manifests[product_manifest] = tomllib.load(handle)
        for error in product_test_support_violations(product_manifests[product_manifest]):
            errors.append(f"{product_manifest}: {error}")
        for error in product_inmem_dependency_violations(
            product_manifest, product_manifests[product_manifest]
        ):
            errors.append(f"{product_manifest}: {error}")
    try:
        dependencies = resolved_product_dependencies(
            repo_root, "awaken-worker", "Worker"
        )
    except RuntimeError as error:
        errors.append(str(error))
        dependencies = set()
    for package in dependency_violations(dependencies):
        errors.append(
            f"{WORKER_MANIFEST}: Worker transitively links authority-store dependency `{package}`; "
            "use the existing claim-fenced boundary adapter"
        )
    runtime_host_manifest = product_manifests[RUNTIME_HOST_MANIFEST]
    for error in runtime_host_feature_violations(runtime_host_manifest):
        errors.append(f"{RUNTIME_HOST_MANIFEST}: {error}")
    try:
        runtime_dependencies = resolved_product_dependencies(
            repo_root, "awaken-runtime-host", "default Runtime Host"
        )
    except RuntimeError as error:
        errors.append(str(error))
        runtime_dependencies = set()
    for package in dependency_violations(runtime_dependencies):
        errors.append(
            f"{RUNTIME_HOST_MANIFEST}: default Runtime Host transitively links "
            f"authority-store dependency `{package}`; select it only during Coordinator startup"
        )
    try:
        protocol_dependencies = resolved_product_dependencies(
            repo_root, "awaken-protocol-managed", "Managed protocol"
        )
    except RuntimeError as error:
        errors.append(str(error))
        protocol_dependencies = set()
    for package in sorted(
        protocol_dependencies
        & {
            "awaken-file-store",
            "awaken-memory-store",
            "awaken-resource-store",
            "awaken-session-store",
            "awaken-skill-store",
        }
    ):
        errors.append(
            f"{PROTOCOL_MANAGED_MANIFEST}: default Managed protocol transitively links "
            f"concrete persistence adapter `{package}`; inject the existing contract/application"
        )
    for error in service_binary_violations(
        product_manifests[CLI_MANIFEST],
        (repo_root / CLI_SERVICE).read_text(encoding="utf-8"),
        (repo_root / CONTROL_BIN).read_text(encoding="utf-8"),
        (repo_root / COORDINATOR_BIN).read_text(encoding="utf-8"),
    ):
        errors.append(f"{CLI_MANIFEST}: {error}")

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
    cli_sources: list[str] = []
    for path in sorted(cli_root.rglob("*.rs")):
        raw_source = path.read_text(encoding="utf-8")
        cli_sources.append(raw_source)
        source = re.split(r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*]", raw_source, maxsplit=1)[0]
        for token in retired_launch_violations(source):
            errors.append(
                f"{path.relative_to(repo_root)}: retired remote Deployment launch "
                f"vocabulary `{token}` reappeared"
            )
    errors.extend(
        control_component_violations(
            "\n".join(cli_sources),
            (repo_root / CONTROL_SOURCE / "component.rs").read_text(encoding="utf-8"),
            (repo_root / CLI_SOURCE / "control.rs").read_text(encoding="utf-8"),
        )
    )
    errors.extend(
        domain_component_violations(
            "\n".join(cli_sources),
            "\n".join(
                path.read_text(encoding="utf-8")
                for path in sorted((repo_root / CONTROL_SOURCE).rglob("*.rs"))
            ),
            (repo_root / COORDINATOR_COMPONENT).read_text(encoding="utf-8"),
            (repo_root / RESOURCE_AUTHORITIES).read_text(encoding="utf-8"),
            (repo_root / RESOURCE_CONTRACT).read_text(encoding="utf-8"),
            (repo_root / RUNTIME_HOST_BUILD).read_text(encoding="utf-8"),
            "\n".join(
                path.read_text(encoding="utf-8")
                for path in sorted((repo_root / WORKER_SOURCE).rglob("*.rs"))
            ),
        )
    )
    errors.extend(
        coordinator_resource_access_violations(
            product_manifests[COORDINATOR_MANIFEST],
            product_manifests[CLI_MANIFEST],
            (repo_root / COORDINATOR_SOURCE).read_text(encoding="utf-8"),
            (repo_root / COORDINATOR_COMPONENT).read_text(encoding="utf-8"),
        )
    )
    errors.extend(
        resource_registry_authority_violations(
            (repo_root / RESOURCE_POSTGRES_REGISTRY).read_text(encoding="utf-8")
        )
    )
    errors.extend(
        worker_listener_partition_violations(
            (repo_root / COORDINATOR_SOURCE).read_text(encoding="utf-8"),
            (repo_root / COORDINATOR_COMPONENT).read_text(encoding="utf-8"),
            (repo_root / SERVICE_BOUNDARY).read_text(encoding="utf-8"),
        )
    )
    errors.extend(
        process_store_ownership_violations(
            (repo_root / PROCESS_STORES).read_text(encoding="utf-8"),
            "\n".join(cli_sources),
            (repo_root / RESOURCE_AUTHORITIES).read_text(encoding="utf-8"),
        )
    )
    for error in volatile_runtime_host_surface_violations(
        (repo_root / RUNTIME_HOST_BUILD).read_text(encoding="utf-8")
    ):
        errors.append(f"{RUNTIME_HOST_BUILD}: {error}")
    non_product_sources = {
        path: (repo_root / path).read_text(encoding="utf-8")
        for _, path, _, _ in NON_PRODUCT_APIS
    }
    for error in non_product_surface_violations(non_product_sources):
        errors.append(f"Non-product surface: {error}")
    for error in product_dispatch_fallback_violations(
        (repo_root / RUNTIME_AUTHORITY_SOURCE).read_text(encoding="utf-8")
    ):
        errors.append(f"{RUNTIME_AUTHORITY_SOURCE}: {error}")
    for error in runtime_commit_authority_violations(
        (repo_root / RUNTIME_STORE_SOURCE).read_text(encoding="utf-8")
    ):
        errors.append(f"{RUNTIME_STORE_SOURCE}: {error}")
    for error in redundant_admin_store_reexport_violations(
        (repo_root / ADMIN_CONFIG_SOURCE).read_text(encoding="utf-8")
    ):
        errors.append(f"{ADMIN_CONFIG_SOURCE}: {error}")
    for error in webhook_mutation_authority_violations(
        (repo_root / CONFIG_RESOLVER_STORES_SOURCE).read_text(encoding="utf-8")
    ):
        errors.append(f"{CONFIG_RESOLVER_STORES_SOURCE}: {error}")
    for error in redundant_runtime_memory_reexport_violations(
        (repo_root / RUNTIME_LIB_SOURCE).read_text(encoding="utf-8")
    ):
        errors.append(f"{RUNTIME_LIB_SOURCE}: {error}")
    for error in coordinator_persistence_ownership_violations("\n".join(cli_sources)):
        errors.append(f"awaken-cli persistence ownership: {error}")
    for error in model_inventory_authority_violations(
        (repo_root / EXECUTABLE_AGENT_CONTRACT_SOURCE).read_text(encoding="utf-8"),
        (repo_root / COORDINATOR_SOURCE).read_text(encoding="utf-8"),
        (repo_root / PROTOCOL_MODEL_SOURCE).read_text(encoding="utf-8"),
        (repo_root / RUNTIME_PROCESS_ROUTER).read_text(encoding="utf-8"),
    ):
        errors.append(f"Coordinator model projection: {error}")
    coordinator_production = "\n".join(
        re.split(r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*]", source, maxsplit=1)[0]
        for path in sorted((repo_root / "crates/server/awaken-coordinator/src").rglob("*.rs"))
        if "tests" not in path.stem
        for source in [path.read_text(encoding="utf-8")]
    )
    for error in inference_materializer_authority_violations(
        (repo_root / CREDENTIAL_INFERENCE_SOURCE).read_text(encoding="utf-8"),
        coordinator_production,
    ):
        errors.append(f"Inference materialization: {error}")
    for error in control_publication_authority_violations(
        (repo_root / CONTROL_MODEL_PUBLICATION).read_text(encoding="utf-8"),
        coordinator_production,
    ):
        errors.append(f"Control model publication: {error}")
    for error in credential_refresh_authority_violations(
        "\n".join(
            (repo_root / source).read_text(encoding="utf-8")
            for source in CREDENTIAL_REFRESH_SOURCES
        ),
        coordinator_production,
    ):
        errors.append(f"Credential refresh: {error}")
    for error in coordinator_control_authority_dependency_violations(
        product_manifests[COORDINATOR_MANIFEST]
    ):
        errors.append(f"{COORDINATOR_MANIFEST}: {error}")
    return errors
