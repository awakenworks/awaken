use super::*;

fn binding_objects() -> BTreeSet<String> {
    BTreeSet::from([
        format!("relation:{BINDING_TABLE}"),
        format!("relation:{LEDGER_TABLE}"),
        format!("relation:{LEDGER_META_TABLE}"),
    ])
}

fn observation(objects: BTreeSet<String>, bundle: BundleObservation) -> TargetObservation {
    TargetObservation { objects, bundle }
}

fn row(workspace: &str, origin: &str, reference: &str) -> BindingRow {
    BindingRow {
        platform_workspace_id: workspace.into(),
        binding_origin: origin.into(),
        operator_reference: reference.into(),
    }
}

#[test]
fn target_observation_decision_table_fails_closed_without_parallel_identity() {
    /* Cause/effect graph: C1 binding ledger missing/current/invalid; C2 exact
     * binding objects absent/present/partial; C3 other schema objects absent or
     * present; C4 binding row absent/exact/mismatched/corrupt; C5 explicit
     * initialize/adopt authorization absent/present. Effects: E1 explicit fresh
     * initialization; E2 exact read-only admit;
     * E3 auditable legacy adoption; E4 stable fail-closed rejection. Constraint:
     * Session/Control/IAM rows and a local marker are not observations here.
     *
     * Decision table: I1 missing+empty+!initialize=>E4; I1b same+initialize=>E1;
     * I2 current+exact row=>E2;
     * I3 missing+existing+!C5=>E4; I4 missing+existing+C5=>E3;
     * I5 partial/invalid=>E4; I6 current+no row+binding-only=>E1 (recover
     * bundle-before-row crash); I7 mismatch/corrupt row=>E4. */
    let none = InstallationAuthorization::default();
    let initialize = InstallationAuthorization::new(Some("install-2048".into()), None).unwrap();
    let adoption = InstallationAuthorization::new(None, Some("change-2048".into())).unwrap();
    assert!(
        classify_observation(
            &observation(BTreeSet::new(), BundleObservation::Missing),
            "workspace-a",
            &none,
        )
        .unwrap_err()
        .contains("unbound_empty_database"),
        "I1"
    );
    assert_eq!(
        classify_observation(
            &observation(BTreeSet::new(), BundleObservation::Missing),
            "workspace-a",
            &initialize,
        )
        .unwrap(),
        BindingAction::InitializeFresh,
        "I1b"
    );
    assert!(
        classify_observation(
            &observation(BTreeSet::new(), BundleObservation::Missing),
            "workspace-a",
            &adoption,
        )
        .unwrap_err()
        .contains("unbound_empty_database"),
        "I1c adoption is not initialization authority"
    );
    assert_eq!(
        classify_observation(
            &observation(
                binding_objects(),
                BundleObservation::Current(Some(row(
                    "workspace-a",
                    "fresh_initialization",
                    "install-2048",
                ))),
            ),
            "workspace-a",
            &none,
        )
        .unwrap(),
        BindingAction::Bound,
        "I2"
    );
    let existing = BTreeSet::from(["relation:managed_session".into()]);
    assert!(
        classify_observation(
            &observation(existing.clone(), BundleObservation::Missing),
            "workspace-a",
            &none,
        )
        .unwrap_err()
        .contains("unbound_existing_database"),
        "I3"
    );
    assert_eq!(
        classify_observation(
            &observation(existing.clone(), BundleObservation::Missing),
            "workspace-a",
            &adoption,
        )
        .unwrap(),
        BindingAction::AdoptLegacy,
        "I4"
    );
    assert!(
        classify_observation(
            &observation(existing, BundleObservation::Missing),
            "workspace-a",
            &initialize,
        )
        .unwrap_err()
        .contains("unbound_existing_database"),
        "I4b initialization is not adoption authority"
    );
    assert!(
        classify_observation(
            &observation(
                BTreeSet::from([format!("relation:{BINDING_TABLE}")]),
                BundleObservation::Missing,
            ),
            "workspace-a",
            &adoption,
        )
        .unwrap_err()
        .contains("partial"),
        "I5 partial"
    );
    assert!(
        classify_observation(
            &observation(
                binding_objects(),
                BundleObservation::Invalid("checksum drift".into()),
            ),
            "workspace-a",
            &none,
        )
        .unwrap_err()
        .contains("corrupt"),
        "I5 invalid"
    );
    assert!(
        classify_observation(
            &observation(binding_objects(), BundleObservation::Current(None)),
            "workspace-a",
            &none,
        )
        .unwrap_err()
        .contains("unbound_empty_database"),
        "I6"
    );
    assert_eq!(
        classify_observation(
            &observation(binding_objects(), BundleObservation::Current(None)),
            "workspace-a",
            &initialize,
        )
        .unwrap(),
        BindingAction::InitializeFresh,
        "I6 retry"
    );
    for (rule, binding) in [
        (
            "I7 mismatch",
            row("workspace-other", "fresh_initialization", "install-2048"),
        ),
        (
            "I7 corrupt",
            row("workspace-a", "fresh_migrate", "unexpected"),
        ),
    ] {
        assert!(
            classify_observation(
                &observation(binding_objects(), BundleObservation::Current(Some(binding)),),
                "workspace-a",
                &none,
            )
            .is_err(),
            "{rule}"
        );
    }
}

#[test]
fn local_installation_decision_table_requires_explicit_initialization() {
    /* Cause/effect graph: C1 local marker is absent/exact/mismatched; C2 the
     * canonical SQLite Session store is absent/current/invalid; C3 explicit
     * initialization authorization is absent/present; C4 the role owns
     * Coordinator Session storage or only Control storage; C5 an admitted marker
     * is unchanged/replaced/disappears before store opening. Effects: E1 ordinary
     * exact-only admission; E2 explicit first initialization or completion of a
     * marker-first/store-first crash window; E3 stable read-only rejection; E4
     * the typed proof rejects every changed existing coordinate before writes.
     * Constraints: adoption never grants initialization; configured expected
     * identity mismatch and an invalid Session DB are never bypassed. Explicit
     * initialization may publish a configured identity when no marker exists.
     *
     * Decision table: L1 !marker+!Session+ordinary=>E3; L2 same+init=>E2;
     * L3 marker+!Session+ordinary=>E3; L4 same+init=>E2; L5 !marker+Session+
     * ordinary=>E3; L6 same+init=>E2; L7 marker+Session+ordinary=>E1;
     * L8 invalid or expected mismatch + any auth=>E3; L9 Control marker absent+
     * ordinary/init=>E3/E2; L10 existing A+unchanged=>E4 admit; L11 A->B or
     * A->missing=>E4 reject; L12 initially missing+initialize=>E2 before-write
     * completion. A configured E uses the same L11 exact comparison.
     * Every classifier observation leaves the fixture byte-identical. */
    fn deployment(root: &std::path::Path) -> ResolvedDeployment {
        crate::config::local_test_deployment(root.to_path_buf())
    }
    fn open_session(root: &std::path::Path) {
        drop(
            awaken_session_store::SqliteManagedSessionRepository::open(
                &root.join("sessions.db").to_string_lossy(),
            )
            .unwrap(),
        );
    }

    let ordinary = InstallationAuthorization::default();
    let initialize = InstallationAuthorization::new(Some("install-local-2048".into()), None)
        .expect("complete initialization authority");
    let adoption = InstallationAuthorization::new(None, Some("adopt-local-2048".into()))
        .expect("complete adoption authority");

    let fresh = tempfile::tempdir().unwrap();
    let fresh_deployment = deployment(fresh.path());
    assert!(
        classify_local_installation(&fresh_deployment, &ordinary)
            .unwrap_err()
            .starts_with("unbound_empty_local_storage:"),
        "L1"
    );
    classify_local_installation(&fresh_deployment, &initialize).expect("L2");
    assert!(
        classify_local_installation(&fresh_deployment, &adoption)
            .unwrap_err()
            .starts_with("unbound_empty_local_storage:"),
        "L2 adoption is not initialization authority"
    );
    assert_eq!(
        std::fs::read_dir(fresh.path()).unwrap().count(),
        0,
        "L1-L2 read-only"
    );

    let marker_first = tempfile::tempdir().unwrap();
    std::fs::write(
        marker_first.path().join("platform-workspace-id"),
        "workspace-local",
    )
    .unwrap();
    let marker_first_deployment = deployment(marker_first.path());
    assert!(
        classify_local_installation(&marker_first_deployment, &ordinary)
            .unwrap_err()
            .starts_with("session_storage_missing:"),
        "L3"
    );
    classify_local_installation(&marker_first_deployment, &initialize).expect("L4");
    assert!(
        !marker_first.path().join("sessions.db").exists(),
        "L3-L4 read-only"
    );

    let store_first = tempfile::tempdir().unwrap();
    open_session(store_first.path());
    let store_first_deployment = deployment(store_first.path());
    assert!(
        classify_local_installation(&store_first_deployment, &ordinary)
            .unwrap_err()
            .starts_with("local_installation_incomplete:"),
        "L5"
    );
    classify_local_installation(&store_first_deployment, &initialize).expect("L6");
    assert!(
        !store_first.path().join("platform-workspace-id").exists(),
        "L5-L6 read-only"
    );

    std::fs::write(
        store_first.path().join("platform-workspace-id"),
        "workspace-local",
    )
    .unwrap();
    let admitted = classify_local_installation(&store_first_deployment, &ordinary).expect("L7");
    assert_eq!(
        admitted.workspace_before_write().unwrap(),
        Some("workspace-local".into()),
        "L10 unchanged exact proof"
    );
    let session_before = std::fs::read(store_first.path().join("sessions.db")).unwrap();
    std::fs::write(
        store_first.path().join("platform-workspace-id"),
        "workspace-replaced",
    )
    .unwrap();
    std::fs::remove_file(store_first.path().join("platform-workspace-id")).unwrap();
    assert!(
        admitted
            .workspace_before_write()
            .unwrap_err()
            .starts_with("local_installation_changed:"),
        "L11 an admitted A cannot become missing"
    );
    assert_eq!(
        std::fs::read(store_first.path().join("sessions.db")).unwrap(),
        session_before,
        "L11 fence precedes every Session write"
    );

    let authorized_missing =
        classify_local_installation(&store_first_deployment, &initialize).expect("L12");
    assert_eq!(
        authorized_missing.workspace_before_write().unwrap(),
        None,
        "L12 an initially missing authorized marker may be completed after stores"
    );
    assert!(
        !store_first.path().join("platform-workspace-id").exists(),
        "L12 before-write fence remains read-only"
    );

    std::fs::write(
        store_first.path().join("platform-workspace-id"),
        "workspace-local",
    )
    .unwrap();
    let replaced =
        classify_local_installation(&store_first_deployment, &ordinary).expect("L11 A proof");
    std::fs::write(
        store_first.path().join("platform-workspace-id"),
        "workspace-replaced",
    )
    .unwrap();
    assert!(
        replaced
            .workspace_before_write()
            .unwrap_err()
            .starts_with("local_installation_changed:"),
        "L11 A->B rejects before writes"
    );

    std::fs::write(
        store_first.path().join("platform-workspace-id"),
        "workspace-expected",
    )
    .unwrap();
    let mut expected_change = store_first_deployment.clone();
    expected_change.expected_platform_workspace_id = Some("workspace-expected".into());
    let expected = classify_local_installation(&expected_change, &ordinary).expect("L11 E proof");
    std::fs::write(
        store_first.path().join("platform-workspace-id"),
        "workspace-replaced",
    )
    .unwrap();
    assert!(
        expected
            .workspace_before_write()
            .unwrap_err()
            .starts_with("local_installation_changed:"),
        "L11 expected E->B rejects before writes"
    );

    let invalid = tempfile::tempdir().unwrap();
    std::fs::write(invalid.path().join("sessions.db"), []).unwrap();
    assert!(
        classify_local_installation(&deployment(invalid.path()), &initialize)
            .unwrap_err()
            .starts_with("session_storage_empty:"),
        "L8 invalid"
    );
    let mismatch = tempfile::tempdir().unwrap();
    std::fs::write(
        mismatch.path().join("platform-workspace-id"),
        "workspace-other",
    )
    .unwrap();
    let mut mismatch_deployment = deployment(mismatch.path());
    mismatch_deployment.expected_platform_workspace_id = Some("workspace-expected".into());
    assert!(
        classify_local_installation(&mismatch_deployment, &initialize)
            .unwrap_err()
            .starts_with("platform_workspace_mismatch:"),
        "L8 mismatch"
    );
    let expected_missing = tempfile::tempdir().unwrap();
    let mut expected_missing_deployment = deployment(expected_missing.path());
    expected_missing_deployment.expected_platform_workspace_id = Some("workspace-expected".into());
    classify_local_installation(&expected_missing_deployment, &initialize)
        .expect("L8 explicit initialization may publish the configured identity");
    assert_eq!(
        std::fs::read_dir(expected_missing.path()).unwrap().count(),
        0,
        "L8 classifier remains read-only"
    );

    let control = tempfile::tempdir().unwrap();
    let mut control_deployment = deployment(control.path());
    control_deployment.role = crate::config::Role::Control;
    assert!(
        classify_local_installation(&control_deployment, &ordinary)
            .unwrap_err()
            .starts_with("unbound_empty_local_storage:"),
        "L9 ordinary"
    );
    classify_local_installation(&control_deployment, &initialize).expect("L9 initialize");
}

#[test]
fn role_owned_targets_are_complete_and_deduplicated() {
    /* Causes: C1 role owns Control/Coordinator/neither; C2 several component
     * fields alias one URL or use distinct URLs. Effects: E1 only role-owned
     * targets enter continuity; E2 exact URL aliases become one physical
     * preflight carrying all labels; E3 Worker has no target. Decision table:
     * T1 Control+aliases=>E1+E2; T2 Coordinator runtime/resources/sessions
     * aliases plus distinct capture=>E1+E2; T3 Worker=>E3. */
    let directory = tempfile::tempdir().unwrap();
    let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
    deployment.role = crate::config::Role::Control;
    deployment.control.catalog =
        awaken_control::StoreBackend::Postgres("postgres://target/shared".into());
    deployment.control.credential =
        awaken_control::StoreBackend::Postgres("postgres://target/shared".into());
    deployment.control.environment =
        awaken_control::StoreBackend::Postgres("postgres://target/environment".into());
    let targets = postgres_targets(&deployment);
    assert_eq!(targets.len(), 2, "T1");
    assert!(
        targets
            .iter()
            .any(|target| target.labels == vec!["control.catalog", "control.credential"]),
        "T1/E2: {targets:?}"
    );

    deployment.role = crate::config::Role::Coordinator;
    deployment.runtime.database_url = Some("postgres://target/shared".into());
    deployment.resources = ResourceStoreBackend::Postgres("postgres://target/shared".into());
    deployment.coordinator.sessions =
        awaken_control::StoreBackend::Postgres("postgres://target/shared".into());
    deployment.coordinator.captured_content =
        awaken_control::StoreBackend::Postgres("postgres://target/capture".into());
    let targets = postgres_targets(&deployment);
    assert_eq!(targets.len(), 2, "T2");
    assert!(
        targets.iter().any(|target| target.labels
            == vec!["coordinator.runtime", "coordinator.sessions", "resources"]),
        "T2/E2: {targets:?}"
    );

    deployment.role = crate::config::Role::Worker;
    assert!(postgres_targets(&deployment).is_empty(), "T3");
}

#[test]
fn expected_workspace_requirement_is_mode_and_backend_complete() {
    /* Decision table: W1 Local+no PG+no expected keeps first-install behavior;
     * W2 Server+no expected rejects even before a connection; W3 Local+PG+no
     * expected rejects because no binding value exists; W4 exact expected is
     * returned unchanged. */
    let directory = tempfile::tempdir().unwrap();
    let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
    assert_eq!(
        required_workspace_id(&deployment, true).unwrap(),
        None,
        "W1"
    );
    deployment.mode = OperatingMode::Server;
    assert!(
        required_workspace_id(&deployment, true)
            .unwrap_err()
            .contains("expected_platform_workspace_id_required"),
        "W2"
    );
    deployment.mode = OperatingMode::Local;
    assert!(required_workspace_id(&deployment, false).is_err(), "W3");
    deployment.expected_platform_workspace_id = Some("workspace-exact".into());
    assert_eq!(
        required_workspace_id(&deployment, false).unwrap(),
        Some("workspace-exact"),
        "W4"
    );
}

#[tokio::test]
async fn postgres_preflights_all_targets_then_binds_fresh_or_explicit_legacy() {
    /* Live cause/effect table: P0 AWAKEN_TEST_DATABASE_URL explicitly set and
     * unreachable => fail the gate; P0b URL absent and the canonical local-dev
     * default unreachable => skip only this optional local case; P1 fresh+
     * legacy targets under ordinary migrate => reject and zero objects in the
     * fresh target (replacement database cannot be silently rebuilt); P1b only
     * initialization authorization still rejects the legacy target before DDL;
     * P2 both explicit references => fresh/legacy rows record distinct origins,
     * references, and exact Workspace; P2b ordinary idempotent retry now admits
     * only those exact rows; P3 Serve verification is read-only; P4 changed
     * expected Workspace rejects. The fixture uses unique schemas and never
     * drops a shared database. */
    const DEFAULT_DATABASE_URL: &str =
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test";
    let configured_url = match std::env::var("AWAKEN_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("AWAKEN_TEST_DATABASE_URL is not valid Unicode: {error}"),
    };
    let base_url = configured_url
        .as_deref()
        .unwrap_or(DEFAULT_DATABASE_URL)
        .to_owned();
    let admin = match PgPool::connect(&base_url).await {
        Ok(pool) => pool,
        Err(error) if configured_url.is_none() => {
            eprintln!("default local Postgres is unavailable; skipping CLI binding test: {error}");
            return;
        }
        Err(error) => {
            panic!("explicit AWAKEN_TEST_DATABASE_URL must be reachable: {error}")
        }
    };
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fresh_schema = format!("cli_binding_fresh_{suffix}");
    let legacy_schema = format!("cli_binding_legacy_{suffix}");
    for schema in [&fresh_schema, &legacy_schema] {
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
    }
    let target_url = |schema: &str| {
        let mut url = url::Url::parse(&base_url).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        url.to_string()
    };
    let fresh_url = target_url(&fresh_schema);
    let legacy_url = target_url(&legacy_schema);
    let legacy_pool = PgPool::connect(&legacy_url).await.unwrap();
    sqlx::query("CREATE UNLOGGED TABLE legacy_component (id TEXT PRIMARY KEY)")
        .execute(&legacy_pool)
        .await
        .unwrap();
    legacy_pool.close().await;

    let directory = tempfile::tempdir().unwrap();
    let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
    deployment.role = crate::config::Role::Control;
    deployment.mode = OperatingMode::Server;
    deployment.expected_platform_workspace_id = Some("workspace-pg".into());
    deployment.control.catalog = awaken_control::StoreBackend::Postgres(fresh_url.clone());
    deployment.control.credential = awaken_control::StoreBackend::Postgres(legacy_url.clone());
    deployment.control.config = awaken_control::StoreBackend::Postgres(fresh_url.clone());
    deployment.control.admin = awaken_control::StoreBackend::Postgres(legacy_url.clone());
    deployment.control.data_subject = awaken_control::StoreBackend::Postgres(fresh_url.clone());
    deployment.control.environment = awaken_control::StoreBackend::Postgres(legacy_url.clone());
    assert!(
        !has_role_owned_local_persistence(&deployment),
        "P0 all-PG Server has no local marker authority"
    );
    assert!(
        classify_local_installation(&deployment, &InstallationAuthorization::default()).is_ok(),
        "P0 fresh Server migration is not deadlocked on a missing local marker"
    );
    assert!(
        !directory.path().join("platform-workspace-id").exists(),
        "P0 continuity remains read-only"
    );

    let ordinary = InstallationAuthorization::default();
    let error = prepare_deployment_postgres_installations(&deployment, &ordinary)
        .await
        .expect_err("P1 ordinary migrate cannot bind an empty replacement target");
    assert!(error.contains("unbound_empty_database"), "P1: {error}");
    let fresh_pool = PgPool::connect(&fresh_url).await.unwrap();
    assert!(schema_objects(&fresh_pool).await.unwrap().is_empty(), "P1");
    fresh_pool.close().await;

    let initialize_only =
        InstallationAuthorization::new(Some("install-2048".into()), None).unwrap();
    let error = prepare_deployment_postgres_installations(&deployment, &initialize_only)
        .await
        .expect_err("P1b initialization is not legacy-adoption authority");
    assert!(error.contains("unbound_existing_database"), "P1b: {error}");
    let fresh_pool = PgPool::connect(&fresh_url).await.unwrap();
    assert!(schema_objects(&fresh_pool).await.unwrap().is_empty(), "P1b");
    fresh_pool.close().await;

    let authorization =
        InstallationAuthorization::new(Some("install-2048".into()), Some("change-2048".into()))
            .unwrap();
    assert_eq!(
        prepare_deployment_postgres_installations(&deployment, &authorization)
            .await
            .unwrap()
            .target_count(),
        2,
        "P2"
    );
    for (rule, url, origin, reference) in [
        (
            "P2 fresh",
            &fresh_url,
            "fresh_initialization",
            "install-2048",
        ),
        ("P2 legacy", &legacy_url, "legacy_adoption", "change-2048"),
    ] {
        let pool = PgPool::connect(url).await.unwrap();
        assert_eq!(
            read_binding_row(&pool).await.unwrap(),
            Some(row("workspace-pg", origin, reference)),
            "{rule}"
        );
        pool.close().await;
    }
    assert_eq!(
        prepare_deployment_postgres_installations(&deployment, &ordinary)
            .await
            .unwrap()
            .target_count(),
        2,
        "P2b ordinary retry admits exact bindings without initialization authority"
    );
    assert_eq!(
        verify_deployment_postgres_installations(&deployment)
            .await
            .unwrap()
            .target_count(),
        2,
        "P3"
    );
    deployment.expected_platform_workspace_id = Some("workspace-other".into());
    assert!(
        verify_deployment_postgres_installations(&deployment)
            .await
            .unwrap_err()
            .contains("postgres_installation_mismatch"),
        "P4"
    );

    for schema in [&fresh_schema, &legacy_schema] {
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
    }
    admin.close().await;
}
