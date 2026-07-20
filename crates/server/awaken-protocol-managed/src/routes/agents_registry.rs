//! The Managed **agent registry** (`/v1/agents`), the official `@anthropic-ai/sdk`
//! `beta.agents.*` client's surface: create / retrieve / update / list / archive
//! plus the version history (`/v1/agents/:id/versions`). An agent is a reusable,
//! versioned configuration (model + system + tools + mcp_servers + skills +
//! multiagent topology) a session instantiates by id.
//!
//! Distinct from the workspace **config plane** (`/v1/config/agents/*`), which is
//! the admin authoring surface; this is the public account-level registry the SDK
//! addresses. State is a neutral in-memory store (one process): a stable
//! `agent_…` id, monotonic `version` with optimistic-concurrency updates, and a
//! full snapshot appended to history on every mutation.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

use crate::routes::ManagedJson;
use crate::routes::WorkspaceScope;
use crate::state::DEFAULT_SCOPE;
use crate::types::agent::{Agent, AgentCreateParams, AgentUpdateParams};
use crate::types::{ErrorResponse, ModelConfig, Page, PageQuery, paginate};

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// A stored agent configuration and its version history.
#[derive(Clone)]
struct Record {
    name: String,
    description: Option<String>,
    model: ModelConfig,
    system: Option<String>,
    metadata: BTreeMap<String, String>,
    mcp_servers: Vec<Value>,
    skills: Vec<Value>,
    tools: Vec<Value>,
    multiagent: Option<Value>,
    version: u64,
    archived_at: Option<String>,
    /// The projected agent at each past version (index 0 == v1), for `versions.list`.
    history: Vec<Agent>,
}

impl Record {
    fn project(&self, id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            object_type: "agent",
            archived_at: self.archived_at.clone(),
            created_at: OBJECT_AT.to_string(),
            updated_at: OBJECT_AT.to_string(),
            name: self.name.clone(),
            description: self.description.clone(),
            model: self.model.clone(),
            system: self.system.clone(),
            metadata: self.metadata.clone(),
            mcp_servers: self.mcp_servers.clone(),
            skills: self.skills.clone(),
            tools: self.tools.clone(),
            multiagent: self.multiagent.clone(),
            version: self.version,
        }
    }
}

// The agent-config port + its neutral view now live in `awaken-session-contract`
// (a contract/ leaf), re-exported here so existing `awaken_protocol_managed::…` paths
// keep resolving until consumers flip to the contract directly.
pub use awaken_session_contract::{AgentConfigSource, AgentConfigView};

/// The agent-registry state.
#[derive(Default)]
pub struct AgentRegistryState {
    inner: Mutex<BTreeMap<String, Record>>,
    /// The aspect-layer agent→owner index (ADR-0051): the scope that created each
    /// registry agent, so the edge ownership guard fences a cross-tenant request
    /// and `list` shows only the caller's agents. Registry-created agents only;
    /// config-plane projections carry their own (scoped) config-store truth.
    owners: Mutex<HashMap<String, String>>,
    seq: AtomicU64,
    /// When wired, `/v1/agents` reads projections from the config plane: an agent
    /// published there is retrievable here even if it was never created via this
    /// registry, and the config is authoritative for model/system/tools.
    config_source: Option<Arc<dyn AgentConfigSource>>,
}

impl AgentRegistryState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the config-plane projection source (see [`AgentConfigSource`]).
    #[must_use]
    pub fn with_config_source(mut self, source: Arc<dyn AgentConfigSource>) -> Self {
        self.config_source = Some(source);
        self
    }

    /// The owner scope of registry agent `id`, if this registry created it — the
    /// aspect-layer lookup the edge ownership guard consults (ADR-0051).
    #[must_use]
    pub fn owner_scope(&self, id: &str) -> Option<String> {
        self.owners.lock().unwrap().get(id).cloned()
    }
}

/// The request's resolved scope from the edge-stamped [`WorkspaceScope`], or the
/// seeded default when the deployment resolved none (ADR-0051).
fn request_scope(scope: &Option<Extension<WorkspaceScope>>) -> String {
    scope
        .as_ref()
        .map_or_else(|| DEFAULT_SCOPE.to_string(), |w| w.0.0.clone())
}

/// The tenant ownership guard for `/v1/agents/{id}` (ADR-0051): a request whose
/// resolved scope does not own the addressed registry agent is answered 404 (never
/// 403 — no existence disclosure). The collection routes (`/v1/agents`) and ids
/// unknown to the registry (config-plane projections) pass through.
pub async fn agent_scope_guard(
    State(state): State<Arc<AgentRegistryState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(id) = agent_id_from_path(request.uri().path()) {
        let request_scope = request
            .extensions()
            .get::<WorkspaceScope>()
            .map(|w| w.0.clone())
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        if let Some(owner) = state.owner_scope(&id)
            && owner != request_scope
        {
            return not_found().into_response();
        }
    }
    next.run(request).await
}

/// The `{id}` from a `/v1/agents/{id}[/...]` path, or `None` for the collection
/// route and any non-agent path.
fn agent_id_from_path(path: &str) -> Option<String> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != "v1" || segments.next()? != "agents" {
        return None;
    }
    match segments.next() {
        Some(id) if !id.is_empty() => Some(id.to_string()),
        _ => None,
    }
}

/// Present tool ids in the managed wire's tool shape.
fn tools_wire(ids: &[String]) -> Vec<Value> {
    ids.iter()
        .map(|id| json!({ "type": "custom", "name": id }))
        .collect()
}

/// Project a config-plane view to the `BetaManagedAgent` wire shape. The agent has
/// no registry record, so presentation fields default (name = id, version = 1).
fn project_config_view(id: &str, view: &AgentConfigView) -> Agent {
    Agent {
        id: id.to_string(),
        object_type: "agent",
        archived_at: None,
        created_at: OBJECT_AT.to_string(),
        updated_at: OBJECT_AT.to_string(),
        name: id.to_string(),
        description: None,
        model: ModelConfig::new(view.model.clone().unwrap_or_default()),
        system: view.system.clone(),
        metadata: BTreeMap::new(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        tools: tools_wire(&view.tool_ids),
        multiagent: None,
        version: 1,
    }
}

/// Mount the agent-registry routes.
pub fn agents_router(state: Arc<AgentRegistryState>) -> Router {
    let guard_state = state.clone();
    Router::new()
        .route("/v1/agents", post(create_agent).get(list_agents))
        .route("/v1/agents/{id}", get(retrieve_agent).post(update_agent))
        .route("/v1/agents/{id}/archive", post(archive_agent))
        .route("/v1/agents/{id}/versions", get(list_versions))
        .with_state(state)
        // The tenant ownership guard (ADR-0051) fences `/v1/agents/{id}` by owner.
        .layer(axum::middleware::from_fn_with_state(
            guard_state,
            agent_scope_guard,
        ))
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn not_found() -> WireError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("not_found_error", "agent not found")),
    )
}

async fn create_agent(
    State(state): State<Arc<AgentRegistryState>>,
    scope: Option<Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<AgentCreateParams>,
) -> Result<Json<Agent>, WireError> {
    let owner = request_scope(&scope);
    let n = state.seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("agent_{n:016}");
    let mut record = Record {
        name: params.name,
        description: params.description,
        model: params.model.into_config(),
        system: params.system,
        metadata: params.metadata,
        mcp_servers: params.mcp_servers,
        skills: params.skills,
        tools: params.tools,
        multiagent: params.multiagent.filter(|v| !v.is_null()),
        version: 1,
        archived_at: None,
        history: Vec::new(),
    };
    let projected = record.project(&id);
    record.history.push(projected.clone());
    state.owners.lock().unwrap().insert(id.clone(), owner);
    state.inner.lock().unwrap().insert(id, record);
    Ok(Json(projected))
}

async fn retrieve_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, WireError> {
    // A registry record (an agent created via this API) wins.
    {
        let store = state.inner.lock().unwrap();
        if let Some(record) = store.get(&id) {
            return Ok(Json(record.project(&id)));
        }
    }
    // Otherwise an agent published on the config plane is retrievable here as a pure
    // projection of that single truth (never created via this registry).
    if let Some(view) = state.config_source.as_ref().and_then(|s| s.agent_view(&id)) {
        return Ok(Json(project_config_view(&id, &view)));
    }
    Err(not_found())
}

/// `GET /v1/agents` — one full page, ascending id order, restricted to the
/// caller's own agents (ADR-0051): a workspace never sees another's registry
/// agents in its listing.
async fn list_agents(
    State(state): State<Arc<AgentRegistryState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Query(page): Query<PageQuery>,
) -> Json<Page<Agent>> {
    let scope = request_scope(&scope);
    let owners = state.owners.lock().unwrap();
    let store = state.inner.lock().unwrap();
    let data: Vec<Agent> = store
        .iter()
        .filter(|(id, _)| owners.get(id.as_str()).map(String::as_str) == Some(scope.as_str()))
        .map(|(id, r)| r.project(id))
        .collect();
    Json(paginate(data, &page, |a| a.id.as_str()))
}

/// `POST /v1/agents/:id` — update with optimistic concurrency. The body's
/// `version` must match the agent's current version (else `409`); on success the
/// provided fields replace, `version` increments, and a snapshot is appended to
/// history.
async fn update_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<AgentUpdateParams>,
) -> Result<Json<Agent>, WireError> {
    let mut store = state.inner.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(not_found)?;
    if params.version != record.version {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                // The SDK has no `conflict_error`; a 409 carries `invalid_request_error`.
                "invalid_request_error",
                format!(
                    "version mismatch: expected {}, got {}",
                    record.version, params.version
                ),
            )),
        ));
    }
    // Each field replaces only when present in the body.
    if let Some(name) = params.name {
        record.name = name;
    }
    if let Some(model) = params.model {
        record.model = model.into_config();
    }
    if let Some(description) = params.description {
        record.description = Some(description);
    }
    if let Some(system) = params.system {
        record.system = Some(system);
    }
    if let Some(metadata) = params.metadata {
        record.metadata = metadata;
    }
    if let Some(mcp_servers) = params.mcp_servers {
        record.mcp_servers = mcp_servers;
    }
    if let Some(skills) = params.skills {
        record.skills = skills;
    }
    if let Some(tools) = params.tools {
        record.tools = tools;
    }
    if let Some(multiagent) = params.multiagent {
        record.multiagent = Some(multiagent).filter(|v| !v.is_null());
    }
    record.version += 1;
    let projected = record.project(&id);
    record.history.push(projected.clone());
    Ok(Json(projected))
}

/// `POST /v1/agents/:id/archive` — soft-delete (sets `archived_at`, bumps version).
async fn archive_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, WireError> {
    let mut store = state.inner.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(not_found)?;
    record.archived_at = Some(OBJECT_AT.to_string());
    record.version += 1;
    let projected = record.project(&id);
    record.history.push(projected.clone());
    Ok(Json(projected))
}

/// `GET /v1/agents/:id/versions` — the agent's version history as a cursor page
/// (newest last), each entry a full `BetaManagedAgentsAgent` snapshot.
async fn list_versions(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<Agent>>, WireError> {
    let store = state.inner.lock().unwrap();
    let record = store.get(&id).ok_or_else(not_found)?;
    Ok(Json(paginate(record.history.clone(), &page, |a| {
        a.id.as_str()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// A config plane that has published exactly one agent, `assistant`.
    struct OneAgent;
    impl AgentConfigSource for OneAgent {
        fn agent_view(&self, agent_id: &str) -> Option<AgentConfigView> {
            (agent_id == "assistant").then(|| AgentConfigView {
                model: Some("kimi-k2".to_string()),
                system: Some("be helpful".to_string()),
                tool_ids: vec!["fs_read".to_string()],
                resources: Vec::new(),
            })
        }
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn config_plane_agent_is_retrievable_as_a_projection() {
        let state = Arc::new(AgentRegistryState::new().with_config_source(Arc::new(OneAgent)));
        let app = agents_router(state);

        // `assistant` was never created via /v1/agents — it is projected from the
        // config plane (the "retreat to projection" direction).
        let (status, body) = get(&app, "/v1/agents/assistant").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "assistant");
        assert_eq!(body["model"]["id"], "kimi-k2");
        assert_eq!(body["system"], "be helpful");
        assert_eq!(body["tools"][0]["name"], "fs_read");

        // An id in neither the registry nor the config plane is still 404.
        let (status, _) = get(&app, "/v1/agents/ghost").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // --- Tenant isolation (ADR-0051) -----------------------------------------

    async fn create_owned(app: &Router, scope: &str) -> String {
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/agents")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "name": "a", "model": "kimi" })).unwrap(),
            ))
            .unwrap();
        req.extensions_mut()
            .insert(WorkspaceScope(scope.to_string()));
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    async fn call_scoped(app: &Router, method: &str, uri: &str, scope: Option<&str>) -> StatusCode {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        if let Some(scope) = scope {
            req.extensions_mut()
                .insert(WorkspaceScope(scope.to_string()));
        }
        app.clone().oneshot(req).await.unwrap().status()
    }

    async fn list_ids(app: &Router, scope: &str) -> Vec<String> {
        let mut req = Request::builder()
            .method("GET")
            .uri("/v1/agents")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(WorkspaceScope(scope.to_string()));
        let res = app.clone().oneshot(req).await.unwrap();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_registry_agent_is_fenced_to_its_owner() {
        let app = agents_router(Arc::new(AgentRegistryState::new()));
        let id = create_owned(&app, "ws_a").await;
        let path = format!("/v1/agents/{id}");
        assert_eq!(
            call_scoped(&app, "GET", &path, Some("ws_a")).await,
            StatusCode::OK
        );
        // Another workspace gets 404 (never 403 — no existence disclosure), for reads…
        assert_eq!(
            call_scoped(&app, "GET", &path, Some("ws_b")).await,
            StatusCode::NOT_FOUND
        );
        // …and writes (archive).
        assert_eq!(
            call_scoped(&app, "POST", &format!("{path}/archive"), Some("ws_b")).await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn list_shows_only_the_callers_agents() {
        let app = agents_router(Arc::new(AgentRegistryState::new()));
        let a = create_owned(&app, "ws_a").await;
        let b = create_owned(&app, "ws_b").await;
        assert_eq!(list_ids(&app, "ws_a").await, vec![a]);
        assert_eq!(list_ids(&app, "ws_b").await, vec![b]);
    }

    /// A cross-tenant `update` (POST `/v1/agents/{id}`) and `versions`
    /// (GET `/v1/agents/{id}/versions`) are fenced by the same guard: a foreign
    /// workspace is 404'd before the handler runs (never 403), while the owner's
    /// own update and version listing are admitted. The existing tests only cover
    /// GET/archive; these pin the remaining two `{id}` verbs.
    #[tokio::test]
    async fn cross_tenant_update_and_versions_are_fenced() {
        let app = agents_router(Arc::new(AgentRegistryState::new()));
        let id = create_owned(&app, "ws_a").await;

        // A foreign-tenant update is 404'd at the guard (body never reaches the
        // optimistic-concurrency check).
        assert_eq!(
            call_scoped(&app, "POST", &format!("/v1/agents/{id}"), Some("ws_b")).await,
            StatusCode::NOT_FOUND
        );
        // A foreign-tenant version listing is likewise fenced.
        assert_eq!(
            call_scoped(
                &app,
                "GET",
                &format!("/v1/agents/{id}/versions"),
                Some("ws_b")
            )
            .await,
            StatusCode::NOT_FOUND
        );

        // The owner updates with the correct version, then reads two snapshots back.
        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/v1/agents/{id}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "version": 1, "name": "renamed" })).unwrap(),
            ))
            .unwrap();
        req.extensions_mut().insert(WorkspaceScope("ws_a".into()));
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            call_scoped(
                &app,
                "GET",
                &format!("/v1/agents/{id}/versions"),
                Some("ws_a")
            )
            .await,
            StatusCode::OK
        );
    }

    /// An empty `WorkspaceScope("")` is its own tenant — not silently coerced to the
    /// seeded default. An agent it owns is readable by an equally-empty scope, but
    /// invisible to a named workspace and to a bare (default-resolved) request.
    #[tokio::test]
    async fn an_empty_scope_is_a_distinct_tenant() {
        let app = agents_router(Arc::new(AgentRegistryState::new()));
        let id = create_owned(&app, "").await;
        let path = format!("/v1/agents/{id}");
        assert_eq!(
            call_scoped(&app, "GET", &path, Some("")).await,
            StatusCode::OK
        );
        assert_eq!(
            call_scoped(&app, "GET", &path, Some("ws_a")).await,
            StatusCode::NOT_FOUND
        );
        // A bare request resolves to DEFAULT_SCOPE, which is not "".
        assert_eq!(
            call_scoped(&app, "GET", &path, None).await,
            StatusCode::NOT_FOUND
        );
    }

    /// The seeded default scope is a real tenant an explicit credential can name:
    /// a bare-created (default-owned) agent is reachable by a request that stamps
    /// the literal `DEFAULT_SCOPE`, and unreachable by any other workspace. This
    /// pins the default↔explicit collision so a future change to `request_scope`
    /// can't silently split or merge the two.
    #[tokio::test]
    async fn the_default_scope_is_addressable_as_a_tenant() {
        let app = agents_router(Arc::new(AgentRegistryState::new()));
        // Bare create → owner is DEFAULT_SCOPE.
        let id = create_owned(&app, DEFAULT_SCOPE).await;
        let path = format!("/v1/agents/{id}");
        // A bare request (resolves to DEFAULT_SCOPE) and an explicit DEFAULT_SCOPE
        // request both own it.
        assert_eq!(call_scoped(&app, "GET", &path, None).await, StatusCode::OK);
        assert_eq!(
            call_scoped(&app, "GET", &path, Some(DEFAULT_SCOPE)).await,
            StatusCode::OK
        );
        assert_eq!(
            call_scoped(&app, "GET", &path, Some("ws_a")).await,
            StatusCode::NOT_FOUND
        );
    }

    /// A config-plane projection (an agent published on the config plane, never
    /// created via this registry) has no registry owner, so the ownership guard
    /// passes it through: it is retrievable under ANY scope. Its tenant scoping
    /// lives in the config store, not this aspect layer — this characterizes the
    /// deliberate pass-through so a regression that either over- or under-fences it
    /// is caught.
    #[tokio::test]
    async fn a_config_plane_projection_is_not_fenced_by_this_guard() {
        let state = Arc::new(AgentRegistryState::new().with_config_source(Arc::new(OneAgent)));
        let app = agents_router(state);
        for scope in [Some("ws_a"), Some("ws_b"), None] {
            assert_eq!(
                call_scoped(&app, "GET", "/v1/agents/assistant", scope).await,
                StatusCode::OK,
                "config-plane projection passes the ownership guard for {scope:?}"
            );
        }
    }
}
