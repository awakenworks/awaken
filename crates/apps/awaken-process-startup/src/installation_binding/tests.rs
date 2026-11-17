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

#[tokio::test]
async fn local_installation_decision_table_requires_explicit_initialization() {
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
     * Decision table: L1 !marker+!Session+ordinary=>E2 through the Local
     * first-start row (full table in first_start_decision_table_*); L2
     * same+init=>E2;
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
    async fn open_session(root: &std::path::Path) {
        awaken_session_store::SqliteManagedSessionRepository::open(
            &root.join("sessions.db").to_string_lossy(),
        )
        .await
        .unwrap()
        .close()
        .await;
    }

    let ordinary = InstallationAuthorization::default();
    let initialize = InstallationAuthorization::new(Some("install-local-2048".into()), None)
        .expect("complete initialization authority");
    let adoption = InstallationAuthorization::new(None, Some("adopt-local-2048".into()))
        .expect("complete adoption authority");

    let fresh = tempfile::tempdir().unwrap();
    let fresh_deployment = deployment(fresh.path());
    assert_eq!(
        classify_local_installation(&fresh_deployment, &ordinary)
            .await
            .expect("L1 Local first start")
            .1,
        InstallationAuthorization::local_first_start(),
        "L1 the first-start row takes the explicit initialization authority"
    );
    classify_local_installation(&fresh_deployment, &initialize)
        .await
        .expect("L2");
    assert!(
        classify_local_installation(&fresh_deployment, &adoption)
            .await
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
            .await
            .unwrap_err()
            .starts_with("session_storage_missing:"),
        "L3"
    );
    classify_local_installation(&marker_first_deployment, &initialize)
        .await
        .expect("L4");
    assert!(
        !marker_first.path().join("sessions.db").exists(),
        "L3-L4 read-only"
    );

    let store_first = tempfile::tempdir().unwrap();
    open_session(store_first.path()).await;
    let store_first_deployment = deployment(store_first.path());
    assert!(
        classify_local_installation(&store_first_deployment, &ordinary)
            .await
            .unwrap_err()
            .starts_with("local_installation_incomplete:"),
        "L5"
    );
    classify_local_installation(&store_first_deployment, &initialize)
        .await
        .expect("L6");
    assert!(
        !store_first.path().join("platform-workspace-id").exists(),
        "L5-L6 read-only"
    );

    std::fs::write(
        store_first.path().join("platform-workspace-id"),
        "workspace-local",
    )
    .unwrap();
    let (admitted, _) = classify_local_installation(&store_first_deployment, &ordinary)
        .await
        .expect("L7");
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

    let (authorized_missing, _) = classify_local_installation(&store_first_deployment, &initialize)
        .await
        .expect("L12");
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
    let (replaced, _) = classify_local_installation(&store_first_deployment, &ordinary)
        .await
        .expect("L11 A proof");
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
    let (expected, _) = classify_local_installation(&expected_change, &ordinary)
        .await
        .expect("L11 E proof");
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
            .await
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
            .await
            .unwrap_err()
            .starts_with("platform_workspace_mismatch:"),
        "L8 mismatch"
    );
    let expected_missing = tempfile::tempdir().unwrap();
    let mut expected_missing_deployment = deployment(expected_missing.path());
    expected_missing_deployment.expected_platform_workspace_id = Some("workspace-expected".into());
    classify_local_installation(&expected_missing_deployment, &initialize)
        .await
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
            .await
            .unwrap_err()
            .starts_with("unbound_empty_local_storage:"),
        "L9 ordinary"
    );
    classify_local_installation(&control_deployment, &initialize)
        .await
        .expect("L9 initialize");
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
    let Some(base_url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let admin = awaken_reliability_testkit::postgres_fixture_pool(&base_url).await;
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fresh_schema = format!("cli_binding_fresh_{suffix}");
    let legacy_schema = format!("cli_binding_legacy_{suffix}");
    for schema in [&fresh_schema, &legacy_schema] {
        // Both names use fixed fixture prefixes and the integer time suffix.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
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
        classify_local_installation(&deployment, &InstallationAuthorization::default())
            .await
            .is_ok(),
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
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
    }
    admin.close().await;
}

/// Every file under `root` with its bytes; a missing root is an empty tree.
fn tree(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        path: &std::path::Path,
        out: &mut BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.insert(path.strip_prefix(root).unwrap().to_path_buf(), Vec::new());
                walk(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[tokio::test]
async fn first_start_decision_table_covers_startup_and_migrate_entries() {
    /* Cause/effect graph: C1 operating mode Local/Server; C2 role AllInOne or
     * another role; C3 any role-owned PostgreSQL target; C4 configured
     * expected_platform_workspace_id; C5 platform-workspace-id marker
     * absent/present; C6 canonical Session store absent/present/zero-byte/
     * invalid; C7 requested authorization ordinary/explicit initialization/
     * adoption only. Effects: E1 the one first-start row grants exactly the
     * authorization `--initialize-installation --initialization-reference
     * local-first-start` builds, and both the startup entry (FirstStart) and
     * the flag-free `database migrate` entry carry a first-start proof; E2 a
     * plain-language refusal naming the copy-pasteable recovery command; E3
     * explicit flags keep their meaning in every mode and are never announced
     * as a first start; E4 classification never writes a byte.
     *
     * Decision table (startup = admit_ordinary_start, migrate =
     * prepare_deployment_installations with no flags):
     * F1  Local AllInOne, empty dir                      => E1+E4
     * F2  Local AllInOne, dir missing                    => E1+E4
     * F3  Local AllInOne, dir holding only config.toml   => E1+E4
     * F4  Local, marker without Session store            => session_storage_missing+E2+E4
     * F5  Local, Session store without marker            => local_installation_incomplete+E2+E4
     * F6  Local, zero-byte Session file                  => session_storage_empty+E2+E4
     * F7  Local, invalid Session file                    => session_storage_invalid+E2+E4
     * F8  Local AllInOne with a PostgreSQL target, empty => unbound_empty_local_storage+E2+E4
     * F9  Server, empty, no expected Workspace           => unbound_empty_local_storage+E2+E4
     * F10 Server, empty, expected Workspace              => platform_workspace_missing+E2+E4
     * F11 Local, empty, expected Workspace               => platform_workspace_missing+E2+E4
     * F12 Local Control role, empty                      => unbound_empty_local_storage+E2+E4
     * F13 Local AllInOne, empty, adoption flags only     => unbound_empty_local_storage+E4 (migrate only)
     * F14 Local AllInOne, empty, explicit initialization => E3+E4 (migrate only)
     * Read-only consumers (verify_deployment_installations) report F1 as
     * local_installation_not_initialized and never write. */
    enum Fixture {
        Empty,
        Missing,
        ConfigOnly,
        MarkerOnly,
        SessionOnly,
        ZeroByteSession,
        InvalidSession,
    }
    async fn materialize(root: &std::path::Path, fixture: &Fixture) {
        match fixture {
            Fixture::Missing => {}
            Fixture::Empty => std::fs::create_dir_all(root).unwrap(),
            Fixture::ConfigOnly => {
                std::fs::create_dir_all(root).unwrap();
                std::fs::write(root.join("config.toml"), "identity_mode = \"no-login\"\n").unwrap();
            }
            Fixture::MarkerOnly => {
                std::fs::create_dir_all(root).unwrap();
                awaken_installation_workspace::publish_at(root, "workspace-local").unwrap();
            }
            Fixture::SessionOnly => {
                std::fs::create_dir_all(root).unwrap();
                awaken_session_store::SqliteManagedSessionRepository::open(
                    &root.join("sessions.db").to_string_lossy(),
                )
                .await
                .unwrap()
                .close()
                .await;
            }
            Fixture::ZeroByteSession => {
                std::fs::create_dir_all(root).unwrap();
                std::fs::write(root.join("sessions.db"), []).unwrap();
            }
            Fixture::InvalidSession => {
                std::fs::create_dir_all(root).unwrap();
                std::fs::write(root.join("sessions.db"), b"not a database").unwrap();
            }
        }
    }
    let local = |root: &std::path::Path| crate::config::local_test_deployment(root.to_path_buf());
    let with_postgres = |root: &std::path::Path| {
        let mut deployment = local(root);
        deployment.control.catalog =
            awaken_control::StoreBackend::Postgres("postgres://127.0.0.1:9/never-connected".into());
        deployment
    };
    let server = |root: &std::path::Path| {
        let mut deployment = local(root);
        deployment.mode = OperatingMode::Server;
        deployment
    };
    let server_expected = |root: &std::path::Path| {
        let mut deployment = server(root);
        deployment.expected_platform_workspace_id = Some("workspace-expected".into());
        deployment
    };
    let local_expected = |root: &std::path::Path| {
        let mut deployment = local(root);
        deployment.expected_platform_workspace_id = Some("workspace-expected".into());
        deployment
    };
    let control = |root: &std::path::Path| {
        let mut deployment = local(root);
        deployment.role = Role::Control;
        deployment
    };
    type Build<'a> = &'a dyn Fn(&std::path::Path) -> ResolvedDeployment;
    // (rule, deployment, fixture, expected refusal code or None for E1, recovery hint)
    let rows: [(&str, Build<'_>, Fixture, Option<&str>, &str); 12] = [
        ("F1", &local, Fixture::Empty, None, ""),
        ("F2", &local, Fixture::Missing, None, ""),
        ("F3", &local, Fixture::ConfigOnly, None, ""),
        (
            "F4",
            &local,
            Fixture::MarkerOnly,
            Some("session_storage_missing:"),
            "--initialize-installation --initialization-reference local-recovery",
        ),
        (
            "F5",
            &local,
            Fixture::SessionOnly,
            Some("local_installation_incomplete:"),
            "--initialize-installation --initialization-reference local-recovery",
        ),
        (
            "F6",
            &local,
            Fixture::ZeroByteSession,
            Some("session_storage_empty:"),
            "\n  rm ",
        ),
        (
            "F7",
            &local,
            Fixture::InvalidSession,
            Some("session_storage_invalid:"),
            "\n  mv ",
        ),
        (
            "F8",
            &with_postgres,
            Fixture::Empty,
            Some("unbound_empty_local_storage:"),
            "--initialize-installation --initialization-reference local-recovery",
        ),
        (
            "F9",
            &server,
            Fixture::Empty,
            Some("unbound_empty_local_storage:"),
            "--initialize-installation --initialization-reference local-recovery",
        ),
        (
            "F10",
            &server_expected,
            Fixture::Empty,
            Some("platform_workspace_missing:"),
            "--initialize-installation --initialization-reference local-recovery",
        ),
        (
            "F11",
            &local_expected,
            Fixture::Empty,
            Some("platform_workspace_missing:"),
            "--initialize-installation --initialization-reference local-recovery",
        ),
        (
            "F12",
            &control,
            Fixture::Empty,
            Some("unbound_empty_local_storage:"),
            "awaken-control database migrate",
        ),
    ];
    let ordinary = InstallationAuthorization::default();
    for (rule, build, fixture, refusal, hint) in rows {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("data");
        materialize(&root, &fixture).await;
        let deployment = build(&root);
        let before = tree(parent.path());
        let startup = admit_ordinary_start(&deployment).await;
        let migrate = prepare_deployment_installations(&deployment, &ordinary).await;
        match refusal {
            None => {
                assert!(
                    first_installation_is_automatic(&deployment),
                    "{rule} static half"
                );
                let Ok(OrdinaryAdmission::FirstStart(startup)) = startup else {
                    panic!("{rule}/startup must take the first-start row");
                };
                assert_eq!(
                    startup.local_first_start_root(),
                    Some(root.as_path()),
                    "{rule}/startup E1"
                );
                let migrate = migrate.unwrap_or_else(|error| panic!("{rule}/migrate: {error}"));
                assert_eq!(
                    migrate.local_first_start_root(),
                    Some(root.as_path()),
                    "{rule}/migrate E1"
                );
                assert_eq!(
                    migrate.platform_workspace_before_write().unwrap(),
                    None,
                    "{rule} new identity is minted by the store opener"
                );
                let read_only = verify_deployment_installations(&deployment)
                    .await
                    .err()
                    .unwrap_or_else(|| panic!("{rule}/verify stays read-only"));
                assert!(
                    read_only.starts_with("local_installation_not_initialized:"),
                    "{rule}/verify: {read_only}"
                );
            }
            Some(code) => {
                for (entry, error) in [
                    (
                        "startup",
                        startup
                            .err()
                            .unwrap_or_else(|| panic!("{rule}/startup must refuse")),
                    ),
                    (
                        "migrate",
                        migrate
                            .err()
                            .unwrap_or_else(|| panic!("{rule}/migrate must refuse")),
                    ),
                ] {
                    assert!(error.starts_with(code), "{rule}/{entry}: {error}");
                    assert!(
                        error.contains(hint),
                        "{rule}/{entry} recovery command: {error}"
                    );
                    assert!(
                        !error.contains("<REF>"),
                        "{rule}/{entry} command is copy-pasteable: {error}"
                    );
                }
            }
        }
        assert_eq!(tree(parent.path()), before, "{rule} E4 read-only");
    }

    assert_eq!(
        admission_authorization(&local(std::path::Path::new("/unused")), &ordinary, true),
        InstallationAuthorization::new(Some("local-first-start".into()), None).unwrap(),
        "E1 the same authorization the explicit flags construct"
    );

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("data");
    let adoption = InstallationAuthorization::new(None, Some("adopt-2048".into())).unwrap();
    let error = prepare_deployment_installations(&local(&root), &adoption)
        .await
        .err()
        .expect("F13 adoption is never initialization");
    assert!(
        error.starts_with("unbound_empty_local_storage:"),
        "F13: {error}"
    );
    let initialize = InstallationAuthorization::new(Some("install-2048".into()), None).unwrap();
    let explicit = prepare_deployment_installations(&local(&root), &initialize)
        .await
        .expect("F14 explicit initialization stays valid");
    assert_eq!(
        explicit.local_first_start_root(),
        None,
        "F14 is not announced as a first start"
    );
    assert!(explicit.publishes_local_workspace(), "F14");
    assert!(!root.exists(), "F13-F14 E4 read-only");
}
