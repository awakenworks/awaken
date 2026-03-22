//! MCP tool registry manager: server lifecycle, tool discovery, periodic refresh.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};
use mcp::transport::{McpTransportError, ServerCapabilities, TransportTypeId};
use mcp::{CallToolResult, McpToolDefinition};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::config::McpServerConnectionConfig;
use crate::error::McpError;
use crate::id_mapping::to_tool_id;
use crate::progress::{
    McpProgressUpdate, ProgressEmitGate, normalize_progress, should_emit_progress,
};
use crate::sampling::SamplingHandler;
use crate::transport::{
    McpPromptDefinition, McpPromptResult, McpResourceDefinition, McpToolTransport,
    call_result_to_tool_data, connect_transport,
};

// ── Metadata constants ──

const MCP_META_SERVER: &str = "mcp.server";
const MCP_META_TOOL: &str = "mcp.tool";
const MCP_META_UI_RESOURCE_URI: &str = "mcp.ui.resourceUri";
const MCP_META_UI_CONTENT: &str = "mcp.ui.content";
const MCP_META_UI_MIME_TYPE: &str = "mcp.ui.mimeType";
const MCP_META_RESULT_CONTENT: &str = "mcp.result.content";
const MCP_META_RESULT_STRUCTURED_CONTENT: &str = "mcp.result.structuredContent";

// ── Helper types ──

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpRefreshHealth {
    pub last_attempt_at: Option<SystemTime>,
    pub last_success_at: Option<SystemTime>,
    pub last_error: Option<String>,
    pub consecutive_failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptEntry {
    pub server_name: String,
    pub transport_type: TransportTypeId,
    pub prompt: McpPromptDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceEntry {
    pub server_name: String,
    pub transport_type: TransportTypeId,
    pub resource: McpResourceDefinition,
}

// ── McpTool: wraps an MCP tool as an awaken Tool ──

/// Metadata embedded in the `data` field of a ToolResult as a `_mcp` sub-object.
fn build_mcp_metadata(server_name: &str, tool_name: &str, call_result: &CallToolResult) -> Value {
    let mut meta = serde_json::Map::new();
    meta.insert(
        MCP_META_SERVER.to_string(),
        Value::String(server_name.to_string()),
    );
    meta.insert(
        MCP_META_TOOL.to_string(),
        Value::String(tool_name.to_string()),
    );

    if !call_result.content.is_empty()
        && let Ok(content) = serde_json::to_value(&call_result.content)
    {
        meta.insert(MCP_META_RESULT_CONTENT.to_string(), content);
    }

    if let Some(structured) = call_result.structured_content.clone() {
        meta.insert(MCP_META_RESULT_STRUCTURED_CONTENT.to_string(), structured);
    }

    Value::Object(meta)
}

struct McpTool {
    descriptor: ToolDescriptor,
    server_name: String,
    tool_name: String,
    transport: Arc<dyn McpToolTransport>,
    ui_resource_uri: Option<String>,
}

impl McpTool {
    fn new(
        tool_id: String,
        server_name: String,
        def: McpToolDefinition,
        transport: Arc<dyn McpToolTransport>,
        transport_type: TransportTypeId,
    ) -> Self {
        let name = def.title.clone().unwrap_or_else(|| def.name.clone());
        let desc_text = def
            .description
            .clone()
            .unwrap_or_else(|| format!("MCP tool {}", def.name));

        // Encode MCP-specific metadata in a JSON description annotation since
        // awaken's ToolDescriptor does not have a metadata field.
        let description = format!(
            "{} [mcp.server={}, mcp.tool={}, mcp.transport={}]",
            desc_text, server_name, def.name, transport_type
        );

        let mut d = ToolDescriptor::new(tool_id, name, description)
            .with_parameters(def.input_schema.clone());

        if let Some(group) = def.group.clone() {
            d = d.with_category(group);
        }

        let ui_resource_uri = def
            .meta
            .as_ref()
            .and_then(|m| m.get("ui"))
            .and_then(|ui| ui.get("resourceUri"))
            .and_then(|v| v.as_str())
            .map(String::from);

        Self {
            descriptor: d,
            server_name,
            tool_name: def.name,
            transport,
            ui_resource_uri,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let mut call = Box::pin(
            self.transport
                .call_tool(&self.tool_name, args, Some(progress_tx)),
        );
        let mut gate = ProgressEmitGate::default();

        let res = loop {
            tokio::select! {
                result = &mut call => break result,
                maybe_update = progress_rx.recv() => {
                    let Some(update) = maybe_update else {
                        continue;
                    };
                    emit_mcp_progress(ctx, &mut gate, update).await;
                }
            }
        }
        .map_err(map_mcp_error)?;

        while let Ok(update) = progress_rx.try_recv() {
            emit_mcp_progress(ctx, &mut gate, update).await;
        }

        let data = call_result_to_tool_data(&res);
        let mcp_meta = build_mcp_metadata(&self.server_name, &self.tool_name, &res);

        // Wrap data with _mcp metadata in result
        let enriched_data = match data {
            Value::Object(mut map) => {
                map.insert("_mcp".to_string(), mcp_meta);
                Value::Object(map)
            }
            other => {
                serde_json::json!({
                    "value": other,
                    "_mcp": mcp_meta,
                })
            }
        };

        let mut result = ToolResult::success(self.descriptor.id.clone(), enriched_data);

        if let Some(ref uri) = self.ui_resource_uri
            && let Some(content) = fetch_ui_resource(&self.transport, uri).await
            && let Value::Object(ref mut map) = result.data
            && let Some(Value::Object(mcp)) = map.get_mut("_mcp")
        {
            mcp.insert(
                MCP_META_UI_RESOURCE_URI.to_string(),
                Value::String(uri.clone()),
            );
            mcp.insert(MCP_META_UI_CONTENT.to_string(), Value::String(content.text));
            mcp.insert(
                MCP_META_UI_MIME_TYPE.to_string(),
                Value::String(content.mime_type),
            );
        }

        Ok(result)
    }
}

struct UiResourceContent {
    text: String,
    mime_type: String,
}

async fn fetch_ui_resource(
    transport: &Arc<dyn McpToolTransport>,
    uri: &str,
) -> Option<UiResourceContent> {
    let value = transport.read_resource(uri).await.ok()?;
    let contents = value.get("contents")?.as_array()?;
    let first = contents.first()?;
    let text = first.get("text")?.as_str()?.to_string();
    let mime_type = first
        .get("mimeType")
        .and_then(|v| v.as_str())
        .unwrap_or("text/html")
        .to_string();
    Some(UiResourceContent { text, mime_type })
}

async fn emit_mcp_progress(
    ctx: &ToolCallContext,
    gate: &mut ProgressEmitGate,
    update: McpProgressUpdate,
) {
    let Some(normalized_progress) = normalize_progress(&update) else {
        return;
    };
    if !should_emit_progress(gate, normalized_progress, update.message.as_deref()) {
        return;
    }
    let content = serde_json::json!({
        "progress": normalized_progress,
        "loaded": update.progress,
        "total": update.total,
        "message": update.message,
    });
    ctx.report_activity("mcp.progress", &content.to_string())
        .await;
}

fn map_mcp_error(e: McpTransportError) -> ToolError {
    match e {
        McpTransportError::UnknownTool(name) => ToolError::NotFound(name),
        McpTransportError::Timeout(msg) => ToolError::ExecutionFailed(format!("timeout: {}", msg)),
        other => ToolError::ExecutionFailed(other.to_string()),
    }
}

// ── Server runtime ──

#[derive(Clone)]
struct McpServerRuntime {
    name: String,
    transport_type: TransportTypeId,
    transport: Arc<dyn McpToolTransport>,
    capabilities: Option<ServerCapabilities>,
}

// ── Registry snapshot ──

#[derive(Clone, Default)]
struct McpRegistrySnapshot {
    version: u64,
    tools: HashMap<String, Arc<dyn Tool>>,
}

struct PeriodicRefreshRuntime {
    stop_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

struct McpRegistryState {
    servers: Vec<McpServerRuntime>,
    snapshot: RwLock<McpRegistrySnapshot>,
    refresh_health: RwLock<McpRefreshHealth>,
    periodic_refresh: Mutex<Option<PeriodicRefreshRuntime>>,
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn validate_server_name(name: &str) -> Result<(), McpError> {
    if name.trim().is_empty() {
        return Err(McpError::EmptyServerName);
    }
    Ok(())
}

fn is_unsupported_transport_message(message: &str, operation: &str) -> bool {
    message.contains(operation) && message.contains("not supported")
}

fn server_supports_prompts(capabilities: Option<&ServerCapabilities>) -> bool {
    capabilities.is_none_or(|capabilities| capabilities.prompts.is_some())
}

fn server_supports_resources(capabilities: Option<&ServerCapabilities>) -> bool {
    capabilities.is_none_or(|capabilities| capabilities.resources.is_some())
}

fn is_periodic_refresh_running(state: &McpRegistryState) -> bool {
    let mut runtime = mutex_lock(&state.periodic_refresh);
    if runtime
        .as_ref()
        .is_some_and(|running| running.join.is_finished())
    {
        *runtime = None;
        return false;
    }
    runtime.is_some()
}

async fn discover_tools(
    servers: &[McpServerRuntime],
) -> Result<HashMap<String, Arc<dyn Tool>>, McpError> {
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    for server in servers {
        let mut defs = server.transport.list_tools().await?;
        defs.sort_by(|a, b| a.name.cmp(&b.name));

        for def in defs {
            let tool_id = to_tool_id(&server.name, &def.name)?;
            if tools.contains_key(&tool_id) {
                return Err(McpError::ToolIdConflict(tool_id));
            }
            tools.insert(
                tool_id.clone(),
                Arc::new(McpTool::new(
                    tool_id,
                    server.name.clone(),
                    def,
                    server.transport.clone(),
                    server.transport_type,
                )) as Arc<dyn Tool>,
            );
        }
    }

    Ok(tools)
}

async fn refresh_state(state: &McpRegistryState) -> Result<u64, McpError> {
    let attempted_at = SystemTime::now();
    match discover_tools(&state.servers).await {
        Ok(tools) => {
            let mut snapshot = write_lock(&state.snapshot);
            let version = snapshot.version.saturating_add(1);
            *snapshot = McpRegistrySnapshot { version, tools };

            let mut health = write_lock(&state.refresh_health);
            health.last_attempt_at = Some(attempted_at);
            health.last_success_at = Some(attempted_at);
            health.last_error = None;
            health.consecutive_failures = 0;

            Ok(version)
        }
        Err(err) => {
            let mut health = write_lock(&state.refresh_health);
            health.last_attempt_at = Some(attempted_at);
            health.last_error = Some(err.to_string());
            health.consecutive_failures = health.consecutive_failures.saturating_add(1);
            Err(err)
        }
    }
}

async fn periodic_refresh_loop(
    state: Weak<McpRegistryState>,
    interval: Duration,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = ticker.tick() => {
                let Some(state) = state.upgrade() else {
                    break;
                };
                if let Err(err) = refresh_state(state.as_ref()).await {
                    tracing::warn!(error = %err, "MCP periodic refresh failed");
                }
            }
        }
    }
}

// ── McpToolRegistryManager ──

/// Dynamic MCP registry manager.
///
/// Keeps server transports alive and refreshes discovered tool definitions
/// into a shared snapshot consumed by [`McpToolRegistry`].
#[derive(Clone)]
pub struct McpToolRegistryManager {
    state: Arc<McpRegistryState>,
}

impl std::fmt::Debug for McpToolRegistryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read_lock(&self.state.snapshot);
        let periodic_running = is_periodic_refresh_running(self.state.as_ref());
        f.debug_struct("McpToolRegistryManager")
            .field("servers", &self.state.servers.len())
            .field("tools", &snapshot.tools.len())
            .field("version", &snapshot.version)
            .field("periodic_refresh_running", &periodic_running)
            .finish()
    }
}

impl McpToolRegistryManager {
    pub async fn connect(
        configs: impl IntoIterator<Item = McpServerConnectionConfig>,
    ) -> Result<Self, McpError> {
        Self::connect_with_sampling(configs, None).await
    }

    pub async fn connect_with_sampling(
        configs: impl IntoIterator<Item = McpServerConnectionConfig>,
        sampling_handler: Option<Arc<dyn SamplingHandler>>,
    ) -> Result<Self, McpError> {
        let mut entries: Vec<(McpServerConnectionConfig, Arc<dyn McpToolTransport>)> = Vec::new();
        for cfg in configs {
            validate_server_name(&cfg.name)?;
            let transport = connect_transport(&cfg, sampling_handler.clone()).await?;
            entries.push((cfg, transport));
        }
        Self::from_tool_transports(entries).await
    }

    pub async fn from_transports(
        entries: impl IntoIterator<Item = (McpServerConnectionConfig, Arc<dyn McpToolTransport>)>,
    ) -> Result<Self, McpError> {
        Self::from_tool_transports(entries).await
    }

    async fn from_tool_transports(
        entries: impl IntoIterator<Item = (McpServerConnectionConfig, Arc<dyn McpToolTransport>)>,
    ) -> Result<Self, McpError> {
        let servers = Self::build_servers(entries).await?;
        let tools = discover_tools(&servers).await?;

        let snapshot = McpRegistrySnapshot { version: 1, tools };
        Ok(Self {
            state: Arc::new(McpRegistryState {
                servers,
                snapshot: RwLock::new(snapshot),
                refresh_health: RwLock::new(McpRefreshHealth {
                    last_attempt_at: Some(SystemTime::now()),
                    last_success_at: Some(SystemTime::now()),
                    last_error: None,
                    consecutive_failures: 0,
                }),
                periodic_refresh: Mutex::new(None),
            }),
        })
    }

    async fn build_servers(
        entries: impl IntoIterator<Item = (McpServerConnectionConfig, Arc<dyn McpToolTransport>)>,
    ) -> Result<Vec<McpServerRuntime>, McpError> {
        let mut servers: Vec<McpServerRuntime> = Vec::new();
        let mut names: HashSet<String> = HashSet::new();

        for (cfg, transport) in entries {
            validate_server_name(&cfg.name)?;
            if !names.insert(cfg.name.clone()) {
                return Err(McpError::DuplicateServerName(cfg.name));
            }
            let capabilities = transport.server_capabilities().await?;

            servers.push(McpServerRuntime {
                name: cfg.name,
                transport_type: transport.transport_type(),
                transport,
                capabilities,
            });
        }

        servers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(servers)
    }

    pub async fn refresh(&self) -> Result<u64, McpError> {
        refresh_state(self.state.as_ref()).await
    }

    pub fn start_periodic_refresh(&self, interval: Duration) -> Result<(), McpError> {
        if interval.is_zero() {
            return Err(McpError::InvalidRefreshInterval);
        }

        let handle = Handle::try_current().map_err(|_| McpError::RuntimeUnavailable)?;
        let mut runtime = mutex_lock(&self.state.periodic_refresh);
        if runtime
            .as_ref()
            .is_some_and(|running| !running.join.is_finished())
        {
            return Err(McpError::PeriodicRefreshAlreadyRunning);
        }

        let (stop_tx, stop_rx) = oneshot::channel();
        let weak_state = Arc::downgrade(&self.state);
        let join = handle.spawn(periodic_refresh_loop(weak_state, interval, stop_rx));

        *runtime = Some(PeriodicRefreshRuntime {
            stop_tx: Some(stop_tx),
            join,
        });
        Ok(())
    }

    pub async fn stop_periodic_refresh(&self) -> bool {
        let runtime = {
            let mut guard = mutex_lock(&self.state.periodic_refresh);
            guard.take()
        };

        let Some(mut runtime) = runtime else {
            return false;
        };

        if let Some(stop_tx) = runtime.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        let _ = runtime.join.await;
        true
    }

    pub fn periodic_refresh_running(&self) -> bool {
        is_periodic_refresh_running(self.state.as_ref())
    }

    pub fn registry(&self) -> McpToolRegistry {
        McpToolRegistry {
            state: self.state.clone(),
        }
    }

    pub fn version(&self) -> u64 {
        read_lock(&self.state.snapshot).version
    }

    pub fn servers(&self) -> Vec<(String, TransportTypeId)> {
        self.state
            .servers
            .iter()
            .map(|server| (server.name.clone(), server.transport_type))
            .collect()
    }

    pub fn refresh_health(&self) -> McpRefreshHealth {
        read_lock(&self.state.refresh_health).clone()
    }

    pub async fn list_prompts(&self) -> Result<Vec<McpPromptEntry>, McpError> {
        let mut prompts = Vec::new();

        for server in &self.state.servers {
            if !server_supports_prompts(server.capabilities.as_ref()) {
                continue;
            }
            let mut defs = match server.transport.list_prompts().await {
                Ok(defs) => defs,
                Err(McpTransportError::TransportError(message))
                    if is_unsupported_transport_message(&message, "list_prompts") =>
                {
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            defs.sort_by(|a, b| a.name.cmp(&b.name));
            prompts.extend(defs.into_iter().map(|prompt| McpPromptEntry {
                server_name: server.name.clone(),
                transport_type: server.transport_type,
                prompt,
            }));
        }

        prompts.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then_with(|| a.prompt.name.cmp(&b.prompt.name))
        });
        Ok(prompts)
    }

    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpError> {
        let server = self
            .state
            .servers
            .iter()
            .find(|server| server.name == server_name)
            .ok_or_else(|| McpError::UnknownServer(server_name.to_string()))?;
        if !server_supports_prompts(server.capabilities.as_ref()) {
            return Err(McpError::UnsupportedCapability {
                server_name: server.name.clone(),
                capability: "prompts",
            });
        }

        server
            .transport
            .get_prompt(prompt_name, arguments)
            .await
            .map_err(Into::into)
    }

    pub async fn list_resources(&self) -> Result<Vec<McpResourceEntry>, McpError> {
        let mut resources = Vec::new();

        for server in &self.state.servers {
            if !server_supports_resources(server.capabilities.as_ref()) {
                continue;
            }
            let mut defs = match server.transport.list_resources().await {
                Ok(defs) => defs,
                Err(McpTransportError::TransportError(message))
                    if is_unsupported_transport_message(&message, "list_resources") =>
                {
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            defs.sort_by(|a, b| a.uri.cmp(&b.uri));
            resources.extend(defs.into_iter().map(|resource| McpResourceEntry {
                server_name: server.name.clone(),
                transport_type: server.transport_type,
                resource,
            }));
        }

        resources.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then_with(|| a.resource.uri.cmp(&b.resource.uri))
        });
        Ok(resources)
    }

    pub async fn read_resource(&self, server_name: &str, uri: &str) -> Result<Value, McpError> {
        let server = self
            .state
            .servers
            .iter()
            .find(|server| server.name == server_name)
            .ok_or_else(|| McpError::UnknownServer(server_name.to_string()))?;
        if !server_supports_resources(server.capabilities.as_ref()) {
            return Err(McpError::UnsupportedCapability {
                server_name: server.name.clone(),
                capability: "resources",
            });
        }

        server
            .transport
            .read_resource(uri)
            .await
            .map_err(Into::into)
    }
}

// ── McpToolRegistry ──

/// Dynamic tool registry view backed by [`McpToolRegistryManager`].
#[derive(Clone)]
pub struct McpToolRegistry {
    state: Arc<McpRegistryState>,
}

impl std::fmt::Debug for McpToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read_lock(&self.state.snapshot);
        let periodic_running = is_periodic_refresh_running(self.state.as_ref());
        f.debug_struct("McpToolRegistry")
            .field("servers", &self.state.servers.len())
            .field("tools", &snapshot.tools.len())
            .field("version", &snapshot.version)
            .field("periodic_refresh_running", &periodic_running)
            .finish()
    }
}

impl McpToolRegistry {
    pub fn version(&self) -> u64 {
        read_lock(&self.state.snapshot).version
    }

    pub fn servers(&self) -> Vec<(String, TransportTypeId)> {
        self.state
            .servers
            .iter()
            .map(|server| (server.name.clone(), server.transport_type))
            .collect()
    }

    pub fn len(&self) -> usize {
        read_lock(&self.state.snapshot).tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        read_lock(&self.state.snapshot).tools.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        let snapshot = read_lock(&self.state.snapshot);
        let mut ids: Vec<String> = snapshot.tools.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        read_lock(&self.state.snapshot).tools.clone()
    }

    pub fn refresh_health(&self) -> McpRefreshHealth {
        read_lock(&self.state.refresh_health).clone()
    }
}
