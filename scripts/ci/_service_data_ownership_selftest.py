"""Complete decision-table selftest for service data ownership rules."""

from __future__ import annotations

from _service_data_ownership_fitness import (
    ADMIN_CONFIG_SQLITE_SOURCE,
    CAPTURE_SQLITE_SOURCE,
    CAPTURE_STORE_SOURCE,
    CLI_LIB_SOURCE,
    CLI_COMPOSITION_LIB_SOURCE,
    SCENARIO_PROCESS_SOURCE,
    COMMIT_SQLITE_SOURCE,
    CONFIG_RESOLVER_SOURCE,
    CONFIG_STORE_SQLITE_SOURCE,
    COORDINATOR_SOURCE,
    CREDENTIAL_IN_MEMORY_REPO_SOURCE,
    CREDENTIAL_REPO_SOURCE,
    CREDENTIAL_SEALED_SOURCE,
    CREDENTIAL_SQLITE_SOURCE,
    CREDENTIAL_VAULT_SOURCE,
    DATA_SUBJECT_SOURCE,
    DATA_SUBJECT_SQLITE_SOURCE,
    DREAM_APPLICATION_SOURCE,
    ENV_IMAGE_BUILD_SOURCE,
    ENV_IMAGE_BUILD_SQLITE_SOURCE,
    ENV_STORE_SQLITE_SOURCE,
    FILE_SQLITE_SOURCE,
    FILE_STORE_SOURCE,
    MEMORY_SQLITE_SOURCE,
    MEMORY_STORE_SOURCE,
    MODEL_CATALOG_REPO_SOURCE,
    MODEL_CATALOG_SQLITE_SOURCE,
    NON_PRODUCT_APIS,
    RESOURCE_PERSISTENCE,
    RESOURCE_STORE_SOURCE,
    RUNTIME_COMMIT_SOURCES,
    RUNTIME_HOST_MANIFEST,
    RUNTIME_MEMORY_STORES,
    RUN_INGRESS_FACTORY_SOURCE,
    RUN_INGRESS_LIB_SOURCE,
    RUN_INGRESS_SQLITE_SOURCE,
    SANDBOX_POLICY_STORE_SOURCE,
    SESSION_STORE_SQLITE_SOURCE,
    SKILL_SQLITE_SOURCE,
    SKILL_STORE_SOURCE,
    STORE_FS_MANIFEST,
    TOOL_RELAY_SOURCE,
    VOLATILE_RUNTIME_HOST_APIS,
    WEBHOOK_AUTHORING_SOURCE,
    WEBHOOK_CONTROL_PLANE_SOURCE,
    WEBHOOK_DISPATCH_SOURCE,
    WORKER_REGISTRY_LIB_SOURCE,
    WORKER_REGISTRY_SQLITE_SOURCE,
    WORK_STORE_BACKENDS_SOURCE,
    WORK_STORE_SOURCE,
    _normal_dependencies,
    _service_lifecycle_fitness,
    control_component_violations,
    control_execution_violations,
    control_publication_authority_violations,
    coordinator_control_authority_dependency_violations,
    coordinator_persistence_ownership_violations,
    coordinator_resource_access_violations,
    credential_refresh_authority_violations,
    dependency_closure,
    dependency_violations,
    domain_component_violations,
    inference_materializer_authority_violations,
    model_inventory_authority_violations,
    non_product_surface_violations,
    process_store_ownership_violations,
    product_dispatch_fallback_violations,
    product_inmem_dependency_violations,
    product_test_support_violations,
    re,
    redundant_admin_store_reexport_violations,
    redundant_runtime_memory_reexport_violations,
    resource_authorities_violations,
    resource_registry_authority_violations,
    retired_launch_violations,
    runtime_commit_authority_violations,
    runtime_host_feature_violations,
    source_violations,
    volatile_runtime_host_surface_violations,
    webhook_mutation_authority_violations,
    worker_listener_partition_violations,
)


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
    gated -> accepted; O16 one missing Host gate -> rejected; O16b a
    crate-test-only gate -> accepted; O17 all canonical
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
    detached Coordinator capability -> rejected; O39 one normalized
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

    _service_lifecycle_fitness.selftest()

    assert dependency_violations({"awaken-session-executor", "awaken-runtime-contract"}) == []  # O1
    assert dependency_violations({"awaken-session-store"}) == ["awaken-session-store"]  # O2
    graph = {
        "awaken-worker": {"awaken-session-executor"},
        "awaken-session-executor": {"awaken-session-store"},
        "awaken-session-store": {"sqlx"},
    }
    assert dependency_violations(dependency_closure("awaken-worker", graph)) == [
        "awaken-session-store",
        "sqlx",
    ]  # O3
    assert source_violations("let store = PostgresMemoryRepository::connect(url).await?;")  # O3
    for role in ("MemoryCopyConflictQueries", "MemoryCopyConflictPreservation"):
        assert source_violations(f"let store = Postgres{role}::connect(url).await?;")
        assert source_violations(f"let store = Sqlite{role}::open(path).await?;")
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
    resources = (
        "pub struct ResourceAuthorities; "
        "impl ResourceAuthorities { pub fn new() -> Self { Self } }"
    )
    # Constructor syntax does not transfer application ownership. Exercise the
    # same pure predicate used by the production domain-component scan.
    for constructor in (
        "pub fn new() -> Self { Self }",
        "pub fn new<T>(_: T) -> Self { Self }",
        "pub fn new<T>(_: T) -> Self where T: Send { Self }",
        "pub\nfn\nnew\n<T>\n(_: T)\n-> Self\nwhere T: Send\n{ Self }",
        "#[must_use] pub fn new</* type */ T: Send>(_: T) -> Self { Self }",
        "pub fn new<T: Fn() -> bool>(_: T) -> Self { Self }",
    ):
        assert resource_authorities_violations(
            "pub struct ResourceAuthorities; impl\nResourceAuthorities { " + constructor + " }"
        ) == []
    for impostor in (
        "",
        "// pub struct ResourceAuthorities impl ResourceAuthorities pub fn new() {}",
        'const TEXT: &str = "pub struct ResourceAuthorities impl ResourceAuthorities pub fn new() {}";',
        'const TEXT: &str = r###"pub struct ResourceAuthorities impl ResourceAuthorities pub fn new() {}"###;',
        "pub struct ResourceAuthorities; impl ResourceAuthorities {}",
        "pub struct ResourceAuthorities; impl Other { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities {} impl Other { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { fn new() -> Self { Self } } impl Other { pub fn new() -> Self { Self } }",
        "// pub struct ResourceAuthorities\nimpl ResourceAuthorities { pub fn new() -> Self { Self } }",
        'const NAME: &str = "pub struct ResourceAuthorities"; impl ResourceAuthorities { pub fn new() -> Self { Self } }',
        "pub struct ResourceAuthorities; impl ResourceAuthoritiesExtra { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl Maker for ResourceAuthorities { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { pub(crate) fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { pub(super) fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { pub fn newer() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { pub fn helper() { pub fn new() {} } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { /* pub fn new() -> Self { Self } */ }",
        'pub struct ResourceAuthorities; impl ResourceAuthorities { pub fn helper() { let s = "pub fn new() {}"; } }',
        "pub struct ResourceAuthorities; impl ResourceAuthorities { #[cfg(test)] pub fn new() -> Self { Self } }",
        "#[cfg(test)] pub struct ResourceAuthorities; impl ResourceAuthorities { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; #[cfg(test)] impl ResourceAuthorities { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; macro_rules! fake { () => { impl ResourceAuthorities { pub fn new() -> Self { Self } } } }",
        "pub struct ResourceAuthorities; mod fake { impl ResourceAuthorities { pub fn new() -> Self { Self } } }",
        "struct ResourceAuthorities; impl ResourceAuthorities { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthoritiesExtra; impl ResourceAuthorities { pub fn new() -> Self { Self } }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { pub fn new() -> Self; }",
        "pub struct ResourceAuthorities; impl ResourceAuthorities { pub fn new<T>(_: T) -> Self {",
    ):
        assert resource_authorities_violations(impostor), impostor
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
    for overrides in (
        {"cli_source": "awaken_coordinator::build_coordinator_component( ResourcePlane::new("},
        {"cli_source": "awaken_coordinator::build_coordinator_component( DeploymentApplication::from_repository("},
        {"resource_contract_source": "pub use component::ResourceComponent;"},
        {"resource_contract_source": "pub struct ResourceDependencies;"},
        {"resource_contract_source": "fn build_resource_component() {}"},
        {"runtime_host_source": "ResourcePlane"},
        {"control_source": "Arc<dyn SessionRepository>"},
        {"control_source": "Arc<dyn SessionRepositoryReads>"},
        {"worker_source": "pub struct WorkerNodeBuilder; fn build_worker_component() {}"},
    ):
        inputs = dict(
            cli_source="awaken_coordinator::build_coordinator_component(",
            control_source="", coordinator_source=coordinator, resource_source=resources,
            resource_contract_source="", runtime_host_source="",
            worker_source="pub struct WorkerNodeBuilder",
        )
        inputs.update(overrides)
        assert domain_component_violations(**inputs), overrides
    assert domain_component_violations(
        "DeploymentApplication::from_repository(",
        "Arc<dyn SessionRepository>",
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
        "struct CoordinatorStores { sessions: SessionStore\n}\n"
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
        process_stores.replace("catalog: CatalogRepo", "sessions: SessionStore"),
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
    assert volatile_runtime_host_surface_violations(
        volatile_surface.replace(
            '#[cfg(any(test, feature = "test-support"))]\npub fn with_store_dir',
            "#[cfg(test)]\npub fn with_store_dir",
        )
    ) == []  # O16b: the crate's own unit-test gate is equally non-product
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
                "prepare_all_in_one_process_with_scenario_model",
                "build_all_in_one_router_with_model",
                "build_durable_all_in_one_router",
                "build_secured_all_in_one_router",
            )
        ),
        # O17 also covers the moved SDK-only helpers and their outer module.
        # The removal loop below must reject losing either gate at its owner.
        CLI_COMPOSITION_LIB_SOURCE: feature_gate + "mod scenario_process;",
        SCENARIO_PROCESS_SOURCE: "\n".join(
            feature_gate + f"pub async fn {name}() {{}}"
            for name in (
                "build_all_in_one_service_with_host_customizer",
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
        WORK_STORE_SOURCE: any_gate + "pub use inmem::InMemoryWorkQueue;\n",
        # O17/O18: the backend constructor moved to its only implementation
        # module; the same product-build exclusion must follow it there.
        WORK_STORE_BACKENDS_SOURCE: any_gate + "pub fn open_in_memory() {}",
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
        RUN_INGRESS_FACTORY_SOURCE: any_gate + "pub fn open_sqlite_in_memory() {}",
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
        CREDENTIAL_IN_MEMORY_REPO_SOURCE: any_gate
        + "pub struct InMemoryCredentialRepo {}",
        CREDENTIAL_REPO_SOURCE: any_gate
        + "pub use in_memory::InMemoryCredentialRepo;",
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
        WEBHOOK_CONTROL_PLANE_SOURCE: feature_gate
        + "pub fn loopback_lifecycle_delivery() {}",
        WEBHOOK_AUTHORING_SOURCE: feature_gate
        + "pub fn webhook_config_router_loopback() {}",
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
        "awaken_dispatch::open_sqlite_dispatch_store_in_memory()?; "
        '#[cfg(not(any(test, feature = "test-support")))] '
        'return Err(internal("product SQLite dispatch requires a durable storage_dir")); }'
    )
    assert product_dispatch_fallback_violations(guarded_dispatch) == []  # O21
    assert product_dispatch_fallback_violations(
        "None => awaken_dispatch::open_sqlite_dispatch_store_in_memory()?"
    ) == ["SQLite dispatch missing-storage path does not fail closed"]  # O22
    assert redundant_admin_store_reexport_violations("pub struct AdminState;") == []  # O23
    assert redundant_admin_store_reexport_violations(
        "pub use awaken_config_resolver::{InferenceProfileStore, InMemoryProfileStore};"
    ) == ["Admin API re-exports Config Resolver store contracts or fixtures"]  # O24
    assert runtime_commit_authority_violations("enum SessionCommitBoundary { Local, Remote }") == []  # O25
    assert runtime_commit_authority_violations("enum CommitPlan { Sqlite }")  # O26
    # C7 location table: the Session commit boundary and the migrated neutral
    # delegation must both remain scanned by this exact predicate. Injecting a
    # backend selector into either source must still reject, not gain a waiver.
    assert RUNTIME_COMMIT_SOURCES == (
        "crates/platform/execution/awaken-session-run-delivery/src/commit_boundary.rs",
        "crates/platform/execution/awaken-dispatch-erasure/src/local_commit.rs",
    )
    for source in RUNTIME_COMMIT_SOURCES:
        assert runtime_commit_authority_violations("enum CommitPlan { Sqlite }"), source
        assert runtime_commit_authority_violations("enum SessionCommitBoundary { Local, Remote }") == [], source
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
    # Model inventory FMECA/cause-effect table: FM1 independent Dream/HTTP model
    # normalization diverges; FM2 a Control catalog reader returns beside the
    # executable publication inventory. R41 one publication projector + both
    # consumers + one startup inventory => accept; R42 missing ownership or a
    # retired directory => reject.
    assert model_inventory_authority_violations(
        "pub async fn current_model_references( registrations.current_model_publications( pub fn project_current_model_publications",
        "project_current_model_publications(",
        "current_model_references(",
        "registrations.current_model_publications(",
        "let model_inventory:",
    ) == []  # O41
    assert model_inventory_authority_violations(
        "pub async fn current_model_references( registrations.current_model_publications( pub fn project_current_model_publications",
        "project_current_model_publications(",
        "ModelDirectory current_model_references(",
        "",
        "let model_inventory: let model_inventory:",
    )  # O42 retired path, missing consumer, and duplicate selection
    assert model_inventory_authority_violations(
        "pub async fn current_model_references( registrations.current_model_publications( pub fn project_current_model_publications",
        "",
        "current_model_references(",
        "registrations.current_model_publications(",
        "let model_inventory:",
    ) == ["executable Agent catalog bypasses the one model publication projector"]  # O42a
    assert inference_materializer_authority_violations(
        "struct PinnedCandidateExecutor;",
        "pub mod coordinator_component;",
    ) == []  # O43
    assert inference_materializer_authority_violations(
        "struct PinnedCandidateExecutor; struct PinnedCandidateExecutor;",
        "struct CredentialInferenceMaterializer; impl InferenceExecutorMaterializer for X {}",
    )  # O44
    assert control_publication_authority_violations(
        "struct CatalogModelPublicationResolver;",
        "pub mod coordinator_component;",
    ) == []  # O45
    # Publication-reader cause/effect rule: C1 an exact CatalogRepo or
    # CredentialRepo authority crosses into Coordinator -> reject; C2 a
    # differently named concrete repository used by scenario Vault wiring is
    # not by itself a model-publication reader -> accept. Exact identifier
    # boundaries prevent the newly complete production projection from turning
    # a suffix match into a false architectural owner.
    assert control_publication_authority_violations(
        "struct CatalogModelPublicationResolver;",
        "InMemoryCatalogRepo InMemoryCredentialRepo",
    ) == []  # O45/C2
    assert control_publication_authority_violations(
        "",
        "struct CatalogModelPublicationResolver; CredentialRepo",
    )  # O46
    assert credential_refresh_authority_violations(
        "trait CredentialRefreshFactory;",
        "",
        "struct VaultRefreshFactory; struct VaultRefresher;",
        "pub mod coordinator_component;",
    ) == []  # O47
    assert credential_refresh_authority_violations(
        "trait CredentialRefreshFactory;",
        "",
        "struct VaultRefreshFactory;",
        "struct VaultRefresher;",
    )  # O48
    # Cause C4 the realization keeps its own copy of the contract port; effect
    # E4 refuses the duplicate. Rule O48c=C4=>E4.
    assert credential_refresh_authority_violations(
        "trait CredentialRefreshFactory;",
        "trait CredentialRefreshFactory;",
        "struct VaultRefreshFactory; struct VaultRefresher;",
        "pub mod coordinator_component;",
    )  # O48c
    # Cause C3 a second Vault factory survives beside Control's; effect E3
    # refuses that duplicate even when the required owner also exists.
    # Rule O48b=C3=>E3; O47 has one neutral port and one concrete owner.
    assert credential_refresh_authority_violations(
        "trait CredentialRefreshFactory;",
        "struct VaultRefreshFactory;",
        "struct VaultRefreshFactory; struct VaultRefresher;",
        "pub mod coordinator_component;",
    )  # O48b
    assert coordinator_control_authority_dependency_violations(
        {"dependencies": {"awaken-config-service": {"workspace": True}}}
    ) == []  # O49
    assert coordinator_control_authority_dependency_violations(
        {"dependencies": {"awaken-model-catalog": {"workspace": True}}}
    )  # O50
