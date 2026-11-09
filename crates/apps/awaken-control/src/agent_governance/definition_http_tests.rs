// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Native IAM/SQL regressions observe fine definition admission;
// @formal reason: no browser role or stored preference is accepted as authority.
use super::*;
use axum::Json;

#[tokio::test]
async fn unadopted_governance_preserves_managed_crud_and_the_original_iam_guard() {
    // C1 product adoption is absent; C2 original native IAM permits Workspace
    // read/write; C3 credential invalid or another subject lacks that grant;
    // C4 caller uses standard Managed list/create/update/archive. R1 C1+C2+C4
    // => existing Config publication owner serves the normal wire; R2 C3 =>
    // original 401/403, with no mutation. Optional personal governance confers
    // no new authority and must not require its product in the existing lane.
    let native = native(sqlite_in_memory_store("unadopted_managed").unwrap()).await;
    register(&native.store, &[]);
    workspace_read(&native.store);
    awaken_iam_server::PolicyAdminApi::new(native.store.clone())
        .issue_grant(
            Grant {
                id: GrantId("existing-workspace-write".into()),
                subject: GrantSubject::Principal(principal("person-a")),
                action_pattern: ActionPattern("awaken.workspace::workspace.write".into()),
                scope: ScopeRef::Workspace {
                    workspace_id: WorkspaceId("workspace".into()),
                },
                effect: Effect::Allow,
            },
            at(),
        )
        .unwrap();
    let configs = Arc::new(
        awaken_config_store::SqliteConfigStore::open(":memory:")
            .await
            .unwrap(),
    );
    let (plane, _) = crate::managed_agents::test_support::plane_with_store(configs.clone());
    let repository = Arc::new(crate::ConfigPlaneManagedAgentRepository::request_scoped(
        plane.clone(),
        awaken_time::system_clock(),
    ));
    let raw = crate::config_routes::agent_registry_routers(repository, None, None);
    let guarded = crate::protect_management_router(
        raw,
        plane.management_audit_plane(),
        None,
        Some(native.iam.clone()),
        None,
    );
    let app = super::super::legacy::protect(
        guarded,
        AgentGovernanceStores::new(configs, None),
        Some(native.iam.clone()),
        plane,
        awaken_time::system_clock(),
    );
    let body = json!({"name":"managed-default-adoption","model":"model-a","system":"be helpful","tools":[]});
    assert_eq!(
        invoke(
            &app,
            "sk-invalid-for-account-management",
            "POST",
            "/v1/agents",
            body.clone(),
            "bad-key"
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED,
        "R2"
    );
    assert_eq!(
        invoke(
            &app,
            &native.tokens[1],
            "POST",
            "/v1/agents",
            body.clone(),
            "no-write"
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "R2"
    );
    let (status, created) = invoke(
        &app,
        &native.tokens[0],
        "POST",
        "/v1/agents",
        body,
        "create",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 create: {created}");
    let id = created["id"].as_str().unwrap();
    let (status, listed) = raw_call(&app, &native.tokens[0], "/v1/agents?limit=1").await;
    assert_eq!(status, StatusCode::OK, "R1 list");
    assert_eq!(listed["data"][0]["id"], id);
    let route = format!("/v1/agents/{id}");
    assert_eq!(
        raw_call(&app, &native.tokens[0], &route).await.0,
        StatusCode::OK,
        "R1 retrieve"
    );
    let (status, updated) = invoke(
        &app,
        &native.tokens[0],
        "POST",
        &route,
        json!({"version":created["version"],"system":"updated through Managed API"}),
        "update",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 update: {updated}");
    let (status, archived) = invoke(
        &app,
        &native.tokens[0],
        "POST",
        &format!("{route}/archive"),
        Value::Null,
        "archive",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 archive: {archived}");
}

#[tokio::test]
async fn unadopted_session_definition_overlay_leaves_runtime_and_personal_boundaries_distinct() {
    // C1 adoption absent/present; C2 original Session runtime router; C3 valid
    // native identity+Workspace read but no fine definition right. R1 absent
    // => retain the original response; R2 present => fine read refusal; R3 the
    // new personal directory still refuses missing adoption. This probes the
    // changed composition boundary; the original runtime/IAM guards retain
    // their separate conformance coverage and are not simulated here.
    let native = native(sqlite_in_memory_store("unadopted_session").unwrap()).await;
    register(&native.store, &["a-visible"]);
    workspace_read(&native.store);
    let raw = Router::new().route("/v1/sessions/s",axum::routing::get(||async {Json(json!({
        "id":"s","type":"session","agent":{"id":"a-visible","version":1},"agent_id":"a-visible","agent_version":1
    }))}));
    let unadopted = super::super::managed_definition_reads::protect(
        raw.clone(),
        Some(native.iam.clone()),
        None,
    );
    assert_eq!(
        raw_call(&unadopted, &native.tokens[0], "/v1/sessions/s")
            .await
            .0,
        StatusCode::OK,
        "R1"
    );
    let adopted = super::super::managed_definition_reads::protect(
        raw,
        Some(native.iam.clone()),
        Some(AgentGovernanceConfig {
            product_id: product(),
        }),
    );
    assert_eq!(
        raw_call(&adopted, &native.tokens[0], "/v1/sessions/s")
            .await
            .0,
        StatusCode::FORBIDDEN,
        "R2"
    );
    let configs = Arc::new(
        awaken_config_store::SqliteConfigStore::open(":memory:")
            .await
            .unwrap(),
    );
    let personal = super::super::routes::router(
        AgentGovernanceStores::new(configs, None),
        Some(native.iam.clone()),
    );
    assert_eq!(
        raw_call(&personal, &native.tokens[0], "/v1/agent-directory")
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE,
        "R3"
    );
}
fn workspace_read<B: SqlConn + Clone + 'static>(store: &SqlStore<B>) {
    awaken_iam_server::PolicyAdminApi::new(store.clone())
        .issue_grant(
            Grant {
                id: GrantId("legacy-workspace-read".into()),
                subject: GrantSubject::Principal(principal("person-a")),
                action_pattern: ActionPattern("awaken.workspace::workspace.read".into()),
                scope: ScopeRef::Workspace {
                    workspace_id: WorkspaceId("workspace".into()),
                },
                effect: Effect::Allow,
            },
            at(),
        )
        .unwrap();
}
async fn raw_call(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    invoke(app, token, "GET", path, Value::Null, "read").await
}
async fn invoke(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
    request_id: &str,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .extension(awaken_authz_enforce::RequestTenancy {
                    workspace_id: "workspace".into(),
                })
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-request-id", request_id)
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn exercise_definitions<B, T>(native: Native<B>, configs: Arc<T>)
where
    B: SqlConn + Clone + 'static,
    T: ScopedConfigRegistry
        + AgentCatalogSnapshotRepository
        + PersonalAgentSelectionRepository
        + 'static,
{
    register(
        &native.store,
        &["a-visible", "b-hidden", "c-visible", "d-hidden"],
    );
    workspace_read(&native.store);
    seed_config(configs.as_ref()).await;
    let scope = ScopeId::from("workspace");
    let (plane, _) = crate::managed_agents::test_support::plane_with_store(configs.clone());
    let repository = Arc::new(crate::ConfigPlaneManagedAgentRepository::request_scoped(
        plane.clone(),
        awaken_time::system_clock(),
    ));
    let raw = crate::config_routes::config_router(plane.clone()).merge(
        crate::config_routes::agent_registry_routers(repository, None, None),
    );
    let guarded = crate::protect_management_router(
        raw,
        plane.management_audit_plane(),
        None,
        Some(native.iam.clone()),
        None,
    );
    let views = AgentGovernanceStores::new(
        configs.clone(),
        Some(AgentGovernanceConfig {
            product_id: product(),
        }),
    );
    let app = super::super::legacy::protect(
        guarded,
        views,
        Some(native.iam.clone()),
        plane.clone(),
        awaken_time::system_clock(),
    );
    let token = &native.tokens[0];
    assert_eq!(
        raw_call(&app, &native.service_token, "/v1/config/agents/a-visible")
            .await
            .0,
        StatusCode::FORBIDDEN,
        "a runtime Service without exact fine read cannot read a shared definition"
    );
    let response = raw_call(&app, token, "/v1/config/agents/b-hidden").await;
    assert_eq!(
        response.0,
        StatusCode::FORBIDDEN,
        "Workspace read must not expose stored hidden definitions: {}",
        response.1
    );
    assert_eq!(
        response.1["error"]["type"], "permission_error",
        "legacy Managed denial retains the original SDK envelope"
    );
    let snapshot = configs
        .agent_catalog_snapshot(
            &scope,
            &AgentSelectionOwner::new(ISSUER.into(), "person-a".into()).unwrap(),
            100,
        )
        .await
        .unwrap();
    let hidden = snapshot
        .entries
        .iter()
        .find(|e| e.head.config.id == "b-hidden")
        .unwrap()
        .publication
        .as_ref()
        .unwrap()
        .fingerprint
        .clone();
    for path in [
        "/v1/agents/b-hidden".into(),
        "/v1/agents/b-hidden/versions".into(),
        "/v1/config/agents/b-hidden/versions".into(),
        "/v1/config/agents/%62-hidden".into(),
        format!("/v1/config/publications/{hidden}"),
        format!("/v1/config/publications/{hidden}/export"),
    ] {
        let response = raw_call(&app, token, &path).await;
        assert_eq!(response.0, StatusCode::FORBIDDEN, "{path}: {}", response.1);
        assert!(!response.1.to_string().contains("private definition"));
    }
    for path in ["/v1/config/agents", "/v1/agents?limit=1"] {
        let response = raw_call(&app, token, path).await;
        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1["data"], json!([]));
    }
    grant(
        &native.store,
        "person-a",
        "a-visible",
        "agent.definition.read",
    );
    assert_eq!(
        raw_call(&app, token, "/v1/config/agents/a-visible").await.0,
        StatusCode::OK
    );
    assert_eq!(
        raw_call(&app, token, "/v1/agents/a-visible").await.0,
        StatusCode::OK
    );
    for date in ["2023-06-01", "2026-10-21", "unknown"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/agents?limit=1")
                    .extension(awaken_authz_enforce::RequestTenancy {
                        workspace_id: "workspace".into(),
                    })
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header("anthropic-version", date)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        if date == "unknown" {
            assert_eq!(status, StatusCode::BAD_REQUEST);
        } else {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body["data"][0].get("execution_identity").is_some(),
                date == "2026-10-21"
            );
        }
    }
    for (header, status) in [
        ("other", StatusCode::FORBIDDEN),
        ("", StatusCode::BAD_REQUEST),
    ] {
        let before = native.faults.strict_calls.load(Ordering::SeqCst);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/config/agents/a-visible")
                    .extension(awaken_authz_enforce::RequestTenancy {
                        workspace_id: "workspace".into(),
                    })
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header("anthropic-workspace-id", header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(
            native.faults.strict_calls.load(Ordering::SeqCst),
            before,
            "native selector reconciliation rejects before fine PDP/definition access"
        );
    }

    for path in [
        "/v1/config/agents/a-visible/versions",
        "/v1/agents/a-visible/versions",
    ] {
        assert_eq!(
            raw_call(&app, token, path).await.0,
            StatusCode::OK,
            "{path}"
        );
    }
    let page = raw_call(&app, token, "/v1/agents?limit=1").await;
    assert_eq!(page.0, StatusCode::OK);
    assert_eq!(page.1["data"][0]["id"], "a-visible");
    assert!(page.1["next_page"].is_null());
    grant(
        &native.store,
        "person-a",
        "c-visible",
        "agent.definition.read",
    );
    let first = raw_call(&app, token, "/v1/agents?limit=1").await.1;
    let cursor = first["next_page"].as_str().unwrap();
    let second = raw_call(&app, token, &format!("/v1/agents?limit=1&page={cursor}")).await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["data"][0]["id"], "c-visible");
    assert!(second.1["next_page"].is_null());
    configs
        .put_config_scoped(&scope, &config("b-hidden", "different hidden draft"))
        .await
        .unwrap();
    assert_eq!(
        raw_call(&app, token, "/v1/agents?limit=1").await.1,
        first,
        "hidden candidate changes cannot alter a healthy visible page/cursor"
    );
    // Actual native registration/proof/transport failures abort the page, never
    // return a partial page or silently skip a candidate.
    for mode in [1, 2, 3, 4] {
        native.faults.mode.store(mode, Ordering::SeqCst);
        let response = raw_call(&app, token, "/v1/config/agents").await;
        assert_eq!(
            response.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "fault mode {mode}"
        );
        assert!(response.1.get("data").is_none());
        native.faults.mode.store(0, Ordering::SeqCst);
        assert_eq!(
            raw_call(&app, token, "/v1/config/agents").await.0,
            StatusCode::OK
        );
    }
    *native.faults.catalog_mutation.lock().unwrap() = Some(configs.clone());
    native.faults.mode.store(6, Ordering::SeqCst);
    assert_eq!(
        raw_call(&app, token, "/v1/config/agents").await.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "same page cannot mix a Config ABA with its old frame"
    );
    native.faults.mode.store(0, Ordering::SeqCst);
    let policy = native.store.clone();
    *native.faults.mutate.lock().unwrap() = Some(Box::new(move || {
        revoke(
            &policy,
            &grant_id("person-a", "c-visible", "agent.definition.read"),
        )
    }));
    native.faults.mode.store(5, Ordering::SeqCst);
    assert_eq!(
        raw_call(&app, token, "/v1/agents?limit=1").await.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "policy fence change cannot mix old and new Allows"
    );
    native.faults.mode.store(0, Ordering::SeqCst);
    assert!(
        raw_call(&app, token, "/v1/config/agents/c-visible").await.0 == StatusCode::FORBIDDEN,
        "same issued token observes withdrawal"
    );
    for denied in native.rejected_credentials.iter().skip(1) {
        assert_eq!(
            raw_call(&app, denied, "/v1/config/agents/a-visible")
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        raw_call(
            &app,
            &native.rejected_credentials[0],
            "/v1/config/agents/a-visible"
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "same-named Service is not the granted Account"
    );
    assert_eq!(
        raw_call(&app, &native.tokens[1], "/v1/config/agents/a-visible")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    exercise_session_definition_edge(&native).await;
    let visible_publication = snapshot
        .entries
        .iter()
        .find(|entry| entry.head.config.id == "a-visible")
        .unwrap()
        .publication
        .as_ref()
        .unwrap()
        .fingerprint
        .clone();
    let publication = raw_call(
        &app,
        &native.service_token,
        &format!("/v1/config/publications/{visible_publication}"),
    )
    .await;
    assert_eq!(
        publication.0,
        StatusCode::OK,
        "explicit Service read preserves Flow's immutable publication reader"
    );
    assert_eq!(publication.1["agent_id"], "a-visible");
    // The Service's remaining explicit root-read grant permits this original
    // Config read, never collection/management or a native write.
    assert_eq!(
        raw_call(&app, &native.service_token, "/v1/config/agents/a-visible")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        raw_call(&app, &native.service_token, "/v1/config/agents")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        invoke(
            &app,
            &native.service_token,
            "PUT",
            "/v1/config/agents/a-visible",
            json!({"generation":3,"instructions":"SERVICE CANNOT WRITE"}),
            "service-no-write"
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        raw_call(
            &app,
            &native.service_token,
            &format!("/v1/config/publications/{hidden}")
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "publication for another Agent never inherits the Service root grant"
    );
    // A publish or Managed mutation never becomes a responsible review merely
    // because broad Workspace or a resource write action was granted.
    let body = json!({"generation":4,"name":"reviewed draft","model":{"id":"m","provider_identity_ref":"p","backend_ref":"b"},"system":"human-reviewed instructions"});
    let current = configs
        .get_config_revision_scoped(&scope, "a-visible")
        .await
        .unwrap()
        .unwrap();
    let mut body = body;
    body["generation"] = json!(current.revision);
    let action = "http:PUT:/v1/config/agents/a-visible";
    assert_eq!(
        invoke(
            &app,
            token,
            "PUT",
            "/v1/config/agents/a-visible",
            body.clone(),
            "denied-draft"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(
        plane
            .management_audit_plane()
            .get(&scope, action, "denied-draft")
            .await
            .unwrap()
            .is_none(),
        "fine denial precedes audit intent as well as definition facts"
    );
    grant(
        &native.store,
        "person-a",
        "a-visible",
        "agent.definition.write",
    );
    let mut no_baseline = body.clone();
    no_baseline.as_object_mut().unwrap().remove("generation");
    assert_eq!(
        invoke(
            &app,
            token,
            "PUT",
            "/v1/config/agents/a-visible",
            no_baseline,
            "missing-baseline"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert!(
        plane
            .management_audit_plane()
            .get(&scope, action, "missing-baseline")
            .await
            .unwrap()
            .is_none()
    );
    let result = invoke(
        &app,
        token,
        "PUT",
        "/v1/config/agents/a-visible",
        body,
        "reviewed-draft",
    )
    .await;
    assert_eq!(result.0, StatusCode::OK, "{}", result.1);
    assert_eq!(result.1["generation"], current.revision + 1);
    assert!(result.1.get("system").is_none());
    assert_eq!(
        configs
            .get_config_revision_scoped(&scope, "a-visible")
            .await
            .unwrap()
            .unwrap()
            .config
            .instructions,
        "human-reviewed instructions"
    );
    for (method, path) in [
        ("POST", "/v1/agents"),
        ("POST", "/v1/agents/a-visible"),
        ("POST", "/v1/agents/a-visible/archive"),
        ("POST", "/v1/config/agents/a-visible/publish"),
        ("POST", "/v1/config/agent-previews/preview-unreviewed"),
        ("GET", "/v1/awaken/agents/execution-evidence"),
    ] {
        let before = configs.agent_catalog_epoch(&scope).await.unwrap();
        assert_eq!(
            invoke(
                &app,
                token,
                method,
                path,
                json!({}),
                "closed-legacy-lifecycle"
            )
            .await
            .0,
            StatusCode::FORBIDDEN,
            "{path}"
        );
        assert_eq!(
            configs.agent_catalog_epoch(&scope).await.unwrap(),
            before,
            "closed lifecycle has no Config effects"
        );
        assert!(
            plane
                .management_audit_plane()
                .get(
                    &scope,
                    &format!("http:{method}:{path}"),
                    "closed-legacy-lifecycle"
                )
                .await
                .unwrap()
                .is_none()
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_legacy_definition_requires_fine_permission_not_workspace_read() {
    let native = native(sqlite_in_memory_store("legacy_definitions").unwrap()).await;
    exercise_definitions(
        native,
        Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory()
                .await
                .unwrap(),
        ),
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_legacy_definition_requires_fine_permission_not_workspace_read() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let pool = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
    let configs = Arc::new(
        awaken_config_store::PostgresConfigStore::with_pool(pool.clone())
            .await
            .unwrap(),
    );
    let store = tokio::task::spawn_blocking(move || {
        awaken_iam_server::postgres_migrated_store(&url, "iam").unwrap()
    })
    .await
    .unwrap();
    exercise_definitions(native(store).await, configs).await;
    pool.close().await;
}

// The downstream DTO/commit counter is an explicit HTTP-adapter fixture here.
// Native Managed storage/history/schema are qualified by its own protocol tests;
// this transcript qualifies actual native IAM and the outer independent guards.
async fn exercise_session_definition_edge<B: SqlConn + Clone + 'static>(native: &Native<B>) {
    use axum::routing::get;
    let pap = AuthorizationProfileAdmin::new(Arc::new(native.store.clone()));
    let revision = pap
        .create_draft(crate::authz::hosted_runtime_authorization_profile())
        .unwrap();
    assert!(
        pap.validate(&revision.namespace, revision.revision)
            .unwrap()
            .valid
    );
    pap.activate(
        &mut AuthzApi::new(),
        &PolicySnapshot::default(),
        &revision.namespace,
        revision.revision,
        ActivateAuthorizationProfile {
            expected_active_revision: None,
        },
    )
    .unwrap();
    let grant_run = |action: &str| {
        awaken_iam_server::PolicyAdminApi::new(native.store.clone())
            .issue_grant(
                Grant {
                    id: GrantId(format!("session:{action}")),
                    subject: GrantSubject::Principal(principal("person-a")),
                    action_pattern: ActionPattern(format!("awaken.runtime::{action}")),
                    scope: ScopeRef::Workspace {
                        workspace_id: WorkspaceId("workspace".into()),
                    },
                    effect: Effect::Allow,
                },
                at(),
            )
            .unwrap();
    };
    let data = json!({"id":"s","status":"idle","agent":{"id":"a-visible","type":"agent","version":1,"model":{"id":"private-root-model"},"system":"ROOT DEFINITION","multiagent":{"agents":[{"id":"b-hidden","type":"agent","version":1,"model":{"id":"private-child-model"},"system":"CHILD DEFINITION"}]}},"metadata":{"agent":{"id":"d-hidden","system":"person's own metadata"}},"content":{"agent":{"id":"d-hidden","system":"person's own result"}}});
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let read_count = reads.clone();
    let read_data = data.clone();
    let write_count = writes.clone();
    let write_data = data.clone();
    let account_store = native.store.clone();
    let raw = Router::new().route(
        "/v1/sessions/s",
        get(move || {
            let reads = read_count.clone();
            let data = read_data.clone();
            async move {
                reads.fetch_add(1, Ordering::SeqCst);
                Json(data)
            }
        })
        .post(move |request: Request| {
            let writes = write_count.clone();
            let data = write_data.clone();
            let store = account_store.clone();
            async move {
                assert_eq!(
                    request.headers()["idempotency-key"],
                    "original-native-mutation"
                );
                writes.fetch_add(1, Ordering::SeqCst);
                tokio::task::spawn_blocking(move || {
                    AccountRepository::upsert(
                        &store,
                        Account {
                            id: AccountId("person-a".into()),
                            status: AccountStatus::Disabled,
                            display_name: None,
                            created_at: at(),
                            updated_at: at(),
                        },
                    )
                    .unwrap()
                })
                .await
                .unwrap();
                (StatusCode::FORBIDDEN, Json(data))
            }
        }),
    );
    let guarded = crate::protect_authorized_router(raw, None, Some(native.iam.clone()));
    let app = crate::protect_managed_agent_definitions(
        guarded,
        Some(native.iam.clone()),
        Some(AgentGovernanceConfig {
            product_id: product(),
        }),
    );
    let token = &native.tokens[0];
    // Definition read on its own cannot enter even the fixture Session reader.
    assert_eq!(
        raw_call(&app, token, "/v1/sessions/s").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    grant_run("run.read");
    grant_run("run.create");
    assert_eq!(
        raw_call(&app, token, "/v1/sessions/s").await.0,
        StatusCode::FORBIDDEN,
        "root read never exposes an unqualified frozen child"
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let mutation = || {
        Request::builder()
            .method("POST")
            .uri("/v1/sessions/s")
            .extension(awaken_authz_enforce::RequestTenancy {
                workspace_id: "workspace".into(),
            })
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "original-native-mutation")
            .body(Body::from("{}"))
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(mutation()).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "pre-write native GET/fine denial has zero persistent attempts"
    );
    grant(
        &native.store,
        "person-a",
        "b-hidden",
        "agent.definition.read",
    );
    let before = reads.load(Ordering::SeqCst);
    native.faults.mode.store(7, Ordering::SeqCst);
    assert_eq!(
        raw_call(&app, token, "/v1/sessions/s").await.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "a single failed current runtime decision must not retry into success"
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "failed current run decision never reaches the Session reader"
    );
    native.faults.mode.store(0, Ordering::SeqCst);
    let observed = raw_call(&app, token, "/v1/sessions/s").await;
    assert_eq!(observed.0, StatusCode::OK);
    assert_eq!(
        observed.1, data,
        "full official DTO and person's content remain unchanged after both independent permissions"
    );
    for rejected in native.rejected_credentials.iter().skip(1) {
        assert_eq!(
            raw_call(&app, rejected, "/v1/sessions/s").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        raw_call(&app, &native.rejected_credentials[0], "/v1/sessions/s")
            .await
            .0,
        StatusCode::FORBIDDEN,
        "a signed Service is a distinct native principal, never the same-named Account"
    );
    revoke(
        &native.store,
        &grant_id("person-a", "b-hidden", "agent.definition.read"),
    );
    assert_eq!(
        raw_call(&app, token, "/v1/sessions/s").await.0,
        StatusCode::FORBIDDEN,
        "same token observes current frozen-child withdrawal"
    );
    grant(
        &native.store,
        "person-a",
        "b-hidden",
        "agent.definition.read",
    );
    let response = app.clone().oneshot(mutation()).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a failed ending guard after a persistent attempt is unknown even if the inner response says 403"
    );
    let error = to_bytes(response.into_body(), 65536).await.unwrap();
    assert!(!String::from_utf8_lossy(&error).contains("ROOT DEFINITION"));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    AccountRepository::upsert(
        &native.store,
        Account {
            id: AccountId("person-a".into()),
            status: AccountStatus::Active,
            display_name: None,
            created_at: at(),
            updated_at: at(),
        },
    )
    .unwrap();
    exercise_service_session_reader(native, &app, &data, &reads).await;
}

async fn exercise_service_session_reader<B: SqlConn + Clone + 'static>(
    native: &Native<B>,
    app: &Router,
    data: &Value,
    reads: &AtomicUsize,
) {
    let service = PrincipalRef::Service {
        service_id: "flow-service".into(),
    };
    let issue = |id: &str, action: &str, scope: ScopeRef| {
        awaken_iam_server::PolicyAdminApi::new(native.store.clone())
            .issue_grant(
                Grant {
                    id: GrantId(id.into()),
                    subject: GrantSubject::Principal(service.clone()),
                    action_pattern: ActionPattern(action.into()),
                    scope,
                    effect: Effect::Allow,
                },
                at(),
            )
            .unwrap();
    };
    let token = &native.service_token;
    let before = reads.load(Ordering::SeqCst);
    assert_eq!(
        raw_call(app, token, "/v1/sessions/s").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "unprivileged Service cannot read a Session"
    );
    issue(
        "service-run",
        "awaken.runtime::run.read",
        ScopeRef::Workspace {
            workspace_id: WorkspaceId("workspace".into()),
        },
    );
    issue(
        "service-root",
        "awaken.workspace::agent.definition.read",
        target("a-visible"),
    );
    assert_eq!(
        raw_call(app, token, "/v1/sessions/s").await.0,
        StatusCode::FORBIDDEN,
        "runtime and root rights never expose an unqualified child definition"
    );
    issue(
        "service-child",
        "awaken.workspace::agent.definition.read",
        target("b-hidden"),
    );
    let observed = raw_call(app, token, "/v1/sessions/s").await;
    assert_eq!(observed.0, StatusCode::OK);
    assert_eq!(
        &observed.1, data,
        "properly admitted Service retains the native full runtime snapshot"
    );
    let before = reads.load(Ordering::SeqCst);
    native.faults.mode.store(7, Ordering::SeqCst);
    assert_eq!(
        raw_call(app, token, "/v1/sessions/s").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "Service's failed native decision is not retried"
    );
    native.faults.mode.store(0, Ordering::SeqCst);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/sessions/s")
                .extension(awaken_authz_enforce::RequestTenancy {
                    workspace_id: "foreign-workspace".into(),
                })
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(reads.load(Ordering::SeqCst), before);
    revoke(&native.store, &GrantId("service-child".into()));
    assert_eq!(
        raw_call(app, token, "/v1/sessions/s").await.0,
        StatusCode::FORBIDDEN,
        "same Service token observes the current exact definition withdrawal"
    );
}
