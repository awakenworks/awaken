// @formal evidence management_http.no_business_write_without_durable_audit
// @formal evidence management_http.successful_write_marks_audit_committed
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_config::ModelSelection;
use awaken_agent_contract::ScopeId;
use awaken_config_service::{
    ConfigService, ManagementAuditRepository, ModelPublicationResolver, PublicationResolutionError,
    ResolvedPublicationModels, StaticToolCatalog,
};
use awaken_config_store::SqliteConfigStore;
use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_tenancy::WorkspaceScope;
use axum::middleware::Next;
use axum::routing::post;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use super::*;

struct TestModelResolver;

#[async_trait::async_trait]
impl ModelPublicationResolver for TestModelResolver {
    async fn resolve_models(
        &self,
        _workspace: &ScopeId,
        selection: &ModelSelection,
        candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        let primary = selection
            .resolved()
            .cloned()
            .ok_or_else(|| "test requires a pinned model".to_string())?;
        Ok(ResolvedPublicationModels::host(
            primary,
            candidates.to_vec(),
            None,
            None,
        ))
    }
}

async fn audit_plane() -> ManagementAuditPlane {
    ConfigPlane::new(
        Arc::new(ConfigService::new(
            Arc::new(TestModelResolver),
            Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
                ExecutableAgentCatalog::new(),
            ))),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
            |_| true,
        )),
        Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
        Arc::new(StaticToolCatalog(vec![])),
    )
    .management_audit_plane()
}

/// A management mutation as it reaches the audit layer in a real deployment:
/// the trusted edge (IAM, workspace path, or the composition-root stamp) has
/// already resolved a Workspace, because the audit record is filed against one
/// and the layer refuses an unstamped request rather than inventing a tenant.
fn mutation(call_id: &str, body: impl Into<Body>) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/mutate")
        .header("x-request-id", call_id)
        .body(body.into())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("ws_edge".into()));
    request
}

async fn stamp_workspace(mut request: Request<Body>, next: Next) -> Response {
    request
        .extensions_mut()
        .insert(WorkspaceScope("ws_authenticated".into()));
    next.run(request).await
}

struct FaultInjectingAuditRepository {
    inner: ManagementAuditPlane,
    fail_record: bool,
    fail_mark: bool,
    record_calls: AtomicUsize,
    mark_calls: AtomicUsize,
}

impl FaultInjectingAuditRepository {
    fn new(inner: ManagementAuditPlane, fail_record: bool, fail_mark: bool) -> Self {
        Self {
            inner,
            fail_record,
            fail_mark,
            record_calls: AtomicUsize::new(0),
            mark_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ManagementAuditRepository for FaultInjectingAuditRepository {
    async fn record(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        self.record_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_record {
            Err("injected durable audit failure".into())
        } else {
            self.inner.record(scope, audit).await
        }
    }

    async fn get(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<awaken_agent_config::ManagementAuditEntry>, String> {
        self.inner.get(scope, tool, call_id).await
    }

    async fn mark_committed(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), String> {
        self.mark_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_mark {
            Err("injected durable audit completion failure".into())
        } else {
            self.inner.mark_committed(scope, tool, call_id).await
        }
    }
}

#[tokio::test]
async fn durable_audit_failure_blocks_business_through_canonical_management_router() {
    // Causes: C1 a body-bounded non-read request reaches the canonical protected
    // management router; C2 the durable audit repository rejects its intent;
    // C3 the business handler has an observable write counter. Effects: E1 the
    // edge returns 500 with the durable failure; E2 record is attempted exactly
    // once; E3 the handler and audit completion are never entered. Decision rule:
    // C1+C2+C3 => E1+E2+E3. Applied/replay/conflict/body-limit and IAM rules stay
    // owned by the adjacent tests instead of creating another audit workflow.
    let repository = Arc::new(FaultInjectingAuditRepository::new(
        audit_plane().await,
        true,
        false,
    ));
    let plane = ManagementAuditPlane::from_repository(repository.clone());
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = protect_management_router(
        Router::new().route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        ),
        plane,
        None,
        None,
        None,
    );

    let response = app.oneshot(mutation("audit-fails-1", "{}")).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "E1: {body:?}");
    assert!(
        String::from_utf8_lossy(&body).contains("injected durable audit failure"),
        "E1: {body:?}"
    );
    assert_eq!(repository.record_calls.load(Ordering::SeqCst), 1, "E2");
    assert_eq!(business_calls.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(repository.mark_calls.load(Ordering::SeqCst), 0, "E3");
}

#[tokio::test]
async fn management_audit_completion_marks_successful_write_in_authenticated_partition() {
    // Causes: C1 an authenticated non-read request is admitted as Applied by
    // the real SQLite audit repository; C2 the business handler returns 204;
    // C3 the handler has an observable write counter. Effects: E1 business is
    // entered exactly once and audit record/completion are each attempted once;
    // E2 the exact audit is marked committed in the authenticated partition;
    // E3 no default-partition record is created.
    // Decision rule: C1+C2+C3 => E1+E2+E3 through protect_management_router.
    let durable = audit_plane().await;
    let repository = Arc::new(FaultInjectingAuditRepository::new(
        durable.clone(),
        false,
        false,
    ));
    let plane = ManagementAuditPlane::from_repository(repository.clone());
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = protect_management_router(
        Router::new().route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        ),
        plane.clone(),
        None,
        None,
        None,
    )
    .layer(axum::middleware::from_fn(stamp_workspace));

    let response = app.oneshot(mutation("scope-1", "{}")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(business_calls.load(Ordering::SeqCst), 1, "E1");
    assert_eq!(repository.record_calls.load(Ordering::SeqCst), 1, "E1");
    assert_eq!(repository.mark_calls.load(Ordering::SeqCst), 1, "E1");

    let entry = durable
        .get(
            &ScopeId::from("ws_authenticated"),
            "http:POST:/v1/mutate",
            "scope-1",
        )
        .await
        .unwrap()
        .expect("authenticated workspace audit");
    assert!(entry.business_committed, "E2");
    assert!(
        durable
            .get(
                &ScopeId::from(DEFAULT_SCOPE),
                "http:POST:/v1/mutate",
                "scope-1",
            )
            .await
            .unwrap()
            .is_none(),
        "E3"
    );
}

#[tokio::test]
async fn management_audit_completion_failure_keeps_successful_write_pending() {
    // Causes: C1 the real SQLite repository durably admits a new audit; C2 the
    // business handler returns 204 after one observable write; C3 the delegated
    // completion call fails. Effects: E1 the original 204 remains observable;
    // E2 record, business, and completion are each attempted once; E3 the exact
    // durable audit remains pending. Decision rule: C1+C2+C3 => E1+E2+E3. The
    // pending record is ambiguous and does not authorize blind business replay.
    let durable = audit_plane().await;
    let repository = Arc::new(FaultInjectingAuditRepository::new(
        durable.clone(),
        false,
        true,
    ));
    let plane = ManagementAuditPlane::from_repository(repository.clone());
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = protect_management_router(
        Router::new().route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        ),
        plane,
        None,
        None,
        None,
    );

    let response = app
        .oneshot(mutation("completion-fails-1", "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "E1");
    assert_eq!(repository.record_calls.load(Ordering::SeqCst), 1, "E2");
    assert_eq!(business_calls.load(Ordering::SeqCst), 1, "E2");
    assert_eq!(repository.mark_calls.load(Ordering::SeqCst), 1, "E2");

    let entry = durable
        .get(
            &ScopeId::from("ws_edge"),
            "http:POST:/v1/mutate",
            "completion-fails-1",
        )
        .await
        .unwrap()
        .expect("durable pending audit");
    assert!(!entry.business_committed, "E3");
}

#[tokio::test]
async fn rejected_outer_authentication_cannot_create_an_audit_intent() {
    async fn reject(_request: Request<Body>, _next: Next) -> Response {
        StatusCode::UNAUTHORIZED.into_response()
    }

    let plane = audit_plane().await;
    let app = Router::new()
        .route("/v1/mutate", post(|| async { StatusCode::NO_CONTENT }))
        .layer(axum::middleware::from_fn_with_state(
            plane.clone(),
            durable_management_audit,
        ))
        .layer(axum::middleware::from_fn(reject));

    let response = app.oneshot(mutation("rejected-1", "{}")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        plane
            .get(
                &ScopeId::from(DEFAULT_SCOPE),
                "http:POST:/v1/mutate",
                "rejected-1",
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn replayed_or_conflicting_call_id_fails_closed_without_repeating_business_work() {
    // Cause/effect decision table: C1 an explicit audit request id is new; C2 it
    // is replayed with the same method/path/body; C3 it is reused with another
    // body. Effects: A1 admit and commit business once; A2 reject the ambiguous
    // audit replay without repeating business; A3 fail closed on conflicting
    // audit identity. Domain Idempotency-Key ownership is covered separately.
    // Rules: C1->A1; C1+C2->A2; C1+C3->A3.
    let plane = audit_plane().await;
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            plane,
            durable_management_audit,
        ));

    let first = app
        .clone()
        .oneshot(mutation("stable-1", "one"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    let replay = app
        .clone()
        .oneshot(mutation("stable-1", "one"))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    assert_eq!(business_calls.load(Ordering::SeqCst), 1);

    let conflict = app.oneshot(mutation("stable-1", "two")).await.unwrap();
    assert_eq!(conflict.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(business_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn domain_idempotency_key_is_not_reinterpreted_as_an_audit_call_identity() {
    // Cause/effect decision table: C1 two HTTP attempts carry one domain
    // Idempotency-Key; C2 neither supplies an audit X-Request-ID. Effects: D1 the
    // audit edge records distinct generated attempt ids and admits both; D2 the
    // domain handler remains the sole owner of replay/response semantics. The
    // Managed MCP E2E supplies D2's real aggregate and proves one domain effect.
    // Rule D1: C1+C2 -> both attempts reach the handler, never an audit 409.
    let plane = audit_plane().await;
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            plane,
            durable_management_audit,
        ));
    let domain_attempt = || {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/v1/mutate")
            .header("idempotency-key", "domain-command-1")
            .body(Body::from("one"))
            .unwrap();
        // The edge resolves the Workspace; the audit layer never invents one.
        request
            .extensions_mut()
            .insert(WorkspaceScope("ws_edge".into()));
        request
    };

    let first = app.clone().oneshot(domain_attempt()).await.unwrap();
    let replay = app.oneshot(domain_attempt()).await.unwrap();
    assert_eq!(first.status(), StatusCode::NO_CONTENT, "D1");
    assert_eq!(replay.status(), StatusCode::NO_CONTENT, "D1");
    assert_eq!(business_calls.load(Ordering::SeqCst), 2, "D2");
}

#[tokio::test]
async fn oversized_audit_body_is_rejected_before_business_or_audit() {
    let plane = audit_plane().await;
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            plane.clone(),
            durable_management_audit,
        ));

    let response = app
        .oneshot(mutation("large-1", "x".repeat(MAX_AUDITED_BODY + 1)))
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body:?}");
    assert_eq!(business_calls.load(Ordering::SeqCst), 0);
    assert!(
        plane
            .get(
                &ScopeId::from(DEFAULT_SCOPE),
                "http:POST:/v1/mutate",
                "large-1",
            )
            .await
            .unwrap()
            .is_none()
    );
}
