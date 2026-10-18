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

import _crate_boundary_workspace
import _service_lifecycle_fitness


WORKER_SOURCE = "crates/bin/awaken-worker/src"
CONTROL_SOURCE = "crates/control/awaken-control/src"
CLI_SOURCE = "crates/bin/awaken-cli/src"
CLI_LIB_SOURCE = "crates/bin/awaken-cli/src/lib.rs"
COORDINATOR_COMPONENT = "crates/server/awaken-coordinator/src/coordinator_component.rs"
SERVICE_BOUNDARY = "crates/bin/awaken-cli/src/config/service_boundary.rs"
RESOURCE_AUTHORITIES = "crates/resources/awaken-resource-application/src/authorities.rs"
RESOURCE_CONTRACT = "crates/contract/awaken-resource-contract/src/lib.rs"
RESOURCE_PERSISTENCE = "crates/resources/awaken-resource-persistence/src/lib.rs"
RUNTIME_HOST_BUILD = "crates/server/awaken-session-executor/src/host/build.rs"
# The Host is built from one `SessionExecutorBuilder` value whose constructors and
# `with_*` selections live in this module tree, over the capability groups,
# so the construction surface is build.rs plus these two places.
RUNTIME_HOST_DEPENDENCIES = "crates/server/awaken-session-executor/src/host/dependencies"
RUNTIME_HOST_CAPABILITIES = "crates/server/awaken-session-executor/src/host/capability"
PROCESS_STORES = "crates/bin/awaken-cli/src/process_stores.rs"
RUNTIME_HOST_MANIFEST = "crates/server/awaken-session-executor/Cargo.toml"
COORDINATOR_MANIFEST = "crates/server/awaken-coordinator/Cargo.toml"
PROTOCOL_MANAGED_MANIFEST = "crates/server/awaken-protocol-managed/Cargo.toml"
FILE_STORE_SOURCE = "crates/resources/awaken-file-store/src/lib.rs"
COORDINATOR_SOURCE = "crates/server/awaken-coordinator/src/lib.rs"
MEMORY_STORE_SOURCE = "crates/resources/awaken-memory-store/src/repository.rs"
MEMORY_SQLITE_SOURCE = "crates/resources/awaken-memory-store/src/sqlite.rs"
SKILL_STORE_SOURCE = "crates/resources/awaken-skill-store/src/lib.rs"
SKILL_SQLITE_SOURCE = "crates/resources/awaken-skill-store/src/sqlite.rs"
RESOURCE_STORE_SOURCE = "crates/stores/awaken-resource-store/src/lib.rs"
RESOURCE_POSTGRES_REGISTRY = "crates/stores/awaken-resource-store/src/postgres_registry.rs"
RUNTIME_MEMORY_STORES = "crates/server/awaken-session-executor/src/memory_stores.rs"
ENV_STORE_SQLITE_SOURCE = "crates/stores/awaken-env-store/src/lib.rs"
SANDBOX_POLICY_STORE_SOURCE = "crates/server/awaken-sandbox-policy-store/src/lib.rs"
ENV_IMAGE_BUILD_SOURCE = "crates/server/awaken-environment-image-build/src/lib.rs"
ENV_IMAGE_BUILD_SQLITE_SOURCE = "crates/server/awaken-environment-image-build/src/sqlite.rs"
SESSION_STORE_SQLITE_SOURCE = "crates/stores/awaken-session-store/src/sqlite.rs"
WORK_STORE_SOURCE = "crates/stores/awaken-work-store/src/lib.rs"
WORK_STORE_BACKENDS_SOURCE = "crates/stores/awaken-work-store/src/backends.rs"
COMMIT_SQLITE_SOURCE = "crates/stores/awaken-store-sqlite/src/lib.rs"
FILE_SQLITE_SOURCE = "crates/resources/awaken-file-store/src/sqlite.rs"
WORKER_REGISTRY_SQLITE_SOURCE = "crates/server/awaken-worker-registry/src/sqlite.rs"
WORKER_REGISTRY_LIB_SOURCE = "crates/server/awaken-worker-registry/src/lib.rs"
DREAM_APPLICATION_SOURCE = "crates/server/awaken-dream-application/src/lib.rs"
TOOL_RELAY_SOURCE = "crates/worker/awaken-tool-relay/src/lib.rs"
RUN_INGRESS_FACTORY_SOURCE = "crates/server/awaken-dispatch/src/any_factory.rs"
RUN_INGRESS_LIB_SOURCE = "crates/server/awaken-dispatch/src/lib.rs"
RUN_INGRESS_SQLITE_SOURCE = "crates/server/awaken-dispatch/src/sqlite.rs"
RUNTIME_AUTHORITY_SOURCE = "crates/server/awaken-coordinator/src/runtime_authority.rs"
RUNTIME_STORE_SOURCE = "crates/server/awaken-session-executor/src/store.rs"
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
CREDENTIAL_IN_MEMORY_REPO_SOURCE = (
    "crates/control/awaken-credential-vault/src/repo/in_memory.rs"
)
CREDENTIAL_VAULT_SOURCE = "crates/control/awaken-credential-vault/src/lib.rs"
CREDENTIAL_SQLITE_SOURCE = "crates/stores/awaken-credential-store/src/sqlite.rs"
CREDENTIAL_SEALED_SOURCE = "crates/stores/awaken-credential-store/src/sealed.rs"
WEBHOOK_DISPATCH_SOURCE = "crates/server/awaken-webhook/src/dispatch.rs"
WEBHOOK_CONTROL_PLANE_SOURCE = "crates/server/awaken-webhook/src/lifecycle.rs"
WEBHOOK_AUTHORING_SOURCE = "crates/control/awaken-control/src/webhook_authoring.rs"
EXECUTABLE_AGENT_CONTRACT_SOURCE = (
    "crates/contract/awaken-executable-agent-contract/src/lib.rs"
)
EXECUTABLE_AGENT_CATALOG_SOURCE = "crates/server/awaken-executable-agent-catalog/src/lib.rs"
PROTOCOL_MODEL_SOURCE = "crates/server/awaken-protocol-managed/src/control/models.rs"
RUNTIME_PROCESS_ROUTER = "crates/bin/awaken-cli/src/runtime_process_router.rs"
CREDENTIAL_INFERENCE_SOURCE = (
    "crates/server/awaken-credential-materializer/src/inference.rs"
)
CONTROL_MODEL_PUBLICATION = "crates/control/awaken-control/src/model_publication.rs"
CONTROL_CREDENTIAL_REFRESH_SOURCE = "crates/control/awaken-control/src/credential_refresh.rs"

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
    r"MemoryRepository|MemoryQueries|MemorySnapshotSource|MemoryWriter|MemoryFilesystem|MemoryCopyWriteback|MemoryCopyConflictStore|MemoryCopyConflictQueries|MemoryCopyConflictPreservation|MemoryVersionQueries|MemoryVersionRedaction|MemoryContentPurge|SkillStore|CatalogRepo|CredentialRepo)\b|"
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
    "ephemeral",
    "ephemeral_with_deployment",
    "for_local_resources",
    "with_skill_store",
    "with_skill_store_backend",
    "with_skill_store_ports",
    "with_store_dir",
    "with_durable_store_dir",
    "with_file_content_source",
)

TEST_SUPPORT_GATE = (
    r'#\s*\[\s*cfg\s*\(\s*any\s*\(\s*test\s*,\s*feature\s*=\s*"test-support"\s*\)\s*\)\s*\]'
)
# The crate's own unit tests only: stricter than test-support, equally
# unreachable from a product build.
TEST_ONLY_GATE = r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]"
NON_PRODUCT_GATE = rf"(?:{TEST_SUPPORT_GATE}|{TEST_ONLY_GATE})"
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteResourceStore::in_memory",
        RESOURCE_STORE_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "awaken_session_executor::memory_stores::open",
        RUNTIME_MEMORY_STORES,
        r"\bpub\s*\(\s*crate\s*\)\s+fn\s+open\b",
        NON_PRODUCT_GATE,
    ),
    (
        "awaken_resource_persistence::ephemeral",
        RESOURCE_PERSISTENCE,
        r"\bpub\s+(?:async\s+)?fn\s+ephemeral\b",
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory_environment_image_build_store\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteManagedSessionRepository::open_in_memory",
        SESSION_STORE_SQLITE_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        WORK_STORE_BACKENDS_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteCommitCoordinator::open_in_memory",
        COMMIT_SQLITE_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteFileStore::open_in_memory",
        FILE_SQLITE_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteWorkerDirectory::open_in_memory",
        WORKER_REGISTRY_SQLITE_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        "open_sqlite_dispatch_store_in_memory",
        RUN_INGRESS_FACTORY_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_sqlite_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "awaken_dispatch::memory",
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteConfigStore::open_in_memory",
        CONFIG_STORE_SQLITE_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "SqliteAdminStore::open_in_memory",
        ADMIN_CONFIG_SQLITE_SOURCE,
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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
        CREDENTIAL_IN_MEMORY_REPO_SOURCE,
        r"\bpub\s+struct\s+InMemoryCredentialRepo\b",
        TEST_SUPPORT_GATE,
    ),
    (
        "InMemoryCredentialRepo re-export",
        CREDENTIAL_REPO_SOURCE,
        r"\bpub\s+use\s+in_memory::InMemoryCredentialRepo\b",
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
        WEBHOOK_AUTHORING_SOURCE,
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
        r"\bpub\s+(?:async\s+)?fn\s+open_in_memory\b",
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


def runtime_host_construction_source(repo_root: Path) -> str:
    """build.rs, the dependency set and every capability-group module: the Host's whole construction surface."""

    paths = [
        repo_root / RUNTIME_HOST_BUILD,
        repo_root / f"{RUNTIME_HOST_DEPENDENCIES}.rs",
        *sorted((repo_root / RUNTIME_HOST_DEPENDENCIES).rglob("*.rs")),
        *sorted((repo_root / RUNTIME_HOST_CAPABILITIES).rglob("*.rs")),
    ]
    return "\n".join(path.read_text(encoding="utf-8") for path in paths)


def volatile_runtime_host_surface_violations(source: str) -> list[str]:
    """Return volatile Host APIs that are reachable without test-support."""

    errors: list[str] = []
    for name in VOLATILE_RUNTIME_HOST_APIS:
        gated = _declaration_is_gated(
            source, rf"\bpub\s+(?:async\s+)?fn\s+{re.escape(name)}\b", NON_PRODUCT_GATE
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


def _direct_rust_items(syntax: str):
    """Read complete items in this scope without entering nested bodies."""
    cursor = 0
    while cursor < len(syntax):
        if syntax[cursor].isspace():
            cursor += 1
            continue
        attribute_end = _crate_boundary_workspace._outer_attribute_end(syntax, cursor)
        if attribute_end is not None:
            cursor = attribute_end
            continue
        end = _crate_boundary_workspace._rust_item_end(syntax, cursor)
        if end is None:
            return
        yield syntax[cursor:end]
        cursor = end


def resource_authorities_violations(source: str) -> list[str]:
    """Require the public Resources owner and its own public constructor.

    Constructor generics and whitespace do not change ownership. Comments,
    literals, nested functions and constructors on a different type cannot
    supply this application boundary.
    """
    syntax = _crate_boundary_workspace.rust_syntax(
        _crate_boundary_workspace.production_rust(source)
    )
    has_struct = has_impl = has_constructor = False
    for item in _direct_rust_items(syntax):
        has_struct |= re.match(r"pub\s+struct\s+ResourceAuthorities\b", item) is not None
        opening = _crate_boundary_workspace._first_expression_brace(item, 0)
        if opening is None or re.fullmatch(
            r"impl\s+ResourceAuthorities\s*(?:where\b.*?)?", item[:opening], re.S
        ) is None:
            continue
        has_impl = True
        for method in _direct_rust_items(item[opening + 1 : -1]):
            if re.match(
                r"pub\s+(?:(?:const|async|unsafe)\s+)*fn\s+new\s*(?:<|\()", method
            ) is not None and _crate_boundary_workspace._first_expression_brace(method, 0) is not None:
                has_constructor = True
    required = (
        (has_struct, "public ResourceAuthorities struct"),
        (has_impl, "inherent ResourceAuthorities impl"),
        (has_constructor, "public ResourceAuthorities::new constructor"),
    )
    return [
        f"awaken-resource-application authorities are missing `{name}`"
        for present, name in required if not present
    ]


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
    errors.extend(resource_authorities_violations(resource_source))
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
    for session_port in ("SessionRepository", "SessionStore", "LifecycleOutbox"):
        if re.search(rf"\b{session_port}\b", control_source):
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
    # The public surface may carry more than one merge: naming a single exact
    # chain would make every added public read an edit to this checker rather
    # than a decision about the partition. What the partition actually requires
    # is that the durable operations stay on it and the Worker transport never
    # joins it, so both are asserted against the chain itself.
    merge_call = r"\.merge\((?:[^()]|\((?:[^()]|\([^()]*\))*\))*\)"
    public_match = re.search(
        rf"let router = a2a\s*((?:\s*{merge_call})*);(.*?)"
        r"Ok\(\(\s*managed,\s*router,\s*application,\s*worker_transport,",
        coordinator_source,
        re.S,
    )
    if managed_match is None or application_match is None or public_match is None:
        errors.append("Coordinator Managed/public route partition is missing")
    elif "durable_ops" not in public_match.group(1):
        errors.append("Coordinator public router no longer merges the durable operations")
    elif "worker_transport" in public_match.group(1) or ".merge(worker_transport)" in public_match.group(2):
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
        "SessionRepository",
        "SessionStore",
        "LifecycleOutbox",
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
        rf"(?:awaken_dispatch::)?open_sqlite_dispatch_store_in_memory\(\).*?"
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
    catalog_source: str,
    coordinator_source: str,
    protocol_source: str,
    startup_source: str,
) -> list[str]:
    """Keep one model-reference rule and factual first-time projection."""

    errors: list[str] = []
    if contract_source.count("pub async fn current_model_references(") != 1:
        errors.append(
            "executable Agent contract must own exactly one current-model rule"
        )
    if contract_source.count("pub fn project_current_model_publications") != 1:
        errors.append("executable Agent contract must own one model publication projection")
    reference_owner = contract_source.split("pub async fn current_model_references(", 1)[-1]
    if not re.search(r"registrations\s*\.\s*current_model_publications\s*\(", reference_owner):
        errors.append("current model references bypass the publication projection")
    if "project_current_model_publications(" not in catalog_source:
        errors.append("executable Agent catalog bypasses the one model publication projector")
    if "current_model_references(" not in coordinator_source:
        errors.append("Coordinator Dream readiness bypasses the executable Agent inventory rule")
    if not re.search(r"registrations\s*\.\s*current_model_publications\s*\(", protocol_source):
        errors.append("Managed model projection bypasses the executable Agent inventory rule")
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
        if re.search(
            rf"(?<![A-Za-z0-9_]){re.escape(forbidden)}(?![A-Za-z0-9_])",
            coordinator_source,
        ):
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
    materializer_source: str, control_source: str, coordinator_source: str
) -> list[str]:
    """One neutral factory port, one Control-owned Vault implementation."""

    errors: list[str] = []
    if materializer_source.count("trait CredentialRefreshFactory") != 1:
        errors.append("Credential materializer must own one neutral `trait CredentialRefreshFactory`")
    if "trait CredentialRefreshFactory" in control_source:
        errors.append("Control duplicates the neutral credential refresh factory port")
    for required in ("struct VaultRefreshFactory", "struct VaultRefresher"):
        if control_source.count(required) != 1:
            errors.append(f"Control Vault composition must own one `{required}`")
        if required in materializer_source:
            errors.append(f"Credential materializer retains Control implementation `{required}`")
    for forbidden in (
        "trait CredentialRefreshFactory",
        "struct VaultRefreshFactory",
        "struct VaultRefresher",
    ):
        if forbidden in coordinator_source:
            errors.append(f"Coordinator recreates credential refresh authority `{forbidden}`")
    return errors


def selftest() -> None:
    """Run the complete ownership decision table through its dedicated module."""
    from _service_data_ownership_selftest import selftest as ownership_selftest

    ownership_selftest()


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
            f"{_service_lifecycle_fitness.WORKER_MANIFEST}: Worker transitively links "
            f"authority-store dependency `{package}`; "
            "use the existing claim-fenced boundary adapter"
        )
    runtime_host_manifest = product_manifests[RUNTIME_HOST_MANIFEST]
    for error in runtime_host_feature_violations(runtime_host_manifest):
        errors.append(f"{RUNTIME_HOST_MANIFEST}: {error}")
    try:
        runtime_dependencies = resolved_product_dependencies(
            repo_root, "awaken-session-executor", "default Runtime Host"
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
    errors.extend(
        _service_lifecycle_fitness.check_all(
            repo_root,
            product_manifests[_service_lifecycle_fitness.CLI_MANIFEST],
            product_manifests[_service_lifecycle_fitness.WORKER_MANIFEST],
        )
    )

    source_root = repo_root / WORKER_SOURCE
    for path in sorted(source_root.rglob("*.rs")):
        source = path.read_text(encoding="utf-8")
        source = _crate_boundary_workspace.production_rust(source)
        for token in source_violations(source):
            errors.append(
                f"{path.relative_to(repo_root)}: Worker production code acquires "
                f"authority-store vocabulary `{token}`"
            )

    control_root = repo_root / CONTROL_SOURCE
    for path in sorted(control_root.rglob("*.rs")):
        source = path.read_text(encoding="utf-8")
        source = _crate_boundary_workspace.production_rust(source)
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
        source = _crate_boundary_workspace.production_rust(raw_source)
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
            runtime_host_construction_source(repo_root),
            "\n".join(
                path.read_text(encoding="utf-8")
                for path in sorted((repo_root / WORKER_SOURCE).rglob("*.rs"))
            ),
        )
    )
    errors.extend(
        coordinator_resource_access_violations(
            product_manifests[COORDINATOR_MANIFEST],
            product_manifests[_service_lifecycle_fitness.CLI_MANIFEST],
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
        runtime_host_construction_source(repo_root)
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
        (repo_root / EXECUTABLE_AGENT_CATALOG_SOURCE).read_text(encoding="utf-8"),
        (repo_root / COORDINATOR_SOURCE).read_text(encoding="utf-8"),
        (repo_root / PROTOCOL_MODEL_SOURCE).read_text(encoding="utf-8"),
        (repo_root / RUNTIME_PROCESS_ROUTER).read_text(encoding="utf-8"),
    ):
        errors.append(f"Coordinator model projection: {error}")
    coordinator_production = "\n".join(
        _crate_boundary_workspace.production_rust(source)
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
            _crate_boundary_workspace.production_rust(path.read_text(encoding="utf-8"))
            for path in sorted((repo_root / CREDENTIAL_INFERENCE_SOURCE).parent.rglob("*.rs"))
            # This directory is reached only through `#[cfg(test)] mod tests`
            # from lib.rs; production modules are discovered by the tree walk.
            if "tests" not in path.relative_to((repo_root / CREDENTIAL_INFERENCE_SOURCE).parent).parts
            and not path.stem.endswith("_tests")
        ),
        (repo_root / CONTROL_CREDENTIAL_REFRESH_SOURCE).read_text(encoding="utf-8"),
        coordinator_production,
    ):
        errors.append(f"Credential refresh: {error}")
    for error in coordinator_control_authority_dependency_violations(
        product_manifests[COORDINATOR_MANIFEST]
    ):
        errors.append(f"{COORDINATOR_MANIFEST}: {error}")
    return errors
