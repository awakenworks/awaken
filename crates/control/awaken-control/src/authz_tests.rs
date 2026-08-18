use super::*;
use awaken_iam_contract::AccountId;

#[test]
fn hosted_runtime_profile_is_one_workspace_scoped_lifecycle_contract() {
    use awaken_iam_server::{AuthorizationProfileAdmin, InMemoryStore};

    // Cause graph: canonical Hosted lifecycle vocabulary -> one immutable
    // profile -> the two human access levels plus the workload role. Cloud may
    // bind one human role at an exact Workspace,
    // but cannot add actions or reinterpret their scope.
    //
    // Decision table:
    // | role | action | allowed scope | profile effect |
    // | hosted_admin | run.create/read/resume/cancel | Workspace | allow |
    // | workspace_user | run.read | Workspace | allow |
    // | agent_executor | run.create/read/resume/cancel | Workspace | allow |
    // | either | credential/management action | any | absent/default deny |
    // | release profile | IAM PAP validation | any | valid before deployment |
    let first = hosted_runtime_authorization_profile();
    let second = hosted_runtime_authorization_profile();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    assert_eq!(first.namespace.0, HOSTED_RUNTIME_POLICY_NAMESPACE);
    assert_eq!(
        first
            .document
            .resource_model
            .actions
            .iter()
            .map(|action| action.0.as_str())
            .collect::<Vec<_>>(),
        [
            "awaken.runtime::run.create",
            "awaken.runtime::run.read",
            "awaken.runtime::run.resume",
            "awaken.runtime::run.cancel",
        ]
    );
    assert_eq!(
        first
            .document
            .action_scope_rules
            .iter()
            .map(|rule| rule.action_pattern.as_str())
            .collect::<Vec<_>>(),
        [
            "awaken.runtime::run.create",
            "awaken.runtime::run.read",
            "awaken.runtime::run.resume",
            "awaken.runtime::run.cancel",
        ]
    );
    assert!(
        first
            .document
            .action_scope_rules
            .iter()
            .all(|rule| rule.allowed_scope_kinds == [ScopeKind::Workspace])
    );
    let role_actions = first
        .document
        .grants
        .iter()
        .map(|grant| match &grant.subject {
            GrantSubjectRef::Role { role_id } => (role_id.as_str(), grant.action_pattern.as_str()),
            _ => panic!("Hosted lifecycle grants must be role-owned"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        role_actions,
        [
            (HOSTED_RUNTIME_WORKSPACE_ADMIN_ROLE, "awaken.runtime::run.*"),
            (
                HOSTED_RUNTIME_WORKSPACE_USER_ROLE,
                "awaken.runtime::run.read"
            ),
            (HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE, "awaken.runtime::run.*"),
        ]
    );
    assert!(
        first
            .document
            .grants
            .iter()
            .all(|grant| { grant.scope == ScopeRef::Global && grant.effect == GrantEffect::Allow })
    );
    let pap = AuthorizationProfileAdmin::new(Arc::new(InMemoryStore::new()));
    let draft = pap.create_draft(first).unwrap();
    let validation = pap.validate(&draft.namespace, draft.revision).unwrap();
    assert!(validation.valid, "{:?}", validation.errors);
}

#[test]
fn hosted_runtime_routes_are_projected_from_the_authorization_table() {
    // Cause/effect graph: C1 an IAM route family is Coordinator-exclusive ->
    // E1 export its flat prefix and workspace wrapper; C2 a family is
    // Control-exclusive -> E2 omit both spellings; C3 Environment is shared ->
    // E3 export only its `/work` template in flat and wrapped form.
    // The export is derived from ROUTE_POLICIES, so adding an authenticated
    // Coordinator family without classifying its hosted owner fails this exact
    // decision table instead of silently creating a Cloud-owned second list.
    //
    // Decision table:
    // | rule | family owner | canonical matcher | exported matchers |
    // | R1 | Coordinator | path prefix | flat prefix + Workspace template |
    // | R2 | Control | any | absent |
    // | R3 | split | Environment work template | flat + two-parameter Workspace template |
    let expected_flat = [
        ("prefix", "/v1/sessions"),
        ("prefix", "/v1/dreams"),
        ("prefix", "/v1/a2a"),
        ("prefix", "/v1/message:send"),
        ("prefix", "/v1/message:stream"),
        ("prefix", "/v1/ai-sdk"),
        ("prefix", "/v1/ag-ui"),
        ("prefix", "/v1/durable"),
        ("prefix", "/v1/files"),
        ("prefix", "/v1/skills"),
        ("prefix", "/v1/memory_stores"),
        ("prefix", "/v1/models"),
        ("prefix", "/v1/awaken/sessions"),
        ("prefix", "/v1/application-access-tokens"),
        ("prefix", "/v1/deployments"),
        ("prefix", "/v1/deployment_runs"),
        ("template", "/v1/environments/{environment_id}/work"),
        ("prefix", "/v1/awaken/memory-stores"),
    ];
    let first = hosted_runtime_route_profile();
    let second = hosted_runtime_route_profile();
    assert_eq!(first.schema_version, 1);
    assert_eq!(first.routes.len(), expected_flat.len() * 2);
    for (pair, (kind, flat_path)) in first.routes.chunks_exact(2).zip(expected_flat) {
        let expected_flat_match = match kind {
            "prefix" => HostedRuntimePathMatch::PathPrefix {
                path: flat_path.to_owned(),
            },
            "template" => HostedRuntimePathMatch::PathTemplate {
                path_template: flat_path.to_owned(),
            },
            _ => unreachable!(),
        };
        assert_eq!(&pair[0], &expected_flat_match);
        assert_eq!(
            &pair[1],
            &HostedRuntimePathMatch::PathTemplate {
                path_template: format!(
                    "/v1/workspaces/{{workspace_id}}{}",
                    flat_path.strip_prefix("/v1").unwrap()
                ),
            }
        );
    }
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    for control_path in [
        "/v1/config",
        "/v1/vaults",
        "/v1/agents",
        "/v1/user_profiles",
        "/v1/awaken/environments",
    ] {
        assert!(!first.routes.iter().any(|route| {
            matches!(route, HostedRuntimePathMatch::PathPrefix { path } if path == control_path)
                || matches!(route, HostedRuntimePathMatch::PathTemplate { path_template } if path_template.ends_with(control_path))
        }));
    }
    assert!(
        first
            .routes
            .contains(&HostedRuntimePathMatch::PathTemplate {
                path_template: "/v1/workspaces/{workspace_id}/environments/{environment_id}/work"
                    .to_owned(),
            })
    );
    for runtime_path in [
        "/v1/dreams",
        "/v1/ai-sdk/threads/thread_1/runs",
        "/v1/ag-ui",
        "/v1/durable/threads/thread_1/dispatches",
        "/v1/models",
        "/v1/awaken/sessions/session_1/live-inbox",
    ] {
        assert!(super::action_for(&Method::GET, runtime_path).is_some());
    }
}

#[test]
fn the_route_table_maps_reads_to_read_actions_and_mutations_to_writes() {
    // Cause-effect graph:
    // A request inside one registered Management route family is classified by
    // method -> workspace/apikey/model-supply read or write. A publication
    // reference read is part of config authority; an unknown sibling remains
    // fail-closed.
    //
    // Decision table:
    // | Family                         | Method | Expected action |
    // | model-supply family            | GET    | model_supply.read    |
    // | provider-connections           | POST   | model_supply.connect |
    // | other model-supply family      | mutate | model_supply.write   |
    // | model resolve preview          | POST   | model_supply.read    |
    // | publications/{fingerprint}     | GET    | workspace.read       |
    // | publications/{fingerprint}     | POST   | workspace.write      |
    // | credential family              | read/write | apikey.read/write |
    // | application MCP credential     | POST   | apikey.write         |
    // | unknown config family          | any    | unmapped/deny        |
    let get = Method::GET;
    let post = Method::POST;
    let put = Method::PUT;
    let delete = Method::DELETE;
    // Scoped action shorthand so the assertions below stay line-per-route.
    fn action_for(method: &Method, path: &str) -> Option<&'static str> {
        match super::action_for(method, path) {
            Some(RouteAuthz::Scoped { action, .. }) => Some(action),
            Some(
                RouteAuthz::Application
                | RouteAuthz::Resource { .. }
                | RouteAuthz::HostedRuntime { .. },
            ) => None,
            Some(RouteAuthz::TokenAdmin) => panic!("{path} is not a Scoped route"),
            None => None,
        }
    }
    assert_eq!(
        action_for(&get, "/v1/config/catalog"),
        Some(MODEL_SUPPLY_READ)
    );
    assert_eq!(
        action_for(&get, "/v1/config/capabilities"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/config/brokered-models/refresh"),
        Some(MODEL_SUPPLY_WRITE)
    );
    assert_eq!(
        action_for(&get, "/v1/config/provider-connections"),
        Some(MODEL_SUPPLY_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/config/provider-connections"),
        Some(MODEL_SUPPLY_CONNECT)
    );
    assert_eq!(
        action_for(&get, "/v1/config/executable-models"),
        Some(MODEL_SUPPLY_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/config/model-attributes"),
        Some(MODEL_SUPPLY_WRITE)
    );
    assert_eq!(
        action_for(&get, "/v1/config/provider-descriptors"),
        Some(MODEL_SUPPLY_READ)
    );
    assert_eq!(
        action_for(&get, "/v1/config/publications/fingerprint"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/config/publications/fingerprint"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/config/credentials"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&get, "/v1/config/credentials"),
        Some(APIKEY_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/config/credentials/c1/archive"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/config/credentials/c1/validate"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/config/application-mcp-credentials"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&put, "/v1/config/credential-pools/p1"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/config/inference/resolve"),
        Some(MODEL_SUPPLY_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/config/inference-profiles/p/resolve"),
        Some(MODEL_SUPPLY_READ)
    );
    assert_eq!(action_for(&post, "/v1/vaults"), Some(APIKEY_WRITE));
    // Listing (GET) reads; the SDK `beta.vaults.list` / `credentials.list`.
    assert_eq!(action_for(&get, "/v1/vaults"), Some(APIKEY_READ));
    assert_eq!(
        action_for(&get, "/v1/vaults/v1/credentials"),
        Some(APIKEY_READ)
    );
    assert_eq!(action_for(&get, "/v1/vaults/v1"), Some(APIKEY_READ));
    assert_eq!(action_for(&delete, "/v1/vaults/v1"), Some(APIKEY_WRITE));
    // Archive + update + delete on the credential resource all write.
    assert_eq!(
        action_for(&post, "/v1/vaults/v1/archive"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&delete, "/v1/vaults/v1/credentials/c1"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/vaults/v1/credentials/c1"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/vaults/v1/credentials/c1/archive"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&get, "/v1/vaults/v1/credentials/c1"),
        Some(APIKEY_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/vaults/v1/credentials"),
        Some(APIKEY_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/vaults/v1/credentials/c1/mcp_oauth_validate"),
        Some(APIKEY_WRITE)
    );
    // User-profile family maps to workspace.* by method.
    assert_eq!(action_for(&get, "/v1/user_profiles"), Some(WORKSPACE_READ));
    assert_eq!(
        action_for(&post, "/v1/user_profiles"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/user_profiles/uprof_1/enrollment_url"),
        Some(WORKSPACE_WRITE)
    );
    // Tunnel lifecycle is Cloud-owned but enters this same PEP. Every SDK
    // read stays read-only; create/archive/token/certificate operations write.
    assert_eq!(action_for(&get, "/v1/tunnels"), Some(TUNNEL_MANAGE));
    assert_eq!(
        action_for(&get, "/v1/tunnels/tnl_1/certificates"),
        Some(TUNNEL_MANAGE)
    );
    assert_eq!(
        action_for(&post, "/v1/tunnels/tnl_1/rotate_token"),
        Some(TUNNEL_MANAGE)
    );
    assert_eq!(
        action_for(&post, "/v1/tunnels/tnl_1/certificates/tcrt_1/archive"),
        Some(TUNNEL_MANAGE)
    );
    assert_eq!(
        action_for(&get, "/v1/organizations/tunnels/tnl_1"),
        Some(TUNNEL_MANAGE)
    );
    // Concrete route membership belongs to axum; registered aggregate families
    // receive a total read/write policy while unknown families fail closed.
    assert_eq!(action_for(&get, "/v1/config/unknown"), None);
    assert_eq!(
        action_for(&get, "/v1/config/workspace-context"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        action_for(&post, "/v1/config/catalog"),
        Some(MODEL_SUPPLY_WRITE)
    );
    assert_eq!(
        action_for(&post, "/v1/awaken/sandbox-execution-policies"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        action_for(
            &get,
            "/v1/awaken/environments/env_1/sandbox-execution-policy"
        ),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        action_for(&get, "/v1/awaken/memory-stores/mem_1/dream-policy"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        action_for(&put, "/v1/awaken/memory-stores/mem_1/dream-policy"),
        Some(WORKSPACE_WRITE)
    );
    // An unknown bounded family still fails closed.
    assert_eq!(action_for(&get, "/v1/unknown"), None);
}

#[test]
fn the_token_management_family_delegates_authorization_to_its_handlers() {
    for (method, path) in [
        (Method::POST, "/v1/config/iam/tokens"),
        (Method::GET, "/v1/config/iam/tokens"),
        (Method::DELETE, "/v1/config/iam/tokens/tok_x"),
    ] {
        assert_eq!(action_for(&method, path), Some(RouteAuthz::TokenAdmin));
    }
}

#[test]
fn resource_pep_maps_only_resource_routes_and_is_total_by_method() {
    fn resource_action_for(method: &Method, path: &str) -> Option<&'static str> {
        match super::action_for(method, path) {
            Some(RouteAuthz::Resource { action, .. }) => Some(action),
            _ => None,
        }
    }
    for (path, read, write) in [
        ("/v1/files", FILE_READ, FILE_WRITE),
        ("/v1/files/file_1/content", FILE_READ, FILE_WRITE),
        ("/v1/skills", SKILL_READ, SKILL_WRITE),
        ("/v1/skills/skill_1/versions/1", SKILL_READ, SKILL_WRITE),
        ("/v1/memory_stores", WORKSPACE_READ, WORKSPACE_WRITE),
        (
            "/v1/memory_stores/mem_1/memory_versions/ver_1/redact",
            WORKSPACE_READ,
            WORKSPACE_WRITE,
        ),
    ] {
        assert_eq!(resource_action_for(&Method::GET, path), Some(read));
        assert_eq!(resource_action_for(&Method::HEAD, path), Some(read));
        assert_eq!(resource_action_for(&Method::POST, path), Some(write));
        assert_eq!(resource_action_for(&Method::DELETE, path), Some(write));
    }
    assert_eq!(resource_action_for(&Method::GET, "/v1/sessions"), None);
    assert_eq!(
        resource_action_for(&Method::GET, "/v1/config/catalog"),
        None
    );
}

#[test]
fn runtime_protocol_adapters_use_the_existing_hosted_run_namespace() {
    // Cause/effect graph: a protocol adapter backed by the canonical Run (C1) +
    // HTTP method (C2) -> the already-owned Hosted Run lifecycle action (E1),
    // while the embedded plane keeps its existing resource-Workspace action
    // (E2). Resource adapters remain outside this table (constraint C3). This
    // prevents Cloud from asking the resource profile for broad `workspace.*`
    // and does not create another protocol route, role, or action vocabulary.
    //
    // Decision table:
    // | Run-backed family | Method | Hosted action | Embedded action |
    // | any listed adapter | GET/HEAD | run.read | workspace.read |
    // | any listed adapter | POST/DELETE | run.create | workspace.write |
    // Application-token mint/revoke is in this table because the token is an
    // ephemeral projection of one frozen Session Run, not a credential-vault
    // mutation. The trusted backend still authenticates before this PEP.
    fn actions(method: &Method, path: &str) -> (&'static str, &'static str) {
        match super::action_for(method, path) {
            Some(RouteAuthz::HostedRuntime {
                action,
                embedded_action,
                ..
            }) => (action, embedded_action),
            other => panic!("{method} {path} is not Hosted Runtime: {other:?}"),
        }
    }

    for path in [
        "/v1/sessions/session-1/threads",
        "/v1/a2a",
        "/v1/message:send",
        "/v1/message:stream",
        "/v1/durable/session-1",
        "/v1/awaken/sessions/session-1/live-inbox",
        "/v1/application-access-tokens/token-1",
    ] {
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(actions(&method, path), (RUN_READ, WORKSPACE_READ));
        }
        for method in [Method::POST, Method::DELETE] {
            assert_eq!(actions(&method, path), (RUN_CREATE, WORKSPACE_WRITE));
        }
    }
}

#[test]
fn embedded_resource_profile_authorizes_every_preset_reader_role() {
    // Cause/effect decision table:
    // C1 role has file.read (admin/workspace_admin/workspace_user) -> E1 the
    // qualified resource action is allowed at its bound Workspace; C2 the same
    // principal asks to write as workspace_user -> E2 default-deny. This pins
    // profile grants, qualified resource-role bindings, and scope ancestry as
    // one policy path instead of duplicating the HTTP middleware assertions.
    for (index, role) in ["admin", "workspace_admin", "workspace_user"]
        .into_iter()
        .enumerate()
    {
        let dir = tempfile::tempdir().unwrap();
        let iam = embedded_iam(dir.path());
        let service_id = format!("resource-profile-{index}");
        iam.mint_service_token(TokenSpec {
            token_id: format!("tok_resource_profile_{index}"),
            service_id: service_id.clone(),
            workspace_id: BOOTSTRAP_WORKSPACE.to_owned(),
            role: role.to_owned(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();
        let principal = PrincipalRef::Service { service_id };
        let scope = ScopeRef::Workspace {
            workspace_id: WorkspaceId(BOOTSTRAP_WORKSPACE.to_owned()),
        };
        let read = AuthorizationRequest::direct(
            principal.clone(),
            qualify_action(FILE_READ),
            scope.clone(),
        );
        let read = iam.state.lock().unwrap().authz.authorize(&read);
        assert_eq!(
            read.decision,
            AuthorizationDecision::Allow,
            "C1 {role}: {read:?}"
        );

        if role == "workspace_user" {
            let write = AuthorizationRequest::direct(principal, qualify_action(FILE_WRITE), scope);
            let write = iam.state.lock().unwrap().authz.authorize(&write);
            assert_eq!(write.decision, AuthorizationDecision::Deny, "C2: {write:?}");
        }
    }
}

#[test]
fn only_canonical_utc_timestamps_pass_the_expiry_shape_check() {
    assert!(canonical_timestamp_shape("2027-01-01T00:00:00Z"));
    assert!(!canonical_timestamp_shape("banana"));
    assert!(!canonical_timestamp_shape("2027-01-01T00:00:00+02:00"));
    assert!(!canonical_timestamp_shape("2027-01-01 00:00:00Z"));
    assert!(!canonical_timestamp_shape("2027-01-01T00:00:00.000Z"));
}

#[test]
fn only_preset_role_ids_are_mintable() {
    for role in ["admin", "workspace_admin", "workspace_user"] {
        assert!(is_preset_role(role), "{role}");
    }
    assert!(!is_preset_role("superuser"));
    assert!(!is_preset_role(""));
}

#[test]
fn fresh_token_ids_are_tok_prefixed_hex() {
    let id = fresh_token_id();
    assert!(id.starts_with("tok_"), "{id}");
    assert_eq!(id.len(), 4 + 16);
    assert!(id[4..].bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(fresh_token_id(), fresh_token_id());
}

#[test]
fn timestamps_render_canonical_rfc3339_utc() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // leap-year boundary
    assert_eq!(civil_from_days(20_513), (2026, 3, 1)); // day after 2026-02-28
    let now = now_rfc3339();
    assert_eq!(now.len(), 20);
    assert!(now.ends_with('Z'));
    assert!(now.starts_with("20"));
}

#[test]
fn the_query_fence_reads_workspace_id_pairs() {
    assert_eq!(
        query_workspace_id(Some("workspace_id=ws1")),
        Some("ws1".to_string())
    );
    assert_eq!(
        query_workspace_id(Some("a=b&workspace_id=ws2")),
        Some("ws2".to_string())
    );
    assert_eq!(query_workspace_id(Some("a=b")), None);
    assert_eq!(query_workspace_id(None), None);
}

// ---- CEG §10 additions -------------------------------------------------
//
// These drive the guard + token handlers end-to-end over an in-process axum
// app, reusing the black-box scaffolding shape from
// `awaken-cli/tests/management_{authz,tokens}.rs` but built from THIS crate's
// private surface (`embedded_iam`, `token_router`, `management_guard`) so the
// control crate carries its own coverage. Each test opens a fresh `iam` over
// its own tempdir, so ids/bootstrap state never collide.

use awaken_iam_core::ActionPattern;
use axum::body::Body;
use axum::http::HeaderValue;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

/// A fresh embedded IAM over a throwaway directory; the tempdir is returned so
/// the caller keeps it alive for the test.
fn fresh_iam() -> (tempfile::TempDir, Arc<ManagementAuthz>) {
    let dir = tempfile::tempdir().unwrap();
    let iam = embedded_iam(dir.path());
    (dir, iam)
}

/// The bootstrap admin cleartext boot wrote to `<dir>/admin-token`.
fn admin_token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(ADMIN_TOKEN_FILE)).expect("bootstrap admin-token file")
}

/// The full guarded management app: the token routes + a mapped-route echo
/// fallback, wrapped by [`management_guard`] exactly as `control_router` wires
/// it. The echo answers 200 for any *mapped* route the guard admits.
fn guarded_app(iam: Arc<ManagementAuthz>) -> Router {
    async fn echo() -> Response {
        (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
    }
    Router::new()
        .merge(token_router(iam.clone()))
        .fallback(echo)
        .layer(axum::middleware::from_fn_with_state(iam, management_guard))
}

// Cause/effect decision table:
// fresh setup token -> HttpOnly cookie -> existing management PDP allows admin;
// consumed setup token -> no second cookie.
#[tokio::test]
async fn local_browser_session_enters_the_existing_management_pdp() {
    let (_dir, iam) = fresh_iam();
    let account = AccountId("local-console-admin".into());
    let (browser, handoff) = iam.begin_local_browser(account).unwrap();

    let exchange = awaken_iam_host::local_browser_router(browser)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/local/exchange")
                .header("host", "127.0.0.1:8080")
                .header("origin", "http://127.0.0.1:8080")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "setup_token": handoff.setup_token }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exchange.status(), StatusCode::OK);
    let cookie = exchange
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    let response = guarded_app(iam)
        .oneshot(
            Request::builder()
                .uri("/v1/config/catalog")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// Cause/effect graph for the local console's Resource API edge:
// setup token -> HttpOnly session -> the canonical management PEP selects the
// resource namespace -> trusted workspace stamp; no bearer and no session ->
// 401. This proves the Console can manage Memory and Skills without receiving
// the durable admin API token or traversing a second resource middleware.
#[tokio::test]
async fn local_browser_session_enters_the_existing_resource_pdp() {
    async fn echo(
        axum::Extension(scope): axum::Extension<awaken_tenancy::WorkspaceScope>,
    ) -> Response {
        (StatusCode::OK, Json(json!({ "workspace": scope.0 }))).into_response()
    }

    let (_dir, iam) = fresh_iam();
    let account = AccountId("local-console-resource-admin".into());
    let (browser, handoff) = iam.begin_local_browser(account).unwrap();
    let exchange = awaken_iam_host::local_browser_router(browser)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/local/exchange")
                .header("host", "127.0.0.1:8080")
                .header("origin", "http://127.0.0.1:8080")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "setup_token": handoff.setup_token }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = exchange
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let app = Router::new()
        .fallback(echo)
        .layer(axum::middleware::from_fn_with_state(iam, management_guard));

    let admitted = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/memory_stores")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admitted.status(), StatusCode::OK);

    let missing = app
        .oneshot(
            Request::builder()
                .uri("/v1/skills")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
}

/// The token routes WITHOUT the guard — so the handlers see no
/// [`AuthedPrincipal`] stamp and must fail closed (401).
fn unguarded_token_app(iam: Arc<ManagementAuthz>) -> Router {
    token_router(iam)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        b = b.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn resp_parts(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Mint a token through the embedding handle (the test controls its id).
fn mint(iam: &ManagementAuthz, id: &str, workspace: &str, role: &str) -> String {
    iam.mint_service_token(TokenSpec {
        token_id: id.to_string(),
        service_id: format!("svc-{id}"),
        workspace_id: workspace.to_string(),
        role: role.to_string(),
        created_at: None,
        expires_at: None,
    })
    .unwrap()
}

// -- management_guard (F25) ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mg1_unmapped_route_is_403_no_action_mapped() {
    let (_dir, iam) = fresh_iam();
    let app = guarded_app(iam);
    // Unmapped fails closed BEFORE the token check — no credential needed.
    let (s, err) = call(&app, "GET", "/v1/config/unknown", None, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no management action"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg1b_only_the_read_only_a2a_well_known_card_bypasses_authentication() {
    // Causal boundary: anonymous peer -> discovery card -> authenticated A2A
    // operation. Making the whole A2A prefix public would turn discovery into
    // execution authority; guarding the well-known card makes discovery fail.
    let (_dir, iam) = fresh_iam();
    let app = guarded_app(iam);
    let (card_status, card) = call(&app, "GET", "/.well-known/agent-card.json", None, None).await;
    assert_eq!(card_status, StatusCode::OK, "{card}");

    let (run_status, run_error) =
        call(&app, "POST", "/v1/a2a/message:send", None, Some(json!({}))).await;
    assert_eq!(run_status, StatusCode::UNAUTHORIZED, "{run_error}");
    assert_eq!(run_error["error"]["type"], json!("authentication_error"));

    let (write_status, write_error) = call(
        &app,
        "POST",
        "/.well-known/agent-card.json",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(write_status, StatusCode::FORBIDDEN, "{write_error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn mg2_missing_token_is_401() {
    let (_dir, iam) = fresh_iam();
    let app = guarded_app(iam);
    let (s, err) = call(&app, "GET", "/v1/config/catalog", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{err}");
    assert_eq!(err["error"]["type"], json!("authentication_error"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mg3_expired_token_is_401_expired() {
    let (_dir, iam) = fresh_iam();
    let expired = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_expired".into(),
            service_id: "svc-expired".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "admin".into(),
            created_at: Some("2020-01-01T00:00:00Z".into()),
            expires_at: Some("2020-01-02T00:00:00Z".into()),
        })
        .unwrap();
    let app = guarded_app(iam);
    let (s, err) = call(&app, "GET", "/v1/config/catalog", Some(&expired), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expired")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg4_revoked_token_is_401_revoked() {
    let (_dir, iam) = fresh_iam();
    let cleartext = mint(&iam, "tok_revoked", BOOTSTRAP_WORKSPACE, "admin");
    iam.revoke_token("tok_revoked").unwrap();
    let app = guarded_app(iam);
    let (s, err) = call(&app, "GET", "/v1/config/catalog", Some(&cleartext), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("revoked")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg5_invalid_token_is_401_invalid() {
    let (_dir, iam) = fresh_iam();
    let app = guarded_app(iam);
    let (s, err) = call(&app, "GET", "/v1/config/catalog", Some("garbage"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg6_token_admin_rejects_an_unregistered_foreign_workspace() {
    // TokenAdmin delegates to the target-scope PDP. Runtime's bootstrap admin
    // is Org-bound, and only the platform-provisioned Workspace is registered
    // below that Org, so an arbitrary body id cannot create a new tenant edge.
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, minted) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({ "workspace_id": "wrkspc_other", "role": "workspace_admin" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{minted}");
}

#[tokio::test(flavor = "multi_thread")]
async fn mg7_path_workspace_mismatch_is_403() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    // Stamp a foreign path tenancy (as `workspace_path_scope` would) — it
    // must equal the token's workspace or the guard fences it.
    let mut req = Request::builder()
        .method("GET")
        .uri("/v1/config/catalog")
        .header("authorization", format!("Bearer {bootstrap}"))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(awaken_authz_enforce::RequestTenancy {
            workspace_id: "wrkspc_other".into(),
        });
    let resp = app.clone().oneshot(req).await.unwrap();
    let (s, err) = resp_parts(resp).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("workspace path"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg8_query_workspace_mismatch_is_403() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "GET",
        "/v1/config/catalog?workspace_id=wrkspc_other",
        Some(&bootstrap),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("workspace_id does not match"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg9_oversize_body_is_413() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let big = "x".repeat(3 * 1024 * 1024);
    let (s, _) = call(
        &app,
        "POST",
        "/v1/config/credentials",
        Some(&bootstrap),
        Some(json!({ "workspace_id": BOOTSTRAP_WORKSPACE, "blob": big })),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn mg10_body_workspace_mismatch_is_403() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/credentials",
        Some(&bootstrap),
        Some(json!({ "workspace_id": "wrkspc_other", "kind": "vault" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mg11a_allow_passes_through() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, body) = call(&app, "GET", "/v1/config/catalog", Some(&bootstrap), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "ok": true }));
}

#[tokio::test(flavor = "multi_thread")]
async fn mg11a_authenticated_workspace_is_stamped_for_inner_layers() {
    async fn scope_echo(
        axum::Extension(scope): axum::Extension<awaken_tenancy::WorkspaceScope>,
    ) -> Response {
        (StatusCode::OK, scope.0).into_response()
    }

    let (_dir, iam) = fresh_iam();
    let token = mint(&iam, "tok_scope", "wrkspc_scope", "workspace_user");
    let app = Router::new()
        .route("/v1/config/catalog", axum::routing::get(scope_echo))
        .layer(axum::middleware::from_fn_with_state(iam, management_guard));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/config/catalog")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "wrkspc_scope"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg11b_require_approval_is_403_approval() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    // Install a RequireApproval grant on `model_supply.read` for the bootstrap
    // principal (RequireApproval > Allow), so an otherwise-Allowed read is
    // approval-gated — which P1 cannot discharge, so it 403s.
    let (principal, _) = iam.authenticate(&bootstrap).unwrap();
    iam.state
        .lock()
        .unwrap()
        .authz
        .policy_mut()
        .add_grant(Grant {
            id: GrantId("ceg-approval".into()),
            subject: GrantSubject::Principal(principal),
            action_pattern: ActionPattern(qualify_action(MODEL_SUPPLY_READ).0),
            scope: ScopeRef::Global,
            effect: Effect::RequireApproval,
        });
    let app = guarded_app(iam);
    let (s, err) = call(&app, "GET", "/v1/config/catalog", Some(&bootstrap), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("approval"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mg11c_deny_is_403() {
    let (_dir, iam) = fresh_iam();
    // workspace_user holds no apikey pattern — reading credentials Denies.
    let user = mint(&iam, "tok_user", BOOTSTRAP_WORKSPACE, "workspace_user");
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "GET",
        &format!("/v1/config/credentials?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&user),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not authorize"),
        "{err}"
    );
}

// -- action_for (F26) ----------------------------------------------------

#[test]
fn af2_credential_id_routes_map_read_and_write_sub_actions() {
    // GET /{id} reads; the write sub-route is `/{id}/archive`. A bare
    // PUT/{id} has NO mapping (there is no such admin route) → None, which
    // the guard turns into a fail-closed 403. (The CEG line's literal
    // "PUT credentials/{id}→APIKEY_WRITE" does not exist in the route table;
    // asserted here as the actual, correct behavior.)
    assert_eq!(
        action_for(&Method::GET, "/v1/config/credentials/c1"),
        Some(RouteAuthz::Scoped {
            action: APIKEY_READ,
            scope: ScopeClass::Workspace,
        })
    );
    assert_eq!(
        action_for(&Method::POST, "/v1/config/credentials"),
        Some(RouteAuthz::Scoped {
            action: APIKEY_WRITE,
            scope: ScopeClass::Workspace,
        })
    );
    assert_eq!(
        action_for(&Method::POST, "/v1/config/credentials/c1/archive"),
        Some(RouteAuthz::Scoped {
            action: APIKEY_WRITE,
            scope: ScopeClass::Workspace,
        })
    );
    assert_eq!(
        action_for(&Method::PUT, "/v1/config/credentials/c1"),
        Some(RouteAuthz::Scoped {
            action: APIKEY_WRITE,
            scope: ScopeClass::Workspace,
        })
    );
}

#[test]
fn af_covers_the_deployment_environment_and_agent_families() {
    // The route table is the authz decision point: a mutation accidentally
    // mapped to a `*.read` action would be a silent authz fail-open. These
    // families were unasserted; lock every read→read / mutation→write row (and
    // the fail-closed `None` rows) so a regression cannot loosen them.
    fn scoped(method: Method, path: &str) -> Option<&'static str> {
        match super::action_for(&method, path) {
            Some(RouteAuthz::Scoped { action, .. }) => Some(action),
            Some(
                RouteAuthz::Application
                | RouteAuthz::Resource { .. }
                | RouteAuthz::HostedRuntime { .. },
            ) => None,
            Some(RouteAuthz::TokenAdmin) => panic!("{path} is TokenAdmin, not Scoped"),
            None => None,
        }
    }
    let get = Method::GET;
    let post = Method::POST;
    let put = Method::PUT;

    // Retired catalog CRUD routes have no authorization mapping, so accidentally
    // remounting a handler cannot revive the parallel authoring path.
    assert_eq!(scoped(get.clone(), "/v1/config/endpoints/ep1"), None);
    assert_eq!(scoped(put.clone(), "/v1/config/endpoints/ep1"), None);
    // Model-supply route ownership is covered by the classifier decision table
    // above; this matrix owns the distinct authoring-Agent and deployment rows.
    // -- config: authoring agents --
    assert_eq!(
        scoped(get.clone(), "/v1/config/agents"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(post.clone(), "/v1/config/agents/a1/validate"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(post.clone(), "/v1/config/agents/a1/publish"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(get.clone(), "/v1/config/agents/a1"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(put.clone(), "/v1/config/agents/a1"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(post.clone(), "/v1/config/agent-previews/preview-1"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(Method::DELETE, "/v1/config/agent-previews/preview-1"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(scoped(get.clone(), "/v1/config/projects"), None);
    assert_eq!(
        scoped(put.clone(), "/v1/config/projects/pr1/agents/a1/mcp"),
        None,
        "Project routes are outside awaken and must fail closed"
    );

    // -- the vault credential archive sub-action writes --
    assert_eq!(
        scoped(post.clone(), "/v1/vaults/v1/credentials/c1/archive"),
        Some(APIKEY_WRITE)
    );

    // -- the public agent registry (distinct from /v1/config/agents authoring) --
    assert_eq!(scoped(get.clone(), "/v1/agents"), Some(WORKSPACE_READ));
    assert_eq!(scoped(post.clone(), "/v1/agents"), Some(WORKSPACE_WRITE));
    assert_eq!(scoped(get.clone(), "/v1/agents/a1"), Some(WORKSPACE_READ));
    assert_eq!(scoped(put.clone(), "/v1/agents/a1"), Some(WORKSPACE_WRITE));
    assert_eq!(
        scoped(get.clone(), "/v1/agents/a1/versions"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(post.clone(), "/v1/agents/a1/disable"),
        Some(WORKSPACE_WRITE),
        "the family guard runs before the router returns 404 for unsupported subpaths"
    );
    assert_eq!(
        scoped(post.clone(), "/v1/agents/a1/archive"),
        Some(WORKSPACE_WRITE)
    );

    // -- deployments + deployment runs --
    assert_eq!(scoped(get.clone(), "/v1/deployments"), Some(WORKSPACE_READ));
    assert_eq!(
        scoped(post.clone(), "/v1/deployments"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(get.clone(), "/v1/deployments/d1"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(put.clone(), "/v1/deployments/d1"),
        Some(WORKSPACE_WRITE)
    );
    for action in ["archive", "pause", "unpause", "run"] {
        assert_eq!(
            scoped(post.clone(), &format!("/v1/deployments/d1/{action}")),
            Some(WORKSPACE_WRITE),
            "deployments/{action} must write"
        );
    }
    assert_eq!(
        scoped(get.clone(), "/v1/deployment_runs"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(get.clone(), "/v1/deployment_runs/r1"),
        Some(WORKSPACE_READ)
    );
    // Axum owns the fact that deployment_runs currently exposes no POST; if one is
    // mounted later, the family policy already classifies it as a write.
    assert_eq!(
        scoped(post.clone(), "/v1/deployment_runs"),
        Some(WORKSPACE_WRITE)
    );

    // -- environments + the self-hosted work queue --
    assert_eq!(
        scoped(get.clone(), "/v1/environments"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(post.clone(), "/v1/environments"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(get.clone(), "/v1/environments/e1"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(put.clone(), "/v1/environments/e1"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(post.clone(), "/v1/environments/e1/archive"),
        Some(WORKSPACE_WRITE)
    );
    assert_eq!(
        scoped(get.clone(), "/v1/environments/e1/work"),
        Some(WORKSPACE_READ)
    );
    // poll + stats are GET reads (they lease/observe, not mutate).
    assert_eq!(
        scoped(get.clone(), "/v1/environments/e1/work/poll"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(get.clone(), "/v1/environments/e1/work/stats"),
        Some(WORKSPACE_READ)
    );
    // the bare work item: GET retrieves (read), POST updates (write).
    assert_eq!(
        scoped(get.clone(), "/v1/environments/e1/work/w1"),
        Some(WORKSPACE_READ)
    );
    assert_eq!(
        scoped(post.clone(), "/v1/environments/e1/work/w1"),
        Some(WORKSPACE_WRITE)
    );
    for action in ["ack", "heartbeat", "stop"] {
        assert_eq!(
            scoped(
                post.clone(),
                &format!("/v1/environments/e1/work/w1/{action}")
            ),
            Some(WORKSPACE_WRITE),
            "work/{action} must write"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mg12_read_only_role_reads_but_a_workspace_write_denies() {
    // A fresh preset role (`workspace_restricted_developer`) that holds
    // `workspace.read` but no write: it passes a deployments read yet is denied
    // a deployments write at a NON-credential surface — the write-deny mirror of
    // the existing credential-read-deny (mg11c), proving the deny path is not
    // credential-namespace-specific.
    let (_dir, iam) = fresh_iam();
    let dev = mint(
        &iam,
        "tok_ro_dev",
        BOOTSTRAP_WORKSPACE,
        "workspace_restricted_developer",
    );
    let app = guarded_app(iam);

    let (s, body) = call(&app, "GET", "/v1/deployments", Some(&dev), None).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "read-only role reads deployments: {body}"
    );

    let (s, err) = call(&app, "PUT", "/v1/deployments/d1", Some(&dev), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not authorize"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn af6_workspace_user_reading_credentials_denies() {
    // Role adaptation: workspace_user cannot reach the apikey namespace, so
    // an apikey.read decision is Deny even inside its own workspace.
    let (_dir, iam) = fresh_iam();
    let user = mint(&iam, "tok_af6", BOOTSTRAP_WORKSPACE, "workspace_user");
    let (principal, ws) = iam.authenticate(&user).unwrap();
    assert_eq!(
        iam.authorize(
            principal,
            APIKEY_READ,
            ScopeRef::Workspace { workspace_id: ws }
        ),
        AuthorizationDecision::Deny
    );
}

// -- bearer_token (F27) --------------------------------------------------

#[test]
fn bearer_token_prefers_authorization_then_falls_back_to_x_api_key() {
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_static("Bearer sk-x"));
    assert_eq!(bearer_token(&h).as_deref(), Some("sk-x"));

    // (b) fall back to x-api-key when Authorization is absent.
    let mut h = HeaderMap::new();
    h.insert("x-api-key", HeaderValue::from_static("sk-y"));
    assert_eq!(bearer_token(&h).as_deref(), Some("sk-y"));

    // An explicit malformed/non-Bearer Authorization header never falls back
    // to a weaker x-api-key credential.
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_static("Basic abc"));
    h.insert("x-api-key", HeaderValue::from_static("sk-z"));
    assert_eq!(bearer_token(&h), None);

    // (c) whitespace / empty → None (blank credentials are filtered).
    assert_eq!(bearer_token(&HeaderMap::new()), None);
    let mut h = HeaderMap::new();
    h.insert("x-api-key", HeaderValue::from_static(""));
    assert_eq!(bearer_token(&h), None);
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_static("Bearer   "));
    assert_eq!(bearer_token(&h), None);
}

// -- authenticate (F28) --------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn authenticate_ok_expired_and_invalid() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    // (a) a live token → Ok with its workspace binding.
    let (principal, ws) = iam.authenticate(&bootstrap).unwrap();
    assert_eq!(
        principal,
        PrincipalRef::Service {
            service_id: BOOTSTRAP_PRINCIPAL.into()
        }
    );
    assert_eq!(ws.0, BOOTSTRAP_WORKSPACE);

    // (c) errors pass through with their distinct reason.
    assert!(matches!(
        iam.authenticate("garbage"),
        Err(AuthReject::Invalid)
    ));
    let expired = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_exp_auth".into(),
            service_id: "svc-exp".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "admin".into(),
            created_at: Some("2020-01-01T00:00:00Z".into()),
            expires_at: Some("2020-01-02T00:00:00Z".into()),
        })
        .unwrap();
    assert!(matches!(
        iam.authenticate(&expired),
        Err(AuthReject::Expired)
    ));
    // (b) Ok-but-workspaceless → Invalid is a fail-closed branch that this
    // plane cannot construct: every mintable credential is an API token
    // bound to a workspace (JWTs are never minted here). Skipped-infeasible.
}

// -- authorize_at_target (F29) ------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn authorize_at_target_allow_deny_and_require_approval() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let (admin_principal, _) = iam.authenticate(&bootstrap).unwrap();

    // (a) Allow → None.
    assert!(
        authorize_at_target(
            &iam,
            admin_principal.clone(),
            APIKEY_WRITE,
            BOOTSTRAP_WORKSPACE
        )
        .is_none()
    );

    // (c) Deny → 403.
    let user = mint(&iam, "tok_at_user", BOOTSTRAP_WORKSPACE, "workspace_user");
    let (user_principal, _) = iam.authenticate(&user).unwrap();
    let refusal = authorize_at_target(&iam, user_principal, APIKEY_WRITE, BOOTSTRAP_WORKSPACE)
        .expect("deny yields a refusal");
    assert_eq!(resp_parts(refusal).await.0, StatusCode::FORBIDDEN);

    // (b) RequireApproval → 403 with the approval message.
    iam.state
        .lock()
        .unwrap()
        .authz
        .policy_mut()
        .add_grant(Grant {
            id: GrantId("ceg-at-approval".into()),
            subject: GrantSubject::Principal(admin_principal.clone()),
            action_pattern: ActionPattern(qualify_action(APIKEY_WRITE).0),
            scope: ScopeRef::Global,
            effect: Effect::RequireApproval,
        });
    let refusal = authorize_at_target(&iam, admin_principal, APIKEY_WRITE, BOOTSTRAP_WORKSPACE)
        .expect("require-approval yields a refusal");
    let (s, err) = resp_parts(refusal).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("approval"),
        "{err}"
    );
}

// -- mint (F30) ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mt1_mint_without_the_guard_stamp_is_401() {
    let (_dir, iam) = fresh_iam();
    let app = unguarded_token_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        None,
        Some(json!({ "workspace_id": BOOTSTRAP_WORKSPACE, "role": "admin" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{err}");
    assert!(err["error"]["message"].as_str().unwrap().contains("guard"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt2_mint_oversize_body_is_413() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let big = "x".repeat(3 * 1024 * 1024);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({ "workspace_id": BOOTSTRAP_WORKSPACE, "role": "admin", "blob": big })),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{err}");
    assert_eq!(err["code"], json!("body_too_large"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt3_mint_non_object_body_is_422() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!(123)),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["code"], json!("invalid_token_spec"));
    assert!(err["detail"].as_str().unwrap().contains("JSON object"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt4_mint_missing_workspace_id_is_422() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({ "role": "admin" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["detail"].as_str().unwrap().contains("workspace_id"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt5_mint_missing_role_is_422() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({ "workspace_id": BOOTSTRAP_WORKSPACE })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["detail"].as_str().unwrap().contains("role"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt6_mint_unknown_role_is_422_with_preset_list() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({ "workspace_id": BOOTSTRAP_WORKSPACE, "role": "superuser" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["code"], json!("unknown_role"));
    let detail = err["detail"].as_str().unwrap();
    assert!(detail.contains("superuser"));
    assert!(detail.contains("admin"), "lists the preset roles: {detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn mt7_mint_noncanonical_expiry_is_422() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({
            "workspace_id": BOOTSTRAP_WORKSPACE, "role": "admin", "expires_at": "banana"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["code"], json!("invalid_token_spec"));
    assert!(err["detail"].as_str().unwrap().contains("expires_at"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt8_mint_target_authorization_denied_is_403() {
    let (_dir, iam) = fresh_iam();
    // workspace_user has no apikey.write, so it cannot mint anywhere.
    let user = mint(&iam, "tok_mt8", BOOTSTRAP_WORKSPACE, "workspace_user");
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&user),
        Some(json!({ "workspace_id": BOOTSTRAP_WORKSPACE, "role": "workspace_user" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt9_mint_engine_failure_is_422() {
    // Canonical-shaped expiry that is BEFORE `created_at` (now) passes the
    // shape gate but the engine rejects it → 422 "mint refused".
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({
            "workspace_id": BOOTSTRAP_WORKSPACE, "role": "admin",
            "expires_at": "2020-01-01T00:00:00Z"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["detail"].as_str().unwrap().contains("mint refused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mt10_mint_success_is_201_cleartext_once_and_secret_free_view() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, minted) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({ "workspace_id": BOOTSTRAP_WORKSPACE, "role": "workspace_admin" })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{minted}");
    let cleartext = minted["token"].as_str().unwrap();
    assert!(cleartext.starts_with("sk-awaken-"), "{cleartext}");
    let view = &minted["api_token"];
    assert_eq!(view["workspace_id"], json!(BOOTSTRAP_WORKSPACE));
    assert_eq!(view["role"], json!("workspace_admin"));
    assert!(view["id"].as_str().unwrap().starts_with("tok_"));
    // The view NEVER carries hash/cleartext material.
    assert!(view.get("secret_hash").is_none());
    assert!(!serde_json::to_string(view).unwrap().contains("$argon2"));
}

// -- list (F31) ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn lt1_list_without_the_guard_stamp_is_401() {
    let (_dir, iam) = fresh_iam();
    let app = unguarded_token_app(iam);
    let (s, _) = call(
        &app,
        "GET",
        "/v1/config/iam/tokens?workspace_id=x",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn lt2_list_without_workspace_id_query_is_422() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(&app, "GET", "/v1/config/iam/tokens", Some(&bootstrap), None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["code"], json!("invalid_token_query"));
}

#[tokio::test(flavor = "multi_thread")]
async fn lt3_list_denies_read_scope_and_returns_secret_free_views() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    // A workspace_user cannot read the token list (apikey.read denied).
    let user = mint(&iam, "tok_lt3", BOOTSTRAP_WORKSPACE, "workspace_user");
    let app = guarded_app(iam);
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&user),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // The bootstrap admin lists, secret-free (its own view is present).
    let (s, listed) = call(
        &app,
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&bootstrap),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{listed}");
    let raw = serde_json::to_string(&listed).unwrap();
    assert!(!raw.contains("$argon2"), "list leaks a hash: {raw}");
    let arr = listed.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|t| t["principal_id"] == json!(BOOTSTRAP_PRINCIPAL))
    );
    for view in arr {
        assert!(view.get("secret_hash").is_none());
        assert_eq!(view["workspace_id"], json!(BOOTSTRAP_WORKSPACE));
    }
}

// -- revoke (F32) --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn rt1_revoke_without_the_guard_stamp_is_401() {
    let (_dir, iam) = fresh_iam();
    let app = unguarded_token_app(iam);
    let (s, _) = call(&app, "DELETE", "/v1/config/iam/tokens/tok_x", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn rt2_revoke_unknown_id_is_404() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/tok_missing",
        Some(&bootstrap),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{err}");
    assert_eq!(err["code"], json!("not_found"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rt3_revoke_denied_at_the_tokens_own_workspace_is_403() {
    let (_dir, iam) = fresh_iam();
    // A target token lives in the bootstrap workspace; a workspace_user in
    // the same workspace lacks apikey.write, so revoking it is denied.
    mint(&iam, "tok_target", BOOTSTRAP_WORKSPACE, "workspace_admin");
    let user = mint(&iam, "tok_rt3_user", BOOTSTRAP_WORKSPACE, "workspace_user");
    let app = guarded_app(iam);
    let (s, err) = call(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/tok_target",
        Some(&user),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rt4_revoke_ok_returns_the_revoked_view() {
    let (dir, iam) = fresh_iam();
    let bootstrap = admin_token(dir.path());
    mint(&iam, "tok_rt4", BOOTSTRAP_WORKSPACE, "workspace_admin");
    let app = guarded_app(iam);
    let (s, view) = call(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/tok_rt4",
        Some(&bootstrap),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{view}");
    assert!(view["revoked_at"].as_str().is_some(), "{view}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rt5_a_token_may_revoke_itself_then_subsequent_calls_401() {
    // The feasible half of RT5 (the 500 "other error" branch cannot be
    // provoked without corrupting the directory, so it is skipped-infeasible):
    // self-revocation completes, and the now-revoked token 401s afterward.
    let (_dir, iam) = fresh_iam();
    let cleartext = mint(&iam, "tok_self", BOOTSTRAP_WORKSPACE, "workspace_admin");
    let app = guarded_app(iam);
    let (s, view) = call(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/tok_self",
        Some(&cleartext),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{view}");
    assert!(view["revoked_at"].as_str().is_some());
    let (s, _) = call(&app, "GET", "/v1/config/catalog", Some(&cleartext), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

// -- privilege escalation: the cross-workspace deny (SEC) -----------------
//
// mg6 proves a *Global*-bound bootstrap admin MAY cross into another workspace
// via the TokenAdmin delegation (its `apikey.*` grant is bound at Global, an
// ancestor of every workspace). The mirror — the deny the whole delegation
// exists to enforce — was unproven: a *workspace-bound* admin (role bound at
// `Workspace{A}`) MUST NOT mint / list / revoke tokens for a *different*
// workspace B, because its role binding does not cover B's scope. The guard
// skips the header-equality fence for TokenAdmin routes (mg6), so the deny must
// come from the handler's target-workspace authorization — this pins it.

#[tokio::test(flavor = "multi_thread")]
async fn mg13_workspace_bound_admin_cannot_mint_list_or_revoke_for_another_workspace() {
    let (_dir, iam) = fresh_iam();
    let ws_a = "wrkspc_a";
    let ws_b = "wrkspc_b";
    // A workspace_admin whose ONLY role binding is at `Workspace{ws_a}` (mint
    // writes the binding at the token's workspace scope, never Global).
    let admin_a = mint(&iam, "tok_wsadmin_a", ws_a, "workspace_admin");
    // A victim token living in workspace B — the revoke target.
    mint(&iam, "tok_victim_b", ws_b, "workspace_admin");
    let app = guarded_app(iam);

    // Positive control: the workspace-bound admin CAN mint inside its own
    // workspace (so the deny below is a scope fence, not a broken credential).
    let (s, ok) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&admin_a),
        Some(json!({ "workspace_id": ws_a, "role": "workspace_user" })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CREATED,
        "workspace admin mints in-workspace: {ok}"
    );

    // Deny (mint): a foreign target workspace → 403, NOT a silent cross-mint.
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&admin_a),
        Some(json!({ "workspace_id": ws_b, "role": "workspace_user" })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "cross-workspace mint must 403: {err}"
    );
    assert_eq!(err["error"]["type"], json!("permission_error"));

    // Deny (list): reading another workspace's token list → 403 (apikey.read at
    // B is not covered by the A-scoped binding).
    let (s, err) = call(
        &app,
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={ws_b}"),
        Some(&admin_a),
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "cross-workspace list must 403: {err}"
    );

    // Deny (revoke): revoking a token that lives in workspace B → 403 (the
    // handler authorizes at the TOKEN'S workspace, which A does not cover).
    let (s, err) = call(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/tok_victim_b",
        Some(&admin_a),
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "cross-workspace revoke must 403: {err}"
    );
}

// -- restart hydration (SEC) ---------------------------------------------
//
// The embedded IAM's live evaluator is populated only by hydrating durable
// rows at boot. A restart must therefore re-admit a previously minted token AND
// keep a previously revoked one denied — otherwise a revocation silently lapses
// (fail-open) or a valid credential is lost (fail-closed) on every process
// bounce.

#[tokio::test(flavor = "multi_thread")]
async fn embedded_iam_rehydrates_minted_and_revoked_tokens_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let survivor;
    let revoked;
    {
        let iam = embedded_iam(dir.path());
        survivor = mint(&iam, "tok_survivor", BOOTSTRAP_WORKSPACE, "admin");
        revoked = mint(&iam, "tok_revoked_restart", BOOTSTRAP_WORKSPACE, "admin");
        iam.revoke_token("tok_revoked_restart").unwrap();
    }

    // Reopen over the SAME data dir: tokens already exist, so no re-bootstrap;
    // the durable rows rehydrate into a fresh live evaluator.
    let iam2 = embedded_iam(dir.path());
    let app = guarded_app(iam2);

    // The survivor still authenticates AND authorizes (admin reads the catalog).
    let (s, body) = call(&app, "GET", "/v1/config/catalog", Some(&survivor), None).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "survivor authenticates post-restart: {body}"
    );

    // The revoked token still denies post-restart (the revocation persisted).
    let (s, err) = call(&app, "GET", "/v1/config/catalog", Some(&revoked), None).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "revocation must survive a restart: {err}"
    );
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("revoked"),
        "{err}"
    );
}

#[path = "authz_identity_tests.rs"]
mod identity_tests;
#[path = "authz_token_view_tests.rs"]
mod token_view_tests;
#[path = "authz_tunnel_guard_tests.rs"]
mod tunnel_guard_tests;
