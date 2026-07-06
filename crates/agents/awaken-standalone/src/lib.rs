//! The open single-machine assembly (`awaken-standalone`).
//!
//! It seeds one tenant (a singleton workspace + project), mints an admin key and
//! an api key into an in-memory [`EnforceEngine`], mounts the Managed session
//! surface (bare `/v1/…` and project-prefixed `/projects/{id}/v1/…`), and wraps
//! both with the session-axis [`guard`]. Everything it composes is open — no
//! admin authoring plane, no durable IAM store, no distributed backend — so the
//! same runtime that a private/cloud deployment scales out runs here on one
//! machine with two keys and a config-free boot.
//!
//! The `TenancySeeder` here is the single-machine substitute for the multi-tenant
//! HTTP authoring plane: configuration is injected at boot, not CRUD'd over the
//! wire.

use std::sync::Arc;

use awaken_authz_enforce::{EnforceEngine, RequestTenancy, TokenSpec, guard};
use awaken_config_resolver::{InMemoryProjectStore, Project, ProjectId, ProjectStore};
use awaken_protocol_managed::{ManagedState, ProjectScope, router as managed_router};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_host::{ManagedHost, SharedHost};
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;

/// The seeded singleton workspace id.
pub const WORKSPACE_ID: &str = "wrkspc_local";
/// The seeded singleton project id (DNS-safe slug; also the URL segment).
pub const PROJECT_ID: &str = "local";

/// A fully-assembled standalone: the router plus the two seeded credentials the
/// operator uses (the admin key provisions, the api key runs agents).
pub struct Standalone {
    pub router: Router,
    pub admin_token: String,
    pub api_token: String,
}

/// Build the standalone over `model`. Seeds the singleton tenant + two keys, then
/// mounts and guards the session surface.
pub fn build(model: Arc<dyn LlmExecutor>) -> Standalone {
    let engine = Arc::new(EnforceEngine::seeded());
    let admin_token = engine
        .mint(TokenSpec {
            token_id: "tok_admin".into(),
            service_id: "operator".into(),
            workspace_id: WORKSPACE_ID.into(),
            role: "admin".into(),
            expires_at: None,
        })
        .expect("mint the admin key");
    let api_token = engine
        .mint(TokenSpec {
            token_id: "tok_api".into(),
            service_id: "app".into(),
            workspace_id: WORKSPACE_ID.into(),
            role: "admin".into(),
            expires_at: None,
        })
        .expect("mint the api key");

    let projects: Arc<dyn ProjectStore> = Arc::new(InMemoryProjectStore::new());
    projects.put_project(Project {
        id: ProjectId(PROJECT_ID.to_string()),
        workspace_id: WORKSPACE_ID.to_string(),
        display_name: "Local".to_string(),
        version: 1,
    });

    let host = Arc::new(SharedHost::new(model, "awaken"));
    let managed_state = Arc::new(ManagedState::new(ManagedHost::new(host)));

    // The bare surface and the inner surface the project ingress forwards to are
    // the SAME router, each wrapped with the guard so both axes are enforced. The
    // project ingress stamps RequestTenancy before forwarding, so the guard on
    // the inner router authorizes at the project's scope.
    let bare = managed_router(managed_state.clone())
        .layer(axum::middleware::from_fn_with_state(engine.clone(), guard));
    let inner =
        managed_router(managed_state).layer(axum::middleware::from_fn_with_state(engine, guard));
    let project_sessions = Router::new().route(
        "/projects/:project_id/*rest",
        any(project_ingress).with_state((projects, inner)),
    );

    Standalone {
        router: Router::new().merge(bare).merge(project_sessions),
        admin_token,
        api_token,
    }
}

/// Resolve the `/projects/{id}` segment: 404 an unauthored project, else strip
/// the prefix, stamp [`ProjectScope`] + [`RequestTenancy`] (the project's OWN
/// workspace, so the guard's fence is correct), and forward to the guarded inner
/// session router.
async fn project_ingress(
    State((projects, sessions)): State<(Arc<dyn ProjectStore>, Router)>,
    Path((project_id, rest)): Path<(String, String)>,
    request: Request,
) -> Response {
    use tower::ServiceExt;
    let Some(project) = projects.get_project(&project_id) else {
        return (
            StatusCode::NOT_FOUND,
            format!("project `{project_id}` not found"),
        )
            .into_response();
    };
    let stripped = match request.uri().query() {
        Some(query) => format!("/{rest}?{query}"),
        None => format!("/{rest}"),
    };
    let (parts, body) = request.into_parts();
    let mut forwarded = Request::builder()
        .method(parts.method)
        .uri(stripped)
        .body(body)
        .expect("a stripped project path re-parses as a URI");
    *forwarded.headers_mut() = parts.headers;
    forwarded
        .extensions_mut()
        .insert(ProjectScope(project_id.clone()));
    forwarded.extensions_mut().insert(RequestTenancy {
        workspace_id: project.workspace_id.clone(),
        project_id: Some(project_id),
    });
    match sessions.oneshot(forwarded).await {
        Ok(response) => response,
        Err(err) => match err {},
    }
}

/// The default single-machine model: a deterministic greeter that ends the turn
/// with one line of text — no external provider, so a standalone boots and runs
/// with zero configuration. A real deployment injects
/// `awaken_provider_genai::GenAiExecutor` (pointed at a provider or the
/// awaken-cloud gateway) via [`build`] instead.
pub struct HelloModel;

#[async_trait::async_trait]
impl LlmExecutor for HelloModel {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("Hello from awaken-standalone.".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}
