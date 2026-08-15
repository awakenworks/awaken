#[tokio::test]
async fn startup_reconciliation_reuses_registration_and_keeps_latest_current() {
    // Registration recovery decision table: R1 newest durable publication is
    // submitted first and becomes current; R2 older publications remain exact
    // history; R3 a source-store failure is returned rather than reported as
    // a successful empty recovery.
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let scope = ScopeId::from("wrkspc_warm");
    let author = plane_over(store.clone());

    // Publish v1, then re-author with new instructions and publish v2 (a
    // distinct content address → a second published row for the same agent).
    let mut v1 = agent_config("warm-agent");
    v1.instructions = "version one".into();
    author.put(&scope, &v1).await.unwrap();
    author.publish(&scope, "warm-agent").await.unwrap();

    let mut v2 = agent_config("warm-agent");
    v2.instructions = "version two".into();
    v2.model_binding = ModelSelection::Target {
        target: awaken_agent_config::ModelTarget {
            model_id: "m-first".into(),
            provider_id: Some("openai".into()),
            api_dialect: Some("open_ai_chat".into()),
            protocol_endpoint_id: None,
            endpoint_name: Some("warm".into()),
        },
        backend_ref: "genai".into(),
        configuration: Default::default(),
    };
    author.put(&scope, &v2).await.unwrap();
    author.publish(&scope, "warm-agent").await.unwrap();

    // A FRESH Coordinator catalog is reached through the same registrar.
    let (cold, catalog) = test_service_and_catalog();
    let n = cold
        .reconcile_registrations(store.as_ref(), &scope)
        .await
        .unwrap();
    assert_eq!(n, 2, "both published rows are read");
    let installed = catalog
        .current(scope.as_str(), "warm-agent")
        .expect("agent hydrated");
    assert_eq!(
        installed.snapshot.resolved_spec.instructions, "version two",
        "the latest publication wins on rehydrate"
    );
    use awaken_executable_agent_contract::ExecutableAgentProfileSource as _;
    let view = catalog
        .session_profile_in(scope.as_str(), "warm-agent")
        .unwrap();
    assert_eq!(
        view.model.as_deref(),
        Some("m-first;provider=openai;api=open_ai_chat;endpoint=warm"),
        "warm install must pair the snapshot with its exact source revision"
    );

    // A store whose list fails returns an error (fail-closed reconciliation).
    let cold2 = test_service();
    assert!(
        cold2
            .reconcile_registrations(&FailingScopedRegistry, &scope)
            .await
            .is_err(),
        "R3"
    );
}

#[tokio::test]
async fn startup_reconciliation_uses_frozen_publication_inputs() {
    // Recovery cause/effect table: F1 publication freezes Resource rev1;
    // F2 mutable draft advances to rev2; F3 a fresh Coordinator reconciles
    // -> the current Session profile still carries rev1's mount, with no
    // fallback read from the mutable Resource repository.
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let resources = Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
    let scope = ScopeId::from("wrkspc_frozen_inputs");
    let inputs = |revision, mount_path: &str| AgentInputConfig {
        agent_id: "frozen-agent".into(),
        environment: None,
        inputs: vec![InputBinding {
            binding_id: BindingId::from("memory"),
            target: InputResourceId::MemoryStore(MemoryStoreId::from("memory-a")),
            mount_path: mount_path.into(),
            access: ResourceAccess::ReadWrite,
            instructions: None,
        }],
        revision,
    };
    resources
        .put_agent_inputs(scope.as_str(), inputs(1, "/published"))
        .unwrap();
    let author_service = test_service().with_resources(resources.clone());
    let author = ConfigPlane::new(
        Arc::new(author_service),
        store.clone(),
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    author
        .put(&scope, &agent_config("frozen-agent"))
        .await
        .unwrap();
    author.publish(&scope, "frozen-agent").await.unwrap();
    resources
        .put_agent_inputs(scope.as_str(), inputs(2, "/draft-v2"))
        .unwrap();

    let (cold, catalog) = test_service_and_catalog();
    cold.reconcile_registrations(store.as_ref(), &scope)
        .await
        .unwrap();
    use awaken_executable_agent_contract::ExecutableAgentProfileSource as _;
    let view = catalog
        .session_profile_in(scope.as_str(), "frozen-agent")
        .expect("F3 frozen publication survives restart");
    assert_eq!(view.resources[0].mount_path, "/published");
}

#[tokio::test]
async fn startup_reconciliation_converges_legacy_duplicate_source_revisions() {
    // Legacy recovery decision table: R1 two immutable fingerprints at one
    // source revision keep the last persisted snapshot; R2 the older row
    // remains durable history; R3 the executable catalog receives one
    // registration, preserving the source-revision uniqueness invariant.
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let scope = ScopeId::from("wrkspc_legacy_duplicate");
    let author = plane_over(store.clone());
    author
        .put(&scope, &agent_config("legacy-agent"))
        .await
        .unwrap();
    let first = author.publish(&scope, "legacy-agent").await.unwrap();

    let alternate = ConfigService::new(Arc::new(FakeProviderResolver), test_registrar());
    let registry = ScopedConfig::new(store.clone(), scope.clone());
    let last = alternate
        .preview_publication(&scope, &registry, "legacy-agent", &[])
        .await
        .unwrap();
    assert_eq!(first.source_revision, last.source_revision, "R1");
    assert_ne!(first.fingerprint, last.fingerprint, "R1");
    store.put_publication_scoped(&scope, &last).await.unwrap();

    let (cold, catalog) = test_service_and_catalog();
    assert_eq!(
        cold.reconcile_registrations(store.as_ref(), &scope)
            .await
            .unwrap(),
        1,
        "R3"
    );
    assert_eq!(
        catalog
            .current(scope.as_str(), "legacy-agent")
            .expect("legacy publication hydrated")
            .snapshot
            .fingerprint
            .0,
        last.fingerprint,
        "R1"
    );
    assert!(
        store
            .get_publication_scoped(&scope, &first.fingerprint)
            .await
            .unwrap()
            .is_some(),
        "R2"
    );
}

#[tokio::test]
async fn startup_reconciliation_quarantines_one_bad_history_without_blocking_good_agents() {
    // Cause/effect graph (FMECA malformed publication S8/O7/D5=280):
    // C1 one durable row references a missing author revision; C2 other rows are
    // valid; C3 Coordinator projection starts empty. R1(C1+C2+C3) -> valid rows
    // are registered, the bad row remains immutable, and recovery returns one
    // degraded aggregate for retry/alerting. It must neither delete history nor
    // fail before later Agents receive their last-known-good projection.
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let scope = ScopeId::from("wrkspc_quarantine");
    let author = plane_over(store.clone());
    for id in ["damaged-agent", "healthy-agent"] {
        author.put(&scope, &agent_config(id)).await.unwrap();
        author.publish(&scope, id).await.unwrap();
    }

    let mut orphan = author
        .latest_publication(&scope, "damaged-agent")
        .await
        .unwrap()
        .unwrap();
    orphan.source_revision = 99;
    orphan.publication_id = "orphan-publication".into();
    orphan.fingerprint = "orphan-publication".into();
    store.put_publication_scoped(&scope, &orphan).await.unwrap();

    let (cold, catalog) = test_service_and_catalog();
    let error = cold
        .reconcile_registrations(store.as_ref(), &scope)
        .await
        .expect_err("R1 reports degraded history");
    assert!(error.contains("1 quarantined"), "R1: {error}");
    assert!(
        catalog.current(scope.as_str(), "healthy-agent").is_some(),
        "R1"
    );
    assert!(
        catalog.current(scope.as_str(), "damaged-agent").is_some(),
        "R1"
    );
    assert!(
        store
            .get_publication_scoped(&scope, "orphan-publication")
            .await
            .unwrap()
            .is_some(),
        "R1"
    );
}

#[tokio::test]
async fn registration_reconciliation_keeps_archived_publications_unavailable() {
    // Lifecycle decision table: R1 current Published + durable publication ->
    // register; R2 current Disabled/Archived + old publication -> withdraw and
    // skip every historical snapshot. Exact history remains a catalog concern.
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let scope = ScopeId::from("wrkspc_archived_warm");
    let author = plane_over(store.clone());
    let mut config = agent_config("archived-agent");
    author.put(&scope, &config).await.unwrap();
    author.publish(&scope, &config.id).await.unwrap();
    config.archived_at = Some("2026-07-30T00:00:00Z".into());
    store.put_config_scoped(&scope, &config).await.unwrap();

    let (cold, catalog) = test_service_and_catalog();
    assert_eq!(
        cold.reconcile_registrations(store.as_ref(), &scope)
            .await
            .unwrap(),
        0,
        "R2"
    );
    assert!(catalog.current(scope.as_str(), &config.id).is_none(), "R2");
    assert!(catalog.is_unavailable(scope.as_str(), &config.id), "R2");
}

#[tokio::test]
async fn publication_preview_is_write_free_and_matches_publish() {
    let store = SqliteConfigStore::open_in_memory().unwrap();
    let workspace = scope("wrkspc_preview");
    let (service, catalog) = test_service_and_catalog();
    ConfigRegistry::put_config(&store, &agent_config("preview-agent"))
        .await
        .unwrap();

    let preview = service
        .preview_publication(&workspace, &store, "preview-agent", &[])
        .await
        .unwrap();
    assert!(
        ConfigRegistry::get_publication(&store, &preview.fingerprint)
            .await
            .unwrap()
            .is_none(),
        "preview must not persist Control publication truth"
    );
    assert!(
        catalog
            .current(workspace.as_str(), "preview-agent")
            .is_none(),
        "preview must not mutate Coordinator registration"
    );

    let published = service
        .publish(&workspace, &store, "preview-agent", &[])
        .await
        .unwrap();
    assert_eq!(preview.fingerprint, published.fingerprint);
    assert_eq!(preview.source_revision, published.source_revision);
}

#[tokio::test]
async fn publish_is_idempotent_by_fingerprint() {
    // Re-publishing an unchanged config is content-addressed: the same
    // fingerprint both times, and exactly one durable published row
    // (`ON CONFLICT(fingerprint) DO NOTHING`).
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let scope = ScopeId::from("wrkspc_idem");
    let plane = plane_over(store.clone());
    plane
        .put(&scope, &agent_config("idem-agent"))
        .await
        .unwrap();

    let first = plane.publish(&scope, "idem-agent").await.unwrap();
    let second = plane.publish(&scope, "idem-agent").await.unwrap();
    assert_eq!(
        first.fingerprint, second.fingerprint,
        "an unchanged config publishes to the same content address"
    );

    let published = store.list_published_scoped(&scope).await.unwrap();
    assert_eq!(published.len(), 1, "idempotent by fingerprint: one row");
    assert_eq!(published[0].fingerprint, first.fingerprint);
}
