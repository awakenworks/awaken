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

use std::path::PathBuf;
use std::sync::Arc;

use awaken_authz_enforce::{EnforceEngine, RequestTenancy, TokenSpec, guard};
use awaken_config_resolver::{InMemoryProjectStore, Project, ProjectId, ProjectStore};
use awaken_protocol_managed::{
    InMemorySessionRepository, ManagedSessionRepository, ManagedState, ProjectScope,
    router as managed_router,
};
use awaken_protocol_transport::ProtocolRuntime;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_host::{
    ManagedHost, ProtocolHost, SharedHost, SqliteManagedSessionRepository, durable_ops_router,
    files_router, memory_stores_router, skills_router,
};
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

    // `SharedHost::new` itself goes durable off `AWAKEN_STORAGE_DIR` (SQLite commit
    // store + memory blob store). Match the Managed session repo to the same root
    // so a created session survives a restart; in-memory when no dir is set.
    let host = Arc::new(SharedHost::new(model, "awaken"));
    let session_repo: Arc<dyn ManagedSessionRepository> = match storage_dir() {
        Some(dir) => Arc::new(
            SqliteManagedSessionRepository::open(&dir.join("sessions.db").to_string_lossy())
                .expect("open sessions.db under AWAKEN_STORAGE_DIR"),
        ),
        None => Arc::new(InMemorySessionRepository::default()),
    };
    let managed_state =
        Arc::new(ManagedState::new(ManagedHost::new(host.clone())).with_session_repo(session_repo));

    // The full open protocol surface (Managed + AI SDK + AG-UI + A2A + the file /
    // memory / skill / durable-ops resource planes), built twice: the bare axis and
    // the copy the project ingress forwards to. Each is wrapped with the guard, so
    // every protocol on both addressing axes is enforced. The project ingress stamps
    // RequestTenancy before forwarding, so the inner guard authorizes at the
    // project's scope.
    let bare = session_surface(&managed_state, &host)
        .layer(axum::middleware::from_fn_with_state(engine.clone(), guard));
    let inner = session_surface(&managed_state, &host)
        .layer(axum::middleware::from_fn_with_state(engine, guard));
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

/// The full open protocol surface over one host: Managed sessions + the AI SDK,
/// AG-UI and A2A adapters (all over a neutral `ProtocolHost` bound to the same
/// host) + the file / memory-store / skill / durable-ops resource planes. Built
/// on demand so the bare and project axes each get their own guarded copy.
fn session_surface(managed_state: &Arc<ManagedState>, host: &Arc<SharedHost>) -> Router {
    let port: Arc<dyn ProtocolRuntime> = Arc::new(ProtocolHost::new(host.clone()));
    managed_router(managed_state.clone())
        .merge(awaken_protocol_ai_sdk::router(port.clone()))
        .merge(awaken_protocol_ag_ui::router(port.clone()))
        .merge(awaken_protocol_a2a::router(port))
        .merge(files_router(host.clone()))
        .merge(memory_stores_router(host.clone()))
        .merge(skills_router(host.clone()))
        .merge(durable_ops_router(host.clone()))
}

/// The durable storage root, when configured. `SharedHost::new` reads the same
/// variable for its own commit/memory durability, so a set dir makes the whole
/// standalone survive a restart.
fn storage_dir() -> Option<PathBuf> {
    std::env::var("AWAKEN_STORAGE_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
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

/// Boot the zero-config single-machine server over the built-in [`HelloModel`].
/// A real deployment calls [`build`] with `awaken_provider_genai::GenAiExecutor`
/// (pointed at a provider or the awaken-cloud gateway) instead.
#[must_use]
pub fn boot() -> Standalone {
    build(Arc::new(HelloModel))
}

/// Boot, bind `addr`, and serve until `shutdown` resolves. `addr` may use port
/// `0` for an ephemeral port; the bound address + the seeded keys are handed to
/// `on_bound` once (the binary passes [`print_banner`]). Extracted from `main` so
/// the bind/serve path is exercised by a test rather than only in production.
pub async fn run(
    addr: &str,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    on_bound: impl FnOnce(&Standalone, std::net::SocketAddr),
) -> std::io::Result<()> {
    let standalone = boot();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    on_bound(&standalone, local);
    axum::serve(listener, standalone.router)
        .with_graceful_shutdown(shutdown)
        .await
}

/// The default `on_bound` for [`run`]: print the [`banner`] to stderr.
pub fn print_banner(standalone: &Standalone, addr: std::net::SocketAddr) {
    eprintln!("{}", banner(standalone, &addr.to_string()));
}

/// The one-time startup banner: the seeded keys (the single-machine operator
/// hand-off) and the addressing, printed once at boot.
#[must_use]
pub fn banner(standalone: &Standalone, addr: &str) -> String {
    format!(
        "awaken-standalone: single-machine open runtime\n  \
         admin key: {}\n  api key:   {}\n  \
         project:   /projects/{PROJECT_ID}/v1/sessions (also bare /v1/sessions)\n  \
         listening on http://{addr}\n  \
         rotate the printed keys before exposing this beyond localhost.",
        standalone.admin_token, standalone.api_token
    )
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
