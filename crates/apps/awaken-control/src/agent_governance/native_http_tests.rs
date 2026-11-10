// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Native IAM/Config integration tests observe current owner,
// @formal reason: exact registration and atomic failure; they add no authority or formal-proof claim.
//! Native IAM HTTP/SQL and Config owner integration, separate from deployed UI acceptance.
use super::*;
use awaken_agent_config::catalog_snapshot::AgentCatalogSnapshotRepository;
use awaken_agent_config::personal_selection::{
    AgentSelectionOwner, PersonalAgentSelectionRepository,
};
use awaken_agent_config::{AgentConfig, ScopeId, ScopedConfigRegistry, StoredPublication};
use awaken_iam_contract::*;
use awaken_iam_core::{
    AccountRepository, ActionPattern, AuthorizationProfileRepository, Effect, Grant, GrantId,
    GrantSubject, OrgRepository, Organization, ResourceModelRepository,
};
use awaken_iam_server::{
    AdminAuthPolicy, AuthApi, AuthorizationProfileAdmin, AuthzApi, DaemonState, MintAccessToken,
    ProviderLoginRouteOwner, SqlConn, SqlStore, daemon_router, jwks_router, op_router,
    sqlite_in_memory_store,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt as _;
const ISSUER: &str = "https://agent-governance-native.example";
const AUDIENCE: &str = "awaken-runtime";
fn at() -> Timestamp {
    Timestamp("2026-11-09T00:00:00Z".into())
}
fn principal(account: &str) -> PrincipalRef {
    PrincipalRef::Account {
        account_id: AccountId(account.into()),
    }
}
fn product() -> ProductId {
    ProductId::new("awaken").unwrap()
}
fn target(id: &str) -> ScopeRef {
    ScopeRef::Resource {
        resource_type: ResourceType("awaken.agent".into()),
        resource_id: agent_governance_resource_id(ISSUER, "workspace", id).unwrap(),
    }
}
fn grant_id(account: &str, id: &str, action: &str) -> GrantId {
    GrantId(format!("{account}:{id}:{action}"))
}
fn grant<B: SqlConn + Clone + 'static>(store: &SqlStore<B>, account: &str, id: &str, action: &str) {
    awaken_iam_server::PolicyAdminApi::new(store.clone())
        .issue_grant(
            Grant {
                id: grant_id(account, id, action),
                subject: GrantSubject::Principal(principal(account)),
                action_pattern: ActionPattern(format!("awaken.workspace::{action}")),
                scope: target(id),
                effect: Effect::Allow,
            },
            at(),
        )
        .unwrap();
}
fn revoke<B: SqlConn + Clone + 'static>(store: &SqlStore<B>, id: &GrantId) {
    awaken_iam_server::PolicyAdminApi::new(store.clone())
        .revoke_grant(id, at())
        .unwrap();
}

fn register<B: SqlConn + Clone + 'static>(store: &SqlStore<B>, ids: &[&str]) {
    OrgRepository::upsert(
        store,
        Organization {
            id: OrgId("organization".into()),
            display_name: None,
            owner: principal("person-a"),
            created_at: at(),
            updated_at: at(),
        },
    )
    .unwrap();
    ResourceModelRepository::put_workspace_org(
        store,
        WorkspaceOrgEdge {
            workspace_id: WorkspaceId("workspace".into()),
            org_id: OrgId("organization".into()),
        },
    )
    .unwrap();
    let candidate = agent_governance_authorization_profile(product());
    store
        .register_product_resource_model(&ProductResourceModelRequest {
            product_id: product(),
            resource_model: agent_governance_resource_model(),
        })
        .unwrap();
    let pap = AuthorizationProfileAdmin::new(Arc::new(store.clone()));
    let revision = pap.create_draft(candidate).unwrap();
    let validation = pap
        .validate(&revision.namespace, revision.revision)
        .unwrap();
    assert!(validation.valid, "{:?}", validation.errors);
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
    assert!(
        AuthorizationProfileRepository::active_profile(store, &revision.namespace)
            .unwrap()
            .is_some()
    );
    store
        .apply_resource_projection(&ResourceProjectionBatch {
            product_id: product(),
            org_id: OrgId("organization".into()),
            projection_id: "native-agent-catalog".into(),
            idempotency_key: "native-agent-catalog-1".into(),
            epoch: 1,
            grants: vec![],
            scope_edges: ids
                .iter()
                .map(|id| ResourceParentEdge {
                    resource_type: ResourceType("awaken.agent".into()),
                    resource_id: agent_governance_resource_id(ISSUER, "workspace", id).unwrap(),
                    parent: ScopeRef::Workspace {
                        workspace_id: WorkspaceId("workspace".into()),
                    },
                })
                .collect(),
            retirements: vec![],
        })
        .unwrap();
}
fn config(id: &str, name: &str) -> AgentConfig {
    AgentConfig {
        id: id.into(),
        name: Some(name.into()),
        instructions: format!("private definition for {id}"),
        model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b").unwrap(),
        ..Default::default()
    }
}
async fn seed_config<T: ScopedConfigRegistry>(store: &T) {
    let scope = ScopeId::from("workspace");
    for id in ["a-visible", "b-hidden", "c-visible", "d-hidden"] {
        let original = config(id, &format!("Published {id}"));
        store.put_config_scoped(&scope, &original).await.unwrap();
        let metadata = awaken_runtime_contract::snapshot::AgentSnapshotMetadata {
            source: awaken_runtime_contract::AgentConfigRevisionRef {
                agent_id: awaken_runtime_contract::snapshot::AgentId(id.into()),
                revision: 1,
            },
            publication_version: awaken_runtime_contract::AgentPublicationVersion("v1".into()),
            ..Default::default()
        };
        let snapshot =
            awaken_agent_config::compile_resolved(&original, &[], metadata, |_| true).unwrap();
        store
            .put_publication_if_source_current_scoped(
                &scope,
                &StoredPublication::published_at_revision(snapshot, id, 1, scope.clone()),
            )
            .await
            .unwrap();
        store
            .put_config_scoped(&scope, &config(id, &format!("Secret draft {id}")))
            .await
            .unwrap();
    }
}
#[derive(Default)]
struct Faults {
    mode: AtomicUsize,
    strict_calls: AtomicUsize,
    observation_calls: AtomicUsize,
    mutate: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    catalog_mutation: Mutex<Option<Arc<dyn ScopedConfigRegistry>>>,
}
async fn fault(State(faults): State<Arc<Faults>>, request: Request, next: Next) -> Response {
    let strict = request.uri().path() == "/v1/authorize/resource/registered";
    let observation = request.uri().path() == "/v1/authorize/resource";
    let mode = faults.mode.load(Ordering::SeqCst);
    if strict {
        faults.strict_calls.fetch_add(1, Ordering::SeqCst);
    }
    if observation {
        faults.observation_calls.fetch_add(1, Ordering::SeqCst);
    }
    if (strict && mode == 1)
        || (observation && mode == 2 && faults.mode.swap(0, Ordering::SeqCst) == 2)
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let request = if mode == 7 && observation {
        let (parts, body) = request.into_parts();
        let bytes = to_bytes(body, 65536).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        if value["request"]["action"] == "awaken.runtime::run.read"
            && faults.mode.swap(0, Ordering::SeqCst) == 7
        {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Request::from_parts(parts, Body::from(bytes))
    } else {
        request
    };
    let response = next.run(request).await;
    if !strict || !response.status().is_success() {
        return response;
    }
    if mode == 5
        && let Some(mutate) = faults.mutate.lock().unwrap().take()
    {
        mutate();
    }
    if mode == 6 {
        let store = { faults.catalog_mutation.lock().unwrap().take() };
        if let Some(store) = store {
            let scope = ScopeId::from("workspace");
            let original = store
                .get_config_revision_scoped(&scope, "a-visible")
                .await
                .unwrap()
                .unwrap()
                .config;
            store
                .put_config_scoped(&scope, &config("a-visible", "intermediate draft"))
                .await
                .unwrap();
            store.put_config_scoped(&scope, &original).await.unwrap();
        }
    }
    if !matches!(mode, 3 | 4) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let bytes = to_bytes(body, 65536).await.unwrap();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    if value["evaluation"]["outcome"]["decision"] == "allow" {
        if mode == 3 {
            value["registration"] = Value::Null;
        } else {
            value["evaluation"]["freshness"] = Value::Null;
        }
    }
    Response::from_parts(parts, Body::from(value.to_string()))
}
struct Native<B: SqlConn + Clone + 'static> {
    store: SqlStore<B>,
    iam: Arc<crate::RemoteManagementAuthz>,
    tokens: [String; 2],
    service_token: String,
    rejected_credentials: Vec<String>,
    faults: Arc<Faults>,
    server: tokio::task::JoinHandle<()>,
}
impl<B: SqlConn + Clone + 'static> Drop for Native<B> {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn native<B: SqlConn + Clone + 'static>(store: SqlStore<B>) -> Native<B> {
    let shared = Arc::new(store.clone());
    for id in ["person-a", "person-b"] {
        AccountRepository::upsert(
            &store,
            Account {
                id: AccountId(id.into()),
                status: AccountStatus::Active,
                display_name: None,
                created_at: at(),
                updated_at: at(),
            },
        )
        .unwrap();
    }
    let mut auth = AuthApi::new()
        .with_issuer(ISSUER)
        .with_identity_repositories(shared.clone(), shared.clone(), shared.clone());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut tokens = Vec::new();
    for account in ["person-a", "person-b"] {
        tokens.push(
            auth.mint_access_token(MintAccessToken {
                issuer: ISSUER.into(),
                subject: account.into(),
                audience: AUDIENCE.into(),
                issued_at: now - 1,
                expires_at: now + 3600,
                scopes: vec!["openid".into()],
            })
            .await
            .unwrap(),
        );
    }
    let claims = awaken_iam_server::verify_access_token(&tokens[0], &auth.jwks()).unwrap();
    let authority = auth.token_authority();
    let mut service_claims = claims.clone();
    service_claims.sub = "flow-service".into();
    service_claims.subject_kind = awaken_iam_server::AccessTokenSubjectKind::Service;
    let service_token = authority.sign_claims(&service_claims).await.unwrap();
    let mut rejected_credentials = Vec::new();
    for kind in 0..5 {
        let mut invalid = claims.clone();
        match kind {
            0 => invalid.subject_kind = awaken_iam_server::AccessTokenSubjectKind::Service,
            1 => invalid.iss = "https://other-native.example".into(),
            2 => invalid.aud = "other-audience".into(),
            3 => invalid.exp = now - 1,
            4 => invalid.iat = now + 100,
            _ => unreachable!(),
        }
        rejected_credentials.push(authority.sign_claims(&invalid).await.unwrap());
    }
    let mut delegated = serde_json::to_value(&claims).unwrap();
    delegated["act"] = json!({"sub":"service-actor","subject_kind":"service"});
    rejected_credentials.push(authority.sign_claims(&delegated).await.unwrap());
    rejected_credentials.push("sk-invalid-for-account-management".into());
    let daemon = Arc::new(Mutex::new(
        DaemonState::with_policy_store(
            AuthzApi::new(),
            AdminAuthPolicy::deny_all(),
            shared,
            store.clone(),
            auth.token_authority(),
        )
        .unwrap(),
    ));
    let auth = Arc::new(tokio::sync::Mutex::new(auth));
    let faults = Arc::new(Faults::default());
    let router = daemon_router(daemon.clone())
        .merge(awaken_iam_server::projection_router(daemon))
        .merge(op_router(
            auth.clone(),
            ISSUER,
            ProviderLoginRouteOwner::EmbeddingHost,
        ))
        .merge(jwks_router(auth))
        .layer(axum::middleware::from_fn_with_state(faults.clone(), fault));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        awaken_iam_server::http::serve(listener, router)
            .await
            .unwrap();
    });
    let iam = tokio::task::spawn_blocking(move || {
        crate::RemoteManagementAuthz::connect_with_user_token_source(
            base,
            AUDIENCE.into(),
            ISSUER.into(),
            Arc::new(|| {
                Ok(awaken_agent_contract::RedactedString::new(
                    "native-test-carrier",
                ))
            }),
            Some("native-test-carrier".into()),
            awaken_time::system_clock(),
        )
        .unwrap()
    })
    .await
    .unwrap();
    Native {
        store,
        iam,
        tokens: tokens.try_into().unwrap(),
        service_token,
        rejected_credentials,
        faults,
        server,
    }
}
async fn call(
    app: &Router,
    token: &str,
    workspace: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .extension(awaken_authz_enforce::RequestTenancy {
                    workspace_id: workspace.into(),
                })
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
// Test-only reply fault after delegating the actual SQL transaction. The
// application must not retry or classify a committed effect as "not applied".
struct AfterCommit<T> {
    store: Arc<T>,
    calls: AtomicUsize,
    reads: AtomicUsize,
    on_commit: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    reply: u8,
}
#[async_trait::async_trait]
impl<T: PersonalAgentSelectionRepository + 'static> PersonalAgentSelectionRepository
    for AfterCommit<T>
{
    async fn personal_agent_selection(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
    ) -> Result<
        Option<awaken_agent_config::personal_selection::AgentSelectionState>,
        awaken_agent_config::ConfigStoreError,
    > {
        self.store
            .personal_agent_selection(scope, owner, agent)
            .await
    }
    async fn personal_agent_selection_outcome(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
        key: &str,
    ) -> Result<
        Option<awaken_agent_config::personal_selection::AgentSelectionReceipt>,
        awaken_agent_config::ConfigStoreError,
    > {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.store
            .personal_agent_selection_outcome(scope, owner, agent, key)
            .await
    }
    async fn set_personal_agent_selection(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
        intent: &awaken_agent_config::personal_selection::AgentSelectionIntent,
    ) -> Result<
        Result<
            awaken_agent_config::personal_selection::AgentSelectionReceipt,
            awaken_agent_config::personal_selection::AgentSelectionError,
        >,
        awaken_agent_config::ConfigStoreError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let committed = self
            .store
            .set_personal_agent_selection(scope, owner, agent, intent)
            .await?;
        if committed.is_err() {
            return Ok(committed);
        }
        if let Some(change) = self.on_commit.lock().unwrap().take() {
            change();
        }
        match self.reply {
            0 => Ok(committed),
            1 => Err(awaken_agent_config::ConfigStoreError(
                "injected acknowledgement loss after native commit".into(),
            )),
            2 => {
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(20),
                        std::future::pending::<()>()
                    )
                    .await
                    .is_err()
                );
                Err(awaken_agent_config::ConfigStoreError(
                    "injected acknowledgement timeout after native commit".into(),
                ))
            }
            _ => unreachable!(),
        }
    }
}
async fn committed_unknown<B, T>(native: &Native<B>, configs: &Arc<T>)
where
    B: SqlConn + Clone + 'static,
    T: PersonalAgentSelectionRepository + AgentCatalogSnapshotRepository + 'static,
{
    let owner = AgentSelectionOwner::new(ISSUER.into(), "person-a".into()).unwrap();
    for (reply, key, selected) in [
        (0, "after-identity-withdrawal", true),
        (1, "after-lost-acknowledgement", false),
        (2, "after-timed-out-acknowledgement", true),
    ] {
        let iam_store = native.store.clone();
        let port = Arc::new(AfterCommit {
            store: configs.clone(),
            calls: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            reply,
            on_commit: Mutex::new(Some(Box::new(move || {
                AccountRepository::upsert(
                    &iam_store,
                    Account {
                        id: AccountId("person-a".into()),
                        status: AccountStatus::Disabled,
                        display_name: None,
                        created_at: at(),
                        updated_at: at(),
                    },
                )
                .unwrap();
            }))),
        });
        let mut views = AgentGovernanceStores::new(
            configs.clone(),
            Some(AgentGovernanceConfig {
                product_id: product(),
            }),
        );
        views.selections = port.clone();
        let app = routes::router(views, Some(native.iam.clone()));
        let intent = json!({"request_key":key,"expected_generation":reply,"selected":selected});
        let response = call(
            &app,
            &native.tokens[0],
            "workspace",
            "PUT",
            "/v1/me/agent-selections/c-visible",
            intent.clone(),
        )
        .await;
        let persisted = configs
            .personal_agent_selection_outcome(&ScopeId::from("workspace"), &owner, "c-visible", key)
            .await
            .unwrap()
            .expect("actual SQL effect and original receipt precede the injected reply failure");
        assert_eq!(serde_json::to_value(&persisted.intent).unwrap(), intent);
        assert_eq!(persisted.applied_generation, u64::from(reply) + 1);
        assert_eq!(
            response.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "post-write identity denial/lost acknowledgement/timeout must all remain unknown"
        );
        assert!(
            response.1.get("receipt").is_none(),
            "stale identity receives no prior receipt"
        );
        assert_eq!(
            port.calls.load(Ordering::SeqCst),
            1,
            "no internal mutating retry"
        );
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
        let observed = call(
            &app,
            &native.tokens[0],
            "workspace",
            "GET",
            &format!("/v1/me/agent-selections/c-visible/outcomes/{key}"),
            Value::Null,
        )
        .await;
        assert_eq!(
            observed.0,
            StatusCode::OK,
            "owner read requires restored current qualification"
        );
        assert_eq!(
            observed.1["receipt"],
            serde_json::to_value(&persisted).unwrap()
        );
        assert_eq!(
            port.reads.load(Ordering::SeqCst),
            1,
            "recovery really reads the original outcome"
        );
        assert_eq!(
            port.calls.load(Ordering::SeqCst),
            1,
            "GET recovery issues no second PUT or replacement key"
        );
        assert_eq!(
            configs
                .personal_agent_selection(&ScopeId::from("workspace"), &owner, "c-visible")
                .await
                .unwrap()
                .unwrap()
                .generation,
            u64::from(reply) + 1
        );
    }
}

async fn exercise<B, T>(native: Native<B>, configs: Arc<T>)
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
        &["a-visible", "b-hidden", "c-visible", "d-hidden"],
    );
    for account in ["person-a", "person-b"] {
        for id in ["a-visible", "c-visible"] {
            for action in ["agent.use", "agent.selection.read", "agent.selection.write"] {
                grant(&native.store, account, id, action);
            }
        }
    }
    // Workspace.read is intentionally absent. Its native Deny is only a fence
    // observation; each returned manifest has an exact current Resource grant.
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
    let probe_iam = native.iam.clone();
    let probe_token = token.clone();
    tokio::task::spawn_blocking(move || {
        let person =
            authority::person(&probe_iam, &probe_token).expect("native current Account stage");
        let stamp = authority::observe(&probe_iam, &person, "workspace")
            .expect("native current Workspace fence stage");
        assert!(
            authority::allows(
                &probe_iam,
                &product(),
                &person,
                "workspace",
                "a-visible",
                authority::USE,
                &stamp
            )
            .expect("native exact registration stage")
        );
    })
    .await
    .unwrap();
    let before_rejected = native.faults.strict_calls.load(Ordering::SeqCst);
    for invalid in &native.rejected_credentials {
        assert_eq!(
            call(
                &app,
                invalid,
                "workspace",
                "GET",
                "/v1/agent-directory",
                Value::Null
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                &app,
                invalid,
                "workspace",
                "PUT",
                "/v1/me/agent-selections/a-visible",
                json!({"request_key":"never-write","expected_generation":0,"selected":true})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        native.faults.strict_calls.load(Ordering::SeqCst),
        before_rejected,
        "non-Account/unverified credentials never reach fine authorization or preference storage"
    );
    let flat = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/agent-directory")
                .extension(awaken_runtime_contract::runs::scope::WorkspaceScope(
                    "workspace".into(),
                ))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        flat.status(),
        StatusCode::OK,
        "the native composition's actual WorkspaceScope is trusted without guessing an installation or company"
    );
    let flat: Value =
        serde_json::from_slice(&to_bytes(flat.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(flat["data"].as_array().unwrap().len(), 2);
    let missing_context = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/agent-directory")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_context.status(), StatusCode::FORBIDDEN);
    let unconfigured = routes::router(
        AgentGovernanceStores::new(configs.clone(), None),
        Some(native.iam.clone()),
    );
    let missing = call(
        &unconfigured,
        token,
        "workspace",
        "GET",
        "/v1/agent-directory",
        Value::Null,
    )
    .await;
    assert_eq!(missing.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(missing.1["error"], "agent-governance-not-configured");
    for path in [
        "/v1/agent-directory?limit=0",
        "/v1/agent-directory?limit=101",
        "/v1/agent-directory?after=not-a-valid-cursor",
        "/v1/agent-directory?owner=person-b",
    ] {
        assert_eq!(
            call(&app, token, "workspace", "GET", path, Value::Null)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(call(&app,token,"workspace","PUT","/v1/me/agent-selections/a-visible",json!({"request_key":"never-write","expected_generation":0,"selected":true,"owner":"person-b"})).await.0,StatusCode::BAD_REQUEST);
    let owner = AgentSelectionOwner::new(ISSUER.into(), "person-a".into()).unwrap();
    assert!(
        configs
            .personal_agent_selection(&ScopeId::from("workspace"), &owner, "a-visible")
            .await
            .unwrap()
            .is_none()
    );
    let first = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/agent-directory?limit=1",
        Value::Null,
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["data"].as_array().unwrap().len(), 1);
    assert_eq!(first.1["data"][0]["name"], "Published a-visible");
    assert_eq!(first.1["data"][0]["version"], 1);
    assert_eq!(first.1["data"][0]["selection"]["state"], "not-selected");
    let text = first.1.to_string();
    for hidden in [
        "b-hidden",
        "d-hidden",
        "Secret draft",
        "private definition",
        "instructions",
        "system",
        "tools",
        "model",
        "total",
    ] {
        assert!(!text.contains(hidden), "unexpected {hidden}: {text}");
    }
    let cursor = first.1["next_page"].as_str().unwrap();
    let second = call(
        &app,
        token,
        "workspace",
        "GET",
        &format!("/v1/agent-directory?limit=1&after={cursor}"),
        Value::Null,
    )
    .await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["data"][0]["id"], "c-visible");
    assert!(second.1["next_page"].is_null());
    configs
        .put_config_scoped(
            &ScopeId::from("workspace"),
            &config("b-hidden", "Changed hidden draft"),
        )
        .await
        .unwrap();
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/agent-directory?limit=1",
            Value::Null
        )
        .await
        .1,
        first.1,
        "hidden mutations do not alter healthy page/cursor/termination"
    );
    let intent = json!({"request_key":"original","expected_generation":0,"selected":true});
    let selected = call(
        &app,
        token,
        "workspace",
        "PUT",
        "/v1/me/agent-selections/a-visible",
        intent.clone(),
    )
    .await;
    assert_eq!(selected.0, StatusCode::OK, "{}", selected.1);
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "PUT",
            "/v1/me/agent-selections/a-visible",
            intent.clone()
        )
        .await
        .1,
        selected.1
    );
    let b = call(
        &app,
        &native.tokens[1],
        "workspace",
        "GET",
        "/v1/agent-directory",
        Value::Null,
    )
    .await;
    assert_eq!(
        b.1["data"][0]["selection"]["state"], "not-selected",
        "same manifest does not expose another Account preference"
    );
    revoke(
        &native.store,
        &grant_id("person-a", "a-visible", "agent.use"),
    );
    let hidden = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/agent-directory?selected=true",
        Value::Null,
    )
    .await;
    assert_eq!(hidden.0, StatusCode::OK);
    assert!(hidden.1["data"].as_array().unwrap().is_empty());
    let recovered = call(
        &app,
        token,
        "workspace",
        "GET",
        "/v1/me/agent-selections/a-visible/outcomes/original",
        Value::Null,
    )
    .await;
    assert_eq!(recovered.0, StatusCode::OK);
    assert_eq!(recovered.1, selected.1);
    assert!(!recovered.1.to_string().contains("Published"));
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "PUT",
            "/v1/me/agent-selections/a-visible",
            intent.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let removed = call(
        &app,
        token,
        "workspace",
        "PUT",
        "/v1/me/agent-selections/a-visible",
        json!({"request_key":"remove","expected_generation":1,"selected":false}),
    )
    .await;
    assert_eq!(removed.0, StatusCode::OK, "{}", removed.1);
    grant(&native.store, "person-a", "a-visible", "agent.use");
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "PUT",
            "/v1/me/agent-selections/a-visible",
            intent
        )
        .await
        .1,
        selected.1,
        "old select replay returns receipt without restoring preference"
    );
    let owner = AgentSelectionOwner::new(ISSUER.into(), "person-a".into()).unwrap();
    assert!(
        !configs
            .personal_agent_selection(&ScopeId::from("workspace"), &owner, "a-visible")
            .await
            .unwrap()
            .unwrap()
            .selected
    );
    revoke(
        &native.store,
        &grant_id("person-a", "a-visible", "agent.selection.read"),
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
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/agent-directory?selected=true",
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
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
    assert_eq!(market.1["data"][0]["selection"]["state"], "unknown");
    grant(
        &native.store,
        "person-a",
        "a-visible",
        "agent.selection.read",
    );
    for mode in [1, 2, 3, 4] {
        let strict_before = native.faults.strict_calls.load(Ordering::SeqCst);
        let observation_before = native.faults.observation_calls.load(Ordering::SeqCst);
        native.faults.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            call(
                &app,
                token,
                "workspace",
                "GET",
                "/v1/agent-directory",
                Value::Null
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE,
            "mode {mode}"
        );
        if mode == 1 {
            assert_eq!(
                native.faults.strict_calls.load(Ordering::SeqCst) - strict_before,
                1
            );
        }
        if mode == 2 {
            assert_eq!(
                native.faults.observation_calls.load(Ordering::SeqCst) - observation_before,
                1,
                "a failed current fence is not retried"
            );
        }
        native.faults.mode.store(0, Ordering::SeqCst);
        assert_eq!(
            call(
                &app,
                token,
                "workspace",
                "GET",
                "/v1/agent-directory",
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let changed = native.store.clone();
    *native.faults.mutate.lock().unwrap() = Some(Box::new(move || {
        grant(&changed, "person-b", "b-hidden", "agent.selection.read");
    }));
    native.faults.mode.store(5, Ordering::SeqCst);
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/agent-directory",
            Value::Null
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE,
        "whole-page policy epoch drift cannot mix older Allow"
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
            "/v1/agent-directory",
            Value::Null
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE,
        "whole-page Config epoch drift discards the already captured manifests"
    );
    native.faults.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        call(
            &app,
            token,
            "workspace",
            "GET",
            "/v1/agent-directory",
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            token,
            "foreign",
            "GET",
            "/v1/me/agent-selections/a-visible/outcomes/original",
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    committed_unknown(&native, &configs).await;
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
            "/v1/agent-directory",
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED,
        "same issued token observes live native Account withdrawal"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_native_directory_and_owner_selection_current_authority_matrix() {
    let native = native(sqlite_in_memory_store("agent_governance").unwrap()).await;
    exercise(
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
async fn postgres_native_directory_and_owner_selection_current_authority_matrix() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let pool = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
    let config = Arc::new(
        awaken_config_store::PostgresConfigStore::with_pool(pool.clone())
            .await
            .unwrap(),
    );
    let prefix = "iam";
    let iam_store = tokio::task::spawn_blocking(move || {
        awaken_iam_server::postgres_migrated_store(&url, prefix).unwrap()
    })
    .await
    .unwrap();
    exercise(native(iam_store).await, config).await;
    pool.close().await;
}

#[path = "definition_http_tests.rs"]
mod definition_http_tests;

#[path = "selection_list_tests.rs"]
mod selection_list_tests;
