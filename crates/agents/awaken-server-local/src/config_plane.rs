//! The config data plane (slice A): author, publish, and install agent configs.
//!
//! `ConfigService` is the config domain's authoring authority — it validates and
//! stores declarative [`AgentConfig`]s in a [`ConfigRegistry`], and on publish
//! compiles one into a content-addressed [`StoredPublication`] and hot-swaps it
//! into the installed catalog. The host then resolves a session's agent to its
//! installed runnable config, so a published agent runs with its own instructions,
//! tools, and plugins (ADR-0031; the config/runtime seam is the compiled snapshot).
//!
//! The runtime never edits config records; it consumes only the compiled config.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_config_store::{
    AgentConfig, ConfigRegistry, RunnableConfig, StoredPublication, compile,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Json, Router};
use serde_json::{Value, json};

/// The config domain service: validate, store, publish, and expose the installed
/// (published) runnable config per agent.
pub struct ConfigService {
    registry: Arc<dyn ConfigRegistry>,
    /// The tool descriptors an agent config may name; `compile` binds `tool_ids`
    /// to these. The host's advertised hand tools.
    tools: Vec<ToolDescriptor>,
    /// The installed catalog: agent id → compiled runnable config, hot-swapped on
    /// publish. A run resolves its agent here (awaken-next `set_registry_snapshot`).
    installed: Mutex<HashMap<String, RunnableConfig>>,
}

impl ConfigService {
    pub fn new(registry: Arc<dyn ConfigRegistry>, tools: Vec<ToolDescriptor>) -> Self {
        Self {
            registry,
            tools,
            installed: Mutex::new(HashMap::new()),
        }
    }

    /// Validate a config by compiling it (a dry run of publish); no store write.
    pub fn validate(&self, config: &AgentConfig) -> Result<(), String> {
        compile(config, &self.tools)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Store a config draft (upsert by id).
    pub async fn put(&self, config: &AgentConfig) -> Result<(), String> {
        self.registry
            .put_config(config)
            .await
            .map_err(|e| e.to_string())
    }

    /// Publish: compile the stored config, persist the publication (idempotent by
    /// fingerprint), and install it into the live catalog so new runs use it.
    pub async fn publish(&self, id: &str) -> Result<StoredPublication, String> {
        let config = self
            .registry
            .get_config(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no config stored for agent `{id}`"))?;
        let runnable = compile(&config, &self.tools).map_err(|e| e.to_string())?;
        let publication = StoredPublication::published(runnable.clone(), id);
        self.registry
            .put_publication(&publication)
            .await
            .map_err(|e| e.to_string())?;
        self.installed
            .lock()
            .unwrap()
            .insert(id.to_string(), runnable);
        Ok(publication)
    }

    /// The installed (published) runnable config for `agent`, if any.
    pub fn installed(&self, agent: &str) -> Option<RunnableConfig> {
        self.installed.lock().unwrap().get(agent).cloned()
    }
}

/// The config data-plane router: `/v1/config/agents/:id` (author) plus
/// `/validate` and `/publish` (lifecycle).
pub fn config_router(service: Arc<ConfigService>) -> Router {
    Router::new()
        .route("/v1/config/agents/:id/validate", post(validate))
        .route("/v1/config/agents/:id/publish", post(publish))
        .route("/v1/config/agents/:id", put(put_config))
        .with_state(service)
}

async fn validate(
    State(svc): State<Arc<ConfigService>>,
    Path(id): Path<String>,
    Json(mut config): Json<AgentConfig>,
) -> (StatusCode, Json<Value>) {
    config.id = id;
    match svc.validate(&config) {
        Ok(()) => (StatusCode::OK, Json(json!({ "valid": true }))),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "valid": false, "error": error })),
        ),
    }
}

async fn put_config(
    State(svc): State<Arc<ConfigService>>,
    Path(id): Path<String>,
    Json(mut config): Json<AgentConfig>,
) -> (StatusCode, Json<Value>) {
    config.id = id.clone();
    match svc.put(&config).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "id": id }))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    }
}

async fn publish(
    State(svc): State<Arc<ConfigService>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match svc.publish(&id).await {
        Ok(publication) => (
            StatusCode::OK,
            Json(json!({
                "publication_id": publication.publication_id,
                "fingerprint": publication.fingerprint,
                "agent_id": publication.agent_id,
                "installed": true,
            })),
        ),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    }
}
