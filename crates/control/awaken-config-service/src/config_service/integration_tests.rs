// HTTP adapter and cross-scope integration tests for Config Service.

#[tokio::test]
async fn get_config_handler_returns_500_on_store_error() {
    // Structural extraction rationale: C1 the adapter calls the unchanged public
    // ConfigService method after it moved to its authoring owner; E1 the existing
    // 500 fail-closed result remains the regression oracle. There is no new
    // decision table because no adapter input or branch was added.
    let (status, _body) = super::get_config(
        State(failing_scoped_plane()),
        Path("a".to_string()),
        default_workspace(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

// F21b: the put handler now reads the versioned preservation authority before
// its CAS; a store failure at that read is an internal error and performs no write.
#[tokio::test]
async fn put_config_handler_returns_500_on_version_authority_error() {
    let (status, _body) = super::put_config(
        State(failing_scoped_plane()),
        default_workspace(),
        Path("a".to_string()),
        Json(json!({ "model": "gpt" })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn request_scope_uses_the_workspace_scope_when_present() {
    // Cause/effect decision table: C1 edge stamp exists, C2 stamp is
    // non-empty. R1 C1+C2 => exact scope; R2 !C1 => reject; R3 C1+!C2 =>
    // reject. No handler may synthesize an ownership coordinate.
    let scope = super::request_scope(Some(Extension(WorkspaceScope("wrkspc_acme".into()))))
        .expect("R1 accepts the exact edge scope");
    assert_eq!(scope.as_str(), "wrkspc_acme");
}

#[test]
fn request_scope_rejects_missing_and_empty_edge_scope() {
    // Same decision table as `request_scope_uses...`: R2 and R3 are
    // indistinguishable fail-closed outcomes at this adapter seam.
    assert!(super::request_scope(None).is_err(), "R2");
    assert!(
        super::request_scope(Some(Extension(WorkspaceScope(" ".into())))).is_err(),
        "R3"
    );
}

#[tokio::test]
async fn handler_rejects_missing_scope_before_parsing_untrusted_config() {
    // Extend the scope cause/effect table with C3=malformed request body.
    // R4 !C1+C3 => 404 Workspace, with no config parsing or persistence;
    // R5 C1+C2+C3 => 400 validation error (covered by handler tests below).
    let (status, _) = super::validate(
        State(static_plane(None)),
        None,
        None,
        Path("mgmt".into()),
        Json(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "R4");
}

fn default_workspace() -> Option<Extension<WorkspaceScope>> {
    Some(Extension(WorkspaceScope(DEFAULT_SCOPE.into())))
}

// ---- get_config handler (F19) ----

#[tokio::test]
async fn get_config_handler_returns_200_when_present() {
    // F19a.
    let plane = static_plane(None);
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &agent_config("mgmt")).await.unwrap();
    let (status, Json(body)) = super::get_config(
        State(plane),
        Path("mgmt".to_string()),
        default_workspace(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "mgmt");
}

#[tokio::test]
async fn get_config_handler_returns_404_when_absent() {
    // F19b: absent (or cross-tenant) → 404, never disclosed.
    let plane = static_plane(None);
    let (status, _body) = super::get_config(
        State(plane),
        Path("ghost".to_string()),
        default_workspace(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- validate handler (F20) ----

#[tokio::test]
async fn validate_handler_returns_400_on_unparseable_body() {
    // F20a: a body the projection can't parse is a 400 (the one non-200 case).
    let plane = static_plane(None);
    let (status, Json(body)) = super::validate(
        State(plane),
        default_workspace(),
        None,
        Path("mgmt".to_string()),
        Json(json!({ "context_policy": 123 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["valid"], json!(false));
}

#[tokio::test]
async fn validate_handler_returns_200_valid_true() {
    // F20b: a parseable, valid config → 200 with valid:true.
    let plane = static_plane(None);
    let (status, Json(body)) = super::validate(
        State(plane),
        default_workspace(),
        None,
        Path("mgmt".to_string()),
        Json(json!({ "model": { "id": "m" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], json!(true), "{body}");
}

#[tokio::test]
async fn validate_handler_returns_200_valid_false_on_compile_failure() {
    // F20c (the F20 decoupling): validation is a query — an *invalid* config
    // still succeeds as a request (200) with valid:false + a routed issue.
    let plane = static_plane(None);
    let (status, Json(body)) = super::validate(
        State(plane),
        default_workspace(),
        None,
        Path("mgmt".to_string()),
        Json(json!({ "model": { "id": "m" }, "tools": ["ghost"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], json!(false));
    assert_eq!(body["issues"][0]["path"], "tools");
}

// ---- put_config handler (F21) ----

#[tokio::test]
async fn put_config_handler_returns_400_on_parse_failure() {
    // F21a.
    let plane = static_plane(None);
    let (status, _body) = super::put_config(
        State(plane),
        default_workspace(),
        Path("mgmt".to_string()),
        Json(json!({ "context_policy": 123 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_config_handler_returns_200_on_success() {
    // F21c: a well-formed body is stored → 200, and is then readable.
    let plane = static_plane(None);
    let (status, Json(body)) = super::put_config(
        State(plane.clone()),
        default_workspace(),
        Path("mgmt".to_string()),
        Json(json!({ "model": { "id": "m" }, "system": "hi" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "mgmt");
    let stored = plane
        .get(&ScopeId::from(DEFAULT_SCOPE), "mgmt")
        .await
        .unwrap();
    assert_eq!(stored.unwrap().instructions, "hi");
}

// ---- publish handler (F23) ----

#[tokio::test]
async fn publish_handler_returns_200_on_success() {
    // F23a.
    let plane = static_plane(None);
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &agent_config("mgmt")).await.unwrap();
    let (status, Json(body)) = super::publish(
        State(plane),
        default_workspace(),
        None,
        Path("mgmt".to_string()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["installed"], json!(true));
}

#[tokio::test]
async fn publish_handler_returns_409_on_unresolvable() {
    // F23b (the status partition): an unresolvable Auto binding → 409.
    let plane = static_plane(Some(Arc::new(ErrResolver)));
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &auto_config("mgmt")).await.unwrap();
    let (status, _body) = super::publish(
        State(plane),
        default_workspace(),
        None,
        Path("mgmt".to_string()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn publish_handler_returns_400_on_other_publish_error() {
    // F23c: any other publish failure (here NotStored) stays a 400.
    let plane = static_plane(None);
    let (status, _body) = super::publish(
        State(plane),
        default_workspace(),
        None,
        Path("ghost".to_string()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserved_publication_requires_an_explicit_execution_workspace() {
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let plane = ConfigPlane::new(
        Arc::new(ConfigService::new(
            Arc::new(FakeResolver),
            Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
        )),
        Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let scope = ScopeId::from(RESERVED_ADMIN_SCOPE);
    plane.put(&scope, &agent_config("mgmt")).await.unwrap();

    let error = plane.publish(&scope, "mgmt").await.unwrap_err();
    assert!(matches!(error, PublishError::ExecutionWorkspaceRequired));

    let publication = plane
        .publish_for_execution_workspace(&scope, "workspace-real", "mgmt")
        .await
        .unwrap();
    assert_eq!(publication.agent_id, "mgmt");
    assert!(catalog.current("__admin", "mgmt").is_none());
    assert!(catalog.current("workspace-real", "mgmt").is_some());
}

// ==== SEC: cross-scope isolation of the config registry ====
//
// The tool catalog fence is covered (`admin_tools_compile_only_in_the_reserved_scope`),
// but the config *registry* fence — that scope A's authored/published config
// is invisible and un-actionable from scope B — was unproven end-to-end
// through the scope edge. A hole here is a cross-tenant config disclosure.

#[tokio::test]
async fn config_registry_is_fenced_across_scopes() {
    let plane = plane_over(Arc::new(SqliteConfigStore::open_in_memory().unwrap()));
    let scope_a = ScopeId::from("wrkspc_a");
    let scope_b = ScopeId::from("wrkspc_b");

    // Author (and it exists) in scope A under an id another scope might reuse.
    plane
        .put(&scope_a, &agent_config("shared-id"))
        .await
        .unwrap();

    // get from B → None (the handler renders this as a 404, never disclosing A).
    assert!(
        plane.get(&scope_b, "shared-id").await.unwrap().is_none(),
        "scope B must not read scope A's config by id"
    );
    // absent from B's list.
    assert!(
        plane.list(&scope_b).await.unwrap().is_empty(),
        "scope B's list must not include scope A's config"
    );
    // publish from B → NotStored: B has nothing by that id to compile.
    let err = plane.publish(&scope_b, "shared-id").await.unwrap_err();
    assert!(
        matches!(err, PublishError::NotStored(_)),
        "scope B must not publish scope A's config: {err:?}"
    );

    // The fence is directional: A still owns and sees its row.
    assert!(plane.get(&scope_a, "shared-id").await.unwrap().is_some());
    assert_eq!(plane.list(&scope_a).await.unwrap().len(), 1);
}

#[tokio::test]
async fn executable_catalog_is_keyed_by_workspace_and_agent_id() {
    // Separate scope-bound registries may legitimately reuse a local Agent id
    // (for example when a router shards configuration storage). The live index
    // must preserve that external Workspace coordinate rather than collapse it.
    let registry_a = SqliteConfigStore::open_in_memory().unwrap();
    let registry_b = SqliteConfigStore::open_in_memory().unwrap();
    let (service, catalog) = test_service_and_catalog();

    let mut a = agent_config("shared-id");
    a.instructions = "workspace A".into();
    ConfigRegistry::put_config(&registry_a, &a).await.unwrap();
    service
        .publish(&scope("wrkspc_a"), &registry_a, &a.id, &[])
        .await
        .unwrap();

    let mut b = agent_config("shared-id");
    b.instructions = "workspace B".into();
    ConfigRegistry::put_config(&registry_b, &b).await.unwrap();
    service
        .publish(&scope("wrkspc_b"), &registry_b, &b.id, &[])
        .await
        .unwrap();

    assert_eq!(
        catalog
            .current("wrkspc_a", "shared-id")
            .unwrap()
            .snapshot
            .resolved_spec
            .instructions,
        "workspace A"
    );
    assert_eq!(
        catalog
            .current("wrkspc_b", "shared-id")
            .unwrap()
            .snapshot
            .resolved_spec
            .instructions,
        "workspace B"
    );
    assert!(
        catalog.current("wrkspc_c", "shared-id").is_none(),
        "an uninstalled Workspace must fail closed even when another Workspace uses the id"
    );
}
