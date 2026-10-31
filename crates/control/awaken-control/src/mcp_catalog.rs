// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: Static owner: the config-only MCP catalog, preferences, inspection and exact binding
// @formal reason: preview. Dynamic path: every query composes already-authored configuration into the
// @formal reason: binding a Session would receive, without executing a Session, storing credential
// @formal reason: material, or constructing an MCP client. Evidence:
// @formal reason: control.agent_profiles.immutable_profile plus this module's catalog and preview tests;
// @formal reason: this is a product-requirement classification, not a direct proof claim.
//! Config-only MCP catalog, preferences, inspection and exact binding preview.
//! No Session execution, credential storage or MCP client is implemented here.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_agent_config::mcp_config::*;
use awaken_protocol_managed::VaultState;
use awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding;
use awaken_tenancy::WorkspaceScope;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
pub struct McpCatalogService {
    store: Arc<dyn McpConfigurationRepository>,
    vault: Arc<VaultState>,
}
mod contract;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use contract::*;

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn error(value: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":{"type":"invalid_request_error","message":value.to_string()}})),
    )
}
fn domain(error_value: McpConfigError) -> (StatusCode, Json<Value>) {
    let status = match &error_value {
        McpConfigError::Conflict => StatusCode::CONFLICT,
        McpConfigError::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({"error":{"type":"invalid_request_error","message":error_value.to_string()}})),
    )
}
fn missing() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error":{"type":"not_found_error","message":"MCP server not found"}})),
    )
}
impl McpCatalogService {
    pub fn new(store: Arc<dyn McpConfigurationRepository>, vault: Arc<VaultState>) -> Self {
        Self { store, vault }
    }
    pub(crate) fn supervise(&self, lifecycle: &awaken_service_lifecycle::ServiceLifecycle) {
        self.supervise_with_interval(lifecycle, std::time::Duration::from_secs(300));
    }

    fn supervise_with_interval(
        &self,
        lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
        refresh_interval: std::time::Duration,
    ) {
        let service = self.clone();
        lifecycle.spawn("control-mcp-catalog", move |cancel| async move {
            let mut interval = tokio::time::interval(refresh_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! { ()=cancel.cancelled()=>break, _=interval.tick()=>{} }
                let scopes=match service.store.mcp_configuration_workspaces().await {
                    Ok(scopes)=>scopes, Err(error)=>{tracing::warn!(%error,"MCP catalog scope lookup failed");continue;}
                };
                for scope in scopes {
                    if !service.load(&scope).await.is_ok_and(|c|c.catalog_url.is_some()) {continue;}
                    let result=tokio::select! { ()=cancel.cancelled()=>return Ok(()), result=service.sync(&scope)=>result };
                    if result.is_err(){tracing::warn!(workspace=%scope,"MCP catalog refresh failed; retaining last accepted snapshot");}
                }
            }
            Ok(())
        });
    }

    async fn load(&self, workspace: &str) -> Result<McpConfiguration, McpConfigError> {
        self.store
            .get_mcp_configuration(workspace)
            .await?
            .map_or_else(
                || {
                    let catalog: McpCatalogSnapshot =
                        serde_json::from_str(include_str!("mcp_catalog/builtin.json"))
                            .map_err(|e| McpConfigError::Storage(e.to_string()))?;
                    catalog.validate()?;
                    Ok(McpConfiguration::new(workspace.into(), catalog))
                },
                Ok,
            )
    }
    async fn save(
        &self,
        expected: u64,
        mut next: McpConfiguration,
    ) -> Result<McpConfiguration, McpConfigError> {
        let current = self.load(&next.workspace_id).await?;
        if current.revision != expected {
            return Err(McpConfigError::Conflict);
        }
        if current == next {
            return Ok(current);
        }
        next.advance(expected)?;
        self.store.put_mcp_configuration(expected, &next).await?;
        Ok(next)
    }
    pub(crate) async fn sync(&self, workspace: &str) -> Result<McpConfiguration, McpConfigError> {
        use awaken_outbound_http::{ExternalHttpClient, JsonRequest};
        let mut config = self.load(workspace).await?;
        let url = config.catalog_url.as_ref().ok_or_else(|| {
            McpConfigError::Invalid("configure a catalog source URL first".into())
        })?;
        let client = ExternalHttpClient::for_target(url, std::time::Duration::from_secs(20))
            .map_err(|e| McpConfigError::Storage(e.to_string()))?;
        let response = client
            .send(&JsonRequest::get(url))
            .await
            .map_err(|e| McpConfigError::Storage(e.to_string()))?;
        if !response.is_success() || response.body().len() > 1024 * 1024 {
            return Err(McpConfigError::Invalid(
                "catalog download failed or exceeds 1 MiB".into(),
            ));
        }
        let snapshot: McpCatalogSnapshot = serde_json::from_str(response.body())
            .map_err(|e| McpConfigError::Invalid(e.to_string()))?;
        if !config.apply_catalog(snapshot)? {
            return Ok(config);
        }
        self.save(config.revision, config).await
    }
}
fn view(config: &McpConfiguration) -> Result<Value, McpConfigError> {
    let mut servers = Vec::new();
    for (origin, items) in [
        ("catalog", config.catalog.servers.iter().collect::<Vec<_>>()),
        ("local", config.local.values().collect::<Vec<_>>()),
    ] {
        for server in items {
            let key = format!("{origin}:{}", server.id);
            let default = config.defaults.get(&key);
            servers.push(McpServerView {
                key,
                origin: origin.into(),
                definition: server.clone(),
                target_fingerprint: server.target_fingerprint()?,
                default_credential: default.cloned(),
                default_matches_target: default.is_none_or(|d| {
                    server.target_fingerprint().ok().as_ref() == Some(&d.target_fingerprint)
                }),
            });
        }
    }
    serde_json::to_value(McpConfigurationView {
        revision: config.revision,
        servers,
        catalog: McpCatalogView {
            source_url: config.catalog_url.clone(),
            active_source_url: config.catalog_source.clone(),
            revision: config.catalog.revision,
            digest: config.catalog.digest()?,
        },
    })
    .map_err(|e| McpConfigError::Storage(e.to_string()))
}

async fn list(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
) -> ApiResult {
    Ok(Json(
        view(&s.load(&ws.0).await.map_err(domain)?).map_err(domain)?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "McpPutServer"))]
struct PutServer {
    expected_revision: u64,
    definition: McpServerDefinition,
}
async fn save_server(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Json(request): Json<PutServer>,
) -> ApiResult {
    let mut config = s.load(&ws.0).await.map_err(domain)?;
    config.put_local(request.definition).map_err(domain)?;
    Ok(Json(
        view(
            &s.save(request.expected_revision, config)
                .await
                .map_err(domain)?,
        )
        .map_err(domain)?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "McpRevision"))]
struct Revision {
    expected_revision: u64,
}
async fn remove_server(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Path(key): Path<String>,
    Json(request): Json<Revision>,
) -> ApiResult {
    let id = key
        .strip_prefix("local:")
        .ok_or_else(|| error("cloud catalog entries are read-only"))?;
    let mut config = s.load(&ws.0).await.map_err(domain)?;
    if config.server(&key).is_none() {
        return Err(missing());
    }
    config.remove_local(id);
    Ok(Json(
        view(
            &s.save(request.expected_revision, config)
                .await
                .map_err(domain)?,
        )
        .map_err(domain)?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
struct TargetQuery {
    url: String,
}
async fn credential_inventory(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
) -> ApiResult {
    Ok(Json(
        json!({"data":s.vault.mcp_credential_inventory(&ws.0).await.map_err(error)?}),
    ))
}
async fn credentials_for_target(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Query(request): Query<TargetQuery>,
) -> ApiResult {
    Ok(Json(
        json!({"data":s.vault.mcp_credential_choices(&ws.0,&request.url).await.map_err(error)?}),
    ))
}
async fn choices(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Path(key): Path<String>,
) -> ApiResult {
    let config = s.load(&ws.0).await.map_err(domain)?;
    let server = config.server(&key).ok_or_else(missing)?;
    let choices = match &server.transport {
        AgentMcpTransportBinding::Http(t) => s
            .vault
            .mcp_credential_choices(&ws.0, &t.url)
            .await
            .map_err(error)?,
        AgentMcpTransportBinding::SandboxStdio(_) => Vec::new(),
    };
    Ok(Json(json!({"data":choices})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "McpSelection"))]
struct Selection {
    expected_revision: u64,
    credential_id: Option<String>,
}
/// The preview pins the user's exact account in an isolated debug Agent using
/// its existing private CredentialRef field. A Session cannot substitute a
/// different Vault match after the operator made this choice.
async fn prepare(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Path(key): Path<String>,
    Json(request): Json<Selection>,
) -> ApiResult {
    let config = s.load(&ws.0).await.map_err(domain)?;
    if config.revision != request.expected_revision {
        return Err(domain(McpConfigError::Conflict));
    }
    let server = config.server(&key).ok_or_else(missing)?;
    let choice = if let Some(id) = &request.credential_id {
        let AgentMcpTransportBinding::Http(t) = &server.transport else {
            return Err(error(
                "Bearer/OAuth credentials cannot be attached to Sandbox stdio",
            ));
        };
        Some(
            s.vault
                .mcp_credential_choices(&ws.0, &t.url)
                .await
                .map_err(error)?
                .into_iter()
                .find(|c| &c.id == id)
                .ok_or_else(|| error("credential is unavailable or targets another endpoint"))?,
        )
    } else {
        None
    };
    let binding = awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
        name: server.id.clone(),
        transport: server.transport.clone(),
        prompts_as_skills: server.prompts_as_skills,
        credential: choice.as_ref().map(|c| c.source.clone()),
    };
    Ok(Json(
        serde_json::to_value(McpPreparedBinding {
            binding,
            credential: choice,
            revision: config.revision,
        })
        .map_err(error)?,
    ))
}
async fn default_credential(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Path(key): Path<String>,
    Json(request): Json<Selection>,
) -> ApiResult {
    let mut config = s.load(&ws.0).await.map_err(domain)?;
    if request.credential_id.is_none() {
        config.defaults.remove(&key);
        return Ok(Json(
            view(
                &s.save(request.expected_revision, config)
                    .await
                    .map_err(domain)?,
            )
            .map_err(domain)?,
        ));
    }
    let server = config.server(&key).ok_or_else(missing)?;
    if let Some(id) = request.credential_id {
        let AgentMcpTransportBinding::Http(t) = &server.transport else {
            return Err(error("Sandbox stdio cannot use an MCP HTTP credential"));
        };
        if !s
            .vault
            .mcp_credential_choices(&ws.0, &t.url)
            .await
            .map_err(error)?
            .iter()
            .any(|c| c.id == id)
        {
            return Err(error(
                "credential is unavailable or targets another endpoint",
            ));
        }
        let target_fingerprint = server.target_fingerprint().map_err(domain)?;
        config.defaults.insert(
            key,
            McpCredentialPreference {
                vault_credential_id: id,
                target_fingerprint,
            },
        );
    } else {
        config.defaults.remove(&key);
    }
    Ok(Json(
        view(
            &s.save(request.expected_revision, config)
                .await
                .map_err(domain)?,
        )
        .map_err(domain)?,
    ))
}
async fn inspect(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Path(key): Path<String>,
    Json(request): Json<Selection>,
) -> ApiResult {
    let config = s.load(&ws.0).await.map_err(domain)?;
    if config.revision != request.expected_revision {
        return Err(domain(McpConfigError::Conflict));
    }
    let server = config.server(&key).ok_or_else(missing)?;
    let AgentMcpTransportBinding::Http(t) = &server.transport else {
        return Err(error(
            "Use a real Session to test Sandbox stdio in its execution environment",
        ));
    };
    Ok(Json(
        serde_json::to_value(
            s.vault
                .inspect_mcp(&ws.0, &t.url, request.credential_id.as_deref())
                .await
                .map_err(error)?,
        )
        .map_err(error)?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "McpSource"))]
struct Source {
    expected_revision: u64,
    url: Option<String>,
}
async fn source(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
    Json(request): Json<Source>,
) -> ApiResult {
    if let Some(url) = &request.url {
        if url.len() > 2048 {
            return Err(error("catalog source URL exceeds 2048 bytes"));
        }
        let parsed = url::Url::parse(url).map_err(error)?;
        if parsed.scheme() != "https"
            && !(parsed.scheme() == "http" && awaken_outbound_http::target_is_loopback(url))
        {
            return Err(error(
                "catalog source requires HTTPS (or a loopback test source)",
            ));
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(error(
                "catalog URL cannot contain credentials or a fragment",
            ));
        }
    }
    let mut config = s.load(&ws.0).await.map_err(domain)?;
    config.catalog_url = request.url;
    Ok(Json(
        view(
            &s.save(request.expected_revision, config)
                .await
                .map_err(domain)?,
        )
        .map_err(domain)?,
    ))
}
async fn sync(
    State(s): State<McpCatalogService>,
    Extension(ws): Extension<WorkspaceScope>,
) -> ApiResult {
    Ok(Json(
        view(&s.sync(&ws.0).await.map_err(domain)?).map_err(domain)?,
    ))
}
pub(crate) fn router(service: McpCatalogService) -> Router {
    Router::new()
        .route("/v1/config/mcp-servers", get(list).put(save_server))
        .route(
            "/v1/config/mcp-servers/credentials",
            get(credentials_for_target),
        )
        .route(
            "/v1/config/mcp-servers/credential-inventory",
            get(credential_inventory),
        )
        .route("/v1/config/mcp-servers/{key}/remove", post(remove_server))
        .route("/v1/config/mcp-servers/{key}/credentials", get(choices))
        .route(
            "/v1/config/mcp-servers/{key}/default-credential",
            put(default_credential),
        )
        .route("/v1/config/mcp-servers/{key}/prepare", post(prepare))
        .route("/v1/config/mcp-servers/{key}/inspect", post(inspect))
        .route("/v1/config/mcp-catalog/source", put(source))
        .route("/v1/config/mcp-catalog/sync", post(sync))
        .with_state(service)
}

#[cfg(test)]
mod tests;

#[cfg(feature = "schema")]
pub fn contract_schemas() -> serde_json::Map<String, Value> {
    let mut result = serde_json::Map::new();
    macro_rules! add {
        ($name:literal,$ty:ty) => {
            result.insert(
                $name.into(),
                serde_json::to_value(schemars::schema_for!($ty)).expect("schema serializes"),
            );
        };
    }
    add!("McpServerDefinition", McpServerDefinition);
    add!("McpConfigurationView", McpConfigurationView);
    add!("McpServerView", McpServerView);
    add!("McpPreparedBinding", McpPreparedBinding);
    add!("AgentMcpTransportBinding", AgentMcpTransportBinding);
    add!(
        "McpCredentialChoice",
        awaken_protocol_managed::McpCredentialChoice
    );
    add!(
        "McpInspectionView",
        awaken_protocol_managed::McpInspectionView
    );
    add!("McpPutServer", PutServer);
    add!("McpRevision", Revision);
    add!("McpSelection", Selection);
    add!("McpSource", Source);
    result
}

#[cfg(feature = "schema")]
pub fn contract_paths() -> serde_json::Map<String, Value> {
    fn reference(name: &str) -> Value {
        json!({"$ref":format!("#/components/schemas/{name}")})
    }
    let mut paths = serde_json::Map::<String, Value>::new();
    for (path, method, input, output) in [
        (
            "/v1/config/mcp-servers",
            "get",
            None,
            Some("McpConfigurationView"),
        ),
        (
            "/v1/config/mcp-servers",
            "put",
            Some("McpPutServer"),
            Some("McpConfigurationView"),
        ),
        ("/v1/config/mcp-servers/credentials", "get", None, None),
        (
            "/v1/config/mcp-servers/credential-inventory",
            "get",
            None,
            None,
        ),
        (
            "/v1/config/mcp-servers/{key}/credentials",
            "get",
            None,
            None,
        ),
        (
            "/v1/config/mcp-servers/{key}/prepare",
            "post",
            Some("McpSelection"),
            Some("McpPreparedBinding"),
        ),
        (
            "/v1/config/mcp-servers/{key}/inspect",
            "post",
            Some("McpSelection"),
            Some("McpInspectionView"),
        ),
        (
            "/v1/config/mcp-servers/{key}/default-credential",
            "put",
            Some("McpSelection"),
            Some("McpConfigurationView"),
        ),
        (
            "/v1/config/mcp-servers/{key}/remove",
            "post",
            Some("McpRevision"),
            Some("McpConfigurationView"),
        ),
        (
            "/v1/config/mcp-catalog/source",
            "put",
            Some("McpSource"),
            Some("McpConfigurationView"),
        ),
        (
            "/v1/config/mcp-catalog/sync",
            "post",
            None,
            Some("McpConfigurationView"),
        ),
    ] {
        let response=output.map(reference).unwrap_or_else(||json!({"type":"object","required":["data"],"properties":{"data":{"type":"array","items":reference("McpCredentialChoice")}}}));
        let mut operation = json!({"responses":{"200":{"description":"Success","content":{"application/json":{"schema":response}}}},"tags":["MCP configuration"]});
        let mut parameters = Vec::new();
        if path.contains("{key}") {
            parameters
                .push(json!({"name":"key","in":"path","required":true,"schema":{"type":"string"}}));
        }
        if path.ends_with("/mcp-servers/credentials") {
            parameters.push(
                json!({"name":"url","in":"query","required":true,"schema":{"type":"string"}}),
            );
        }
        if !parameters.is_empty() {
            operation["parameters"] = json!(parameters);
        }
        if let Some(input) = input {
            operation["requestBody"] =
                json!({"required":true,"content":{"application/json":{"schema":reference(input)}}});
        }
        paths.entry(path).or_insert_with(|| json!({}))[method] = operation;
    }
    paths
}
