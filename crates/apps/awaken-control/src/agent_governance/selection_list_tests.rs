// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Actual Native IAM and Config owners test current selection
// @formal reason: reads independently of Agent use, with no new authority or writes on GET.
use super::*;
use awaken_agent_config::personal_selection::AgentSelectionIntent;

async fn exercise_owner_list<B, T>(native: Native<B>, configs: Arc<T>)
where
    B: SqlConn + Clone + 'static,
    T: ScopedConfigRegistry
        + AgentCatalogSnapshotRepository
        + PersonalAgentSelectionRepository
        + 'static,
{
    seed_config(configs.as_ref()).await;
    register(
        &native.store,
        &["a-visible", "b-hidden", "c-visible", "d-hidden", "z-orphan"],
    );
    let scope = ScopeId::from("workspace");
    let owner = AgentSelectionOwner::new(ISSUER.into(), "person-a".into()).unwrap();
    let original = AgentSelectionIntent {
        request_key: "original".into(),
        expected_generation: 0,
        selected: true,
    };
    // z-orphan deliberately has no Agent definition or publication. Preferences
    // remain owned data even after the defining object has gone away.
    for id in ["a-visible", "b-hidden", "c-visible", "z-orphan"] {
        configs
            .set_personal_agent_selection(&scope, &owner, id, &original)
            .await
            .unwrap()
            .unwrap();
    }
    for id in ["a-visible", "c-visible", "z-orphan"] {
        grant(&native.store, "person-a", id, "agent.selection.read");
    }
    grant(
        &native.store,
        "person-a",
        "z-orphan",
        "agent.selection.write",
    );
    // Same opaque Account at another issuer, another Account, and another
    // Workspace must not overwrite or add this owner's current rows.
    for foreign in [
        AgentSelectionOwner::new(ISSUER.into(), "person-b".into()).unwrap(),
        AgentSelectionOwner::new("https://other-native.example".into(), "person-a".into()).unwrap(),
    ] {
        configs
            .set_personal_agent_selection(&scope, &foreign, "d-hidden", &original)
            .await
            .unwrap()
            .unwrap();
    }
    configs
        .set_personal_agent_selection(&ScopeId::from("foreign"), &owner, "d-hidden", &original)
        .await
        .unwrap()
        .unwrap();
    let before = configs.agent_catalog_epoch(&scope).await.unwrap();
    let app = routes::router(
        AgentGovernanceStores::new(
            configs.clone(),
            Some(AgentGovernanceConfig {
                product_id: product(),
            }),
        ),
        Some(native.iam.clone()),
    );
    let token = &native.tokens[0];
    grant(&native.store, "person-a", "a-visible", "agent.use");
    let usable = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/agent-directory",
        Value::Null,
    )
    .await;
    assert_eq!(usable.0, StatusCode::OK);
    assert_eq!(usable.1["data"].as_array().unwrap().len(), 1);
    revoke(
        &native.store,
        &grant_id("person-a", "a-visible", "agent.use"),
    );
    // Reuse the same issued token after a real native policy revocation.
    let first = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/me/agent-selections?selected=true&limit=1",
        Value::Null,
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    let after = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        "a-visible",
    );
    assert_eq!(
        first.1,
        json!({"data":[{"agent_id":"a-visible","generation":1,"selected":true}],"next_page":after,"visibility":"current-owner-authorized-selection-subset"})
    );
    let second = call(
        &app,
        token,
        "workspace",
        "GET",
        &format!("/v1/me/agent-selections?selected=true&limit=1&after={after}"),
        Value::Null,
    )
    .await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(
        second.1["data"],
        json!([{"agent_id":"c-visible","generation":1,"selected":true}])
    );
    let cursor = second.1["next_page"].as_str().unwrap();
    let third = call(
        &app,
        token,
        "workspace",
        "GET",
        &format!("/v1/me/agent-selections?selected=true&limit=1&after={cursor}"),
        Value::Null,
    )
    .await;
    assert_eq!(third.0, StatusCode::OK);
    assert_eq!(
        third.1["data"],
        json!([{"agent_id":"z-orphan","generation":1,"selected":true}])
    );
    assert!(third.1["next_page"].is_null());
    assert_eq!(
        configs.agent_catalog_epoch(&scope).await.unwrap(),
        before,
        "all owner-list GETs are read-only"
    );
    let market = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/agent-directory",
        Value::Null,
    )
    .await;
    assert_eq!(market.0, StatusCode::OK);
    assert_eq!(
        market.1["data"],
        json!([]),
        "selection.read confers no Agent use or manifest"
    );
    let other = call(
        &app,
        &native.tokens[1],
        "workspace",
        "GET",
        "/v1/me/agent-selections",
        Value::Null,
    )
    .await;
    assert_eq!(other.0, StatusCode::OK);
    assert_eq!(
        other.1["data"],
        json!([]),
        "unreadable preferences are no global absence claim"
    );
    for invalid in &native.rejected_credentials {
        assert_eq!(
            call(
                &app,
                invalid,
                "workspace",
                "GET",
                "/v1/me/agent-selections",
                Value::Null
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    for path in [
        "/v1/me/agent-selections?limit=0",
        "/v1/me/agent-selections?limit=101",
        "/v1/me/agent-selections?after=invalid",
        "/v1/me/agent-selections?owner=person-b",
        "/v1/me/agent-selections?selected=unknown",
    ] {
        assert_eq!(
            call(&app, token, "workspace", "GET", path, Value::Null)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    let foreign = call(
        &app,
        token,
        "foreign",
        "GET",
        "/v1/me/agent-selections",
        Value::Null,
    )
    .await;
    assert_eq!(foreign.0, StatusCode::OK);
    assert_eq!(
        foreign.1,
        json!({"data":[],"next_page":null,"visibility":"current-owner-authorized-selection-subset"}),
        "a valid trusted Workspace with no readable rows never falls back to another Workspace"
    );
    let missing = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/me/agent-selections")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::FORBIDDEN);
    let unconfigured = routes::router(
        AgentGovernanceStores::new(configs.clone(), None),
        Some(native.iam.clone()),
    );
    assert_eq!(
        call(
            &unconfigured,
            token,
            "workspace",
            "GET",
            "/v1/me/agent-selections",
            Value::Null
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // A fresh current generation enables the independently authorized removal,
    // without use, definition, publication, or a fabricated original receipt.
    let removed = call(
        &app,
        token,
        "workspace",
        "PUT",
        "/v1/me/agent-selections/z-orphan",
        json!({"request_key":"remove","expected_generation":1,"selected":false}),
    )
    .await;
    assert_eq!(removed.0, StatusCode::OK, "{}", removed.1);
    let current = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/me/agent-selections?selected=false",
        Value::Null,
    )
    .await;
    assert_eq!(
        current.1["data"],
        json!([{"agent_id":"z-orphan","generation":2,"selected":false}])
    );
    let outcome = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/me/agent-selections/z-orphan/outcomes/original",
        Value::Null,
    )
    .await;
    assert_eq!(
        outcome.1["receipt"]["applied_generation"], 1,
        "original receipt is distinct from current generation two"
    );
    assert_eq!(outcome.1["receipt"]["intent"]["selected"], true);
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "PUT",
            "/v1/me/agent-selections/c-visible",
            json!({"request_key":"ungranted-write","expected_generation":1,"selected":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // A write grant alone never substitutes for independent selection.read.
    grant(
        &native.store,
        "person-a",
        "a-visible",
        "agent.selection.write",
    );
    revoke(
        &native.store,
        &grant_id("person-a", "a-visible", "agent.selection.read"),
    );
    let hidden = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/me/agent-selections?selected=true",
        Value::Null,
    )
    .await;
    assert_eq!(
        hidden.1["data"],
        json!([{"agent_id":"c-visible","generation":1,"selected":true}])
    );
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            &format!("/v1/me/agent-selections?after={after}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/me/agent-selections/a-visible/outcomes/original",
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    grant(
        &native.store,
        "person-a",
        "a-visible",
        "agent.selection.read",
    );
    for mode in [1, 2, 3, 4] {
        native.faults.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            call(
                &app,
                token,
                "workspace",
                "GET",
                "/v1/me/agent-selections",
                Value::Null
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE,
            "mode {mode} cannot become a successful partial/empty page"
        );
        native.faults.mode.store(0, Ordering::SeqCst);
    }
    let changed = native.store.clone();
    *native.faults.mutate.lock().unwrap() = Some(Box::new(move || {
        grant(&changed, "person-b", "b-hidden", "agent.selection.read")
    }));
    native.faults.mode.store(5, Ordering::SeqCst);
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/me/agent-selections",
            Value::Null
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    native.faults.mode.store(0, Ordering::SeqCst);
    *native.faults.catalog_mutation.lock().unwrap() = Some(configs.clone());
    native.faults.mode.store(6, Ordering::SeqCst);
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/me/agent-selections",
            Value::Null
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    native.faults.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/me/agent-selections",
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    AccountRepository::upsert(
        &native.store,
        Account {
            id: AccountId("person-a".into()),
            status: AccountStatus::Disabled,
            display_name: None,
            created_at: at(),
            updated_at: at(),
        },
    )
    .unwrap();
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/me/agent-selections",
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_native_owner_current_selections_survive_lost_agent_use_and_definition() {
    exercise_owner_list(
        native(sqlite_in_memory_store("agent_owner_list").unwrap()).await,
        Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory()
                .await
                .unwrap(),
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_native_owner_current_selections_survive_lost_agent_use_and_definition() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let pool = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
    let config = Arc::new(
        awaken_config_store::PostgresConfigStore::with_pool(pool.clone())
            .await
            .unwrap(),
    );
    let iam_store = tokio::task::spawn_blocking(move || {
        awaken_iam_server::postgres_migrated_store(&url, "iam").unwrap()
    })
    .await
    .unwrap();
    exercise_owner_list(native(iam_store).await, config).await;
    pool.close().await;
}
