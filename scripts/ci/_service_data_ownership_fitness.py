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
COORDINATOR_COMPONENT = "crates/server/awaken-coordinator/src/coordinator_component.rs"
RESOURCE_COMPONENT = "crates/contract/awaken-resource-contract/src/component.rs"
RUNTIME_HOST_BUILD = "crates/server/awaken-runtime-host/src/host/build.rs"
PROCESS_STORES = "crates/bin/awaken-cli/src/process_stores.rs"
RUNTIME_HOST_MANIFEST = "crates/server/awaken-runtime-host/Cargo.toml"
COORDINATOR_MANIFEST = "crates/server/awaken-coordinator/Cargo.toml"
CLI_MANIFEST = "crates/bin/awaken-cli/Cargo.toml"
FILE_STORE_SOURCE = "crates/resources/awaken-file-store/src/lib.rs"
COORDINATOR_SOURCE = "crates/server/awaken-coordinator/src/lib.rs"
MEMORY_STORE_SOURCE = "crates/resources/awaken-memory-store/src/repository.rs"
MEMORY_SQLITE_SOURCE = "crates/resources/awaken-memory-store/src/sqlite.rs"
SKILL_STORE_SOURCE = "crates/resources/awaken-skill-store/src/lib.rs"
SKILL_SQLITE_SOURCE = "crates/resources/awaken-skill-store/src/sqlite.rs"
RESOURCE_STORE_SOURCE = "crates/stores/awaken-resource-store/src/lib.rs"
RUNTIME_MEMORY_STORES = "crates/server/awaken-runtime-host/src/memory_stores.rs"
ENV_STORE_SQLITE_SOURCE = "crates/stores/awaken-env-store/src/lib.rs"
SANDBOX_POLICY_STORE_SOURCE = "crates/server/awaken-sandbox-policy-store/src/lib.rs"
ENV_IMAGE_BUILD_SOURCE = "crates/server/awaken-environment-image-build/src/lib.rs"
ENV_IMAGE_BUILD_SQLITE_SOURCE = "crates/server/awaken-environment-image-build/src/sqlite.rs"

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

VOLATILE_RUNTIME_HOST_APIS = (
    "new",
    "new_with_deployment",
    "with_deployment_config",
    "new_with_resource_component",
    "with_resource_lifecycle",
    "with_upstream",
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
PUBLIC_DECLARATION = r"\bpub(?:\s*\(\s*crate\s*\))?\s+(?:async\s+)?(?:struct|fn|use)\s+"

NON_PRODUCT_APIS = (
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
        "ephemeral_resources_application",
        COORDINATOR_SOURCE,
        r"\bpub\s+fn\s+ephemeral_resources_application\b",
        FEATURE_TEST_SUPPORT_GATE,
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
)

PRODUCT_MANIFESTS = (
    WORKER_MANIFEST,
    RUNTIME_HOST_MANIFEST,
    COORDINATOR_MANIFEST,
    CLI_MANIFEST,
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


def _declaration_is_gated(
    source: str, declaration_pattern: str, gate_pattern: str
) -> bool | None:
    """Whether the declaration exists and its own attribute prefix has the gate."""

    declaration = re.search(declaration_pattern, source)
    if declaration is None:
        return None
    previous_start = max(
        (
            item.start()
            for item in re.finditer(PUBLIC_DECLARATION, source)
            if item.start() < declaration.start()
        ),
        default=0,
    )
    return re.search(gate_pattern, source[previous_start : declaration.start()]) is not None


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
    """Enforce one Control application builder and process-only composition.

    AllInOne and standalone Control may each call the CLI adapter helper, but
    that helper must have exactly one call into the authoritative domain builder.
    Coordinator/runtime assembly must never reconstruct ConfigService or invoke
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
    if "assemble_runtime_process_router(" in control_process_source:
        errors.append("standalone Control delegates to the runtime process assembler")
    if "assemble_control_process_router(" not in control_process_source:
        errors.append("standalone Control does not use its dedicated process assembler")
    if "ephemeral_resource_component(" in control_process_source:
        errors.append("standalone Control constructs the Resources component")
    return errors


def domain_component_violations(
    cli_source: str,
    control_source: str,
    coordinator_source: str,
    resource_source: str,
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
        "DeploymentState::with_repository(",
        "mount_with_managed_application_access_models_and_dreams(",
        "ResourcePlane::new(",
    ):
        if forbidden in cli_source:
            errors.append(f"awaken-cli reconstructs a domain component through `{forbidden}`")
    for required in (
        "pub async fn build_coordinator_component(",
        "DeploymentState::with_repository(",
        "mount_with_managed_application_access_models_and_dreams(",
    ):
        if required not in coordinator_source:
            errors.append(f"awaken-coordinator Coordinator component is missing `{required}`")
    for required in (
        "pub fn build_resource_component(",
        "pub struct ResourceDependencies",
        "pub struct ResourceComponent",
    ):
        if required not in resource_source:
            errors.append(f"awaken-resource-contract component is missing `{required}`")
    if "ResourcePlane" in runtime_host_source:
        errors.append("Runtime Host retains the retired ResourcePlane component owner")
    if "pub struct WorkerNodeBuilder" not in worker_source:
        errors.append("awaken-worker lost its canonical WorkerNodeBuilder component boundary")
    if "build_worker_component" in worker_source:
        errors.append("awaken-worker added a second component builder beside WorkerNodeBuilder")
    if "ManagedSessionRepository" in control_source:
        errors.append("awaken-control still acquires Coordinator's Session repository")
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
        "ResourceComponent",
        "ResourceCatalog",
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
        "let resource_component = if manifest.contains(&MigrationComponent::Resources)",
    ):
        if required not in cli_source:
            errors.append(f"role-aware store assembly is missing `{required}`")
    for required in (
        "pub resource_catalog: Arc<dyn ResourceCatalog>",
        "pub fn resource_catalog(&self) -> Arc<dyn ResourceCatalog>",
    ):
        if required not in resource_source:
            errors.append(f"Resources component does not own `{required}`")
    return errors


def _normal_dependencies(manifest: dict) -> set[str]:
    dependencies: set[str] = set()
    for section in ("dependencies", "build-dependencies"):
        for name, value in manifest.get(section, {}).items():
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
    for section in ("dependencies", "build-dependencies"):
        for name, value in manifest.get(section, {}).items():
            if isinstance(value, dict) and "test-support" in value.get("features", []):
                errors.append(f"{section} dependency `{name}` enables test-support")
    return sorted(errors)


def selftest() -> None:
    """Cause/effect decision table.

    O1 neutral Worker ports/adapters -> accepted; O2 a direct authority-store
    dependency (including a Cargo alias) -> rejected; O3 an authority-store
    constructor reached through a transitive dependency -> rejected; O4 ordinary
    HTTP adapter construction -> accepted. O5 Control creates Managed Execution
    -> rejected; O6 Control-only construction -> accepted; O7 retired launch path
    -> rejected; O8 one Control builder -> accepted; O9 duplicate/wrong Control
    path -> rejected; O10 the four canonical component owners -> accepted; O11
    cross-owner or parallel component construction -> rejected; O12 standalone
    Control constructs Resources -> rejected; O13 grouped role-owned stores and
    Resources catalog -> accepted; O14 a cross-domain store field or unconditional
    migration acquisition -> rejected; O15 every volatile Host API is test-support
    gated -> accepted; O16 one missing Host gate -> rejected; O17 all canonical
    File/Memory/Skill/Resource/Environment volatile entrypoints and the ephemeral
    Resources assembler are test-support gated -> accepted; O18 any one gate missing ->
    rejected; O19 product defaults/normal edges do not enable test-support ->
    accepted; O20 a product default or normal edge enables it -> rejected while
    dev-dependencies and opt-in features remain accepted. Together the rules cover
    compile-time acquisition, production call paths, component ownership, and
    schema acquisition.
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
    component = (
        "pub async fn build_control_component("
        " ConfigService::new( control_router(ControlRouterInput"
    )
    assert control_component_violations(
        "awaken_control::build_control_component(",
        component,
        "assemble_control_process_router(",
    ) == []  # O8 one canonical builder
    assert control_component_violations(
        "ConfigService::new( awaken_control::build_control_component( ",
        component,
        "assemble_runtime_process_router(",
    )  # O9 duplicate CLI construction and wrong standalone path
    assert control_component_violations(
        "awaken_control::build_control_component(",
        component,
        "assemble_control_process_router( ephemeral_resource_component(",
    )  # O12 Control must consume Resource ports without constructing Resources
    coordinator = (
        "pub async fn build_coordinator_component("
        " DeploymentState::with_repository("
        " mount_with_managed_application_access_models_and_dreams("
    )
    resources = (
        "pub fn build_resource_component("
        " pub struct ResourceDependencies pub struct ResourceComponent"
    )
    assert domain_component_violations(
        "awaken_coordinator::build_coordinator_component(",
        "",
        coordinator,
        resources,
        "",
        "pub struct WorkerNodeBuilder",
    ) == []  # O10 four canonical component owners
    assert domain_component_violations(
        "DeploymentState::with_repository(",
        "ManagedSessionRepository",
        "",
        "",
        "ResourcePlane",
        "pub struct WorkerNodeBuilder build_worker_component",
    )  # O11 parallel or cross-owner component construction
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
        "let resource_component = if manifest.contains(&MigrationComponent::Resources)"
    )
    resource_owner = (
        "pub resource_catalog: Arc<dyn ResourceCatalog> "
        "pub fn resource_catalog(&self) -> Arc<dyn ResourceCatalog>"
    )
    assert process_store_ownership_violations(
        process_stores, role_aware, resource_owner
    ) == []  # O13
    assert process_store_ownership_violations(
        process_stores.replace("catalog: CatalogRepo", "sessions: ManagedSessionRepository"),
        role_aware.replace(
            "let resource_component = if manifest.contains(&MigrationComponent::Resources)", ""
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
        FILE_STORE_SOURCE: any_gate + "pub struct InMemoryFileStore {}",
        MEMORY_STORE_SOURCE: any_gate + "pub struct VolatileMemoryRepository {}",
        MEMORY_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        SKILL_STORE_SOURCE: any_gate + "pub struct InMemorySkillStore {}",
        SKILL_SQLITE_SOURCE: any_gate + "pub fn open_in_memory() {}",
        RESOURCE_STORE_SOURCE: any_gate + "pub fn in_memory() {}",
        RUNTIME_MEMORY_STORES: any_gate + "pub(crate) fn open() {}",
        COORDINATOR_SOURCE: feature_gate
        + "pub fn ephemeral_resources_application() {}",
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


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    product_manifests: dict[str, dict] = {}
    for product_manifest in PRODUCT_MANIFESTS:
        with (repo_root / product_manifest).open("rb") as handle:
            product_manifests[product_manifest] = tomllib.load(handle)
        for error in product_test_support_violations(product_manifests[product_manifest]):
            errors.append(f"{product_manifest}: {error}")
    dependencies = _normal_dependencies(product_manifests[WORKER_MANIFEST])
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
            (repo_root / RESOURCE_COMPONENT).read_text(encoding="utf-8"),
            (repo_root / RUNTIME_HOST_BUILD).read_text(encoding="utf-8"),
            "\n".join(
                path.read_text(encoding="utf-8")
                for path in sorted((repo_root / WORKER_SOURCE).rglob("*.rs"))
            ),
        )
    )
    errors.extend(
        process_store_ownership_violations(
            (repo_root / PROCESS_STORES).read_text(encoding="utf-8"),
            "\n".join(cli_sources),
            (repo_root / RESOURCE_COMPONENT).read_text(encoding="utf-8"),
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
    return errors
