//! MCP tool registry manager: server lifecycle, tool discovery, periodic refresh.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use awaken_contract::PeriodicRefresher;
use awaken_contract::contract::progress::ProgressStatus;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use mcp::McpToolDefinition;
use mcp::transport::{McpTransportError, ServerCapabilities, TransportTypeId};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

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
const MCP_META_TRANSPORT: &str = "mcp.transport";
const MCP_META_UI_RESOURCE_URI: &str = "mcp.ui.resourceUri";
const MCP_META_UI_CONTENT: &str = "mcp.ui.content";
const MCP_META_UI_MIME_TYPE: &str = "mcp.ui.mimeType";
const MCP_META_RESULT_CONTENT: &str = "mcp.result.content";
const MCP_META_RESULT_STRUCTURED_CONTENT: &str = "mcp.result.structuredContent";
const FAILURE_THRESHOLD: u64 = 3;
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

// ── Helper types ──

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpRefreshHealth {
    pub last_attempt_at: Option<SystemTime>,
    pub last_success_at: Option<SystemTime>,
    pub last_error: Option<String>,
    pub consecutive_failures: u64,
    pub reconnecting: bool,
    pub permanently_failed: bool,
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

struct McpTool {
    descriptor: ToolDescriptor,
    state: Weak<McpRegistryState>,
    server_name: String,
    tool_name: String,
    ui_resource_uri: Option<String>,
}

impl McpTool {
    fn new(
        state: Weak<McpRegistryState>,
        tool_id: String,
        server_name: String,
        def: McpToolDefinition,
        transport_type: TransportTypeId,
    ) -> Self {
        let name = def.title.clone().unwrap_or_else(|| def.name.clone());
        let description = def
            .description
            .clone()
            .unwrap_or_else(|| format!("MCP tool {}", def.name));

        let mut d = ToolDescriptor::new(tool_id, name, description)
            .with_parameters(def.input_schema.clone())
            .with_metadata(MCP_META_SERVER, Value::String(server_name.to_string()))
            .with_metadata(MCP_META_TOOL, Value::String(def.name.clone()))
            .with_metadata(
                MCP_META_TRANSPORT,
                Value::String(transport_type.to_string()),
            );

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
            state,
            server_name,
            tool_name: def.name,
            ui_resource_uri,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let transport = resolve_live_transport(&self.state, &self.server_name)
            .map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let mut call = Box::pin(transport.call_tool(&self.tool_name, args, Some(progress_tx)));
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
        };

        let res = match res {
            Ok(result) => {
                record_tool_transport_success(&self.state, &self.server_name).await;
                result
            }
            Err(err) => {
                record_tool_transport_failure(&self.state, &self.server_name, &err).await;
                return Err(map_mcp_error(err));
            }
        };

        while let Ok(update) = progress_rx.try_recv() {
            emit_mcp_progress(ctx, &mut gate, update).await;
        }

        let data = call_result_to_tool_data(&res);
        let mut result = ToolResult::success(self.descriptor.id.clone(), data);

        result.metadata.insert(
            MCP_META_SERVER.to_string(),
            Value::String(self.server_name.clone()),
        );
        result.metadata.insert(
            MCP_META_TOOL.to_string(),
            Value::String(self.tool_name.clone()),
        );

        if !res.content.is_empty()
            && let Ok(content) = serde_json::to_value(&res.content)
        {
            result
                .metadata
                .insert(MCP_META_RESULT_CONTENT.to_string(), content);
        }
        if let Some(structured) = res.structured_content.clone() {
            result
                .metadata
                .insert(MCP_META_RESULT_STRUCTURED_CONTENT.to_string(), structured);
        }

        if let Some(ref uri) = self.ui_resource_uri
            && let Some(content) = fetch_ui_resource(&transport, uri).await
        {
            result.metadata.insert(
                MCP_META_UI_RESOURCE_URI.to_string(),
                Value::String(uri.clone()),
            );
            result
                .metadata
                .insert(MCP_META_UI_CONTENT.to_string(), Value::String(content.text));
            result.metadata.insert(
                MCP_META_UI_MIME_TYPE.to_string(),
                Value::String(content.mime_type),
            );
        }

        Ok(result.into())
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
    ctx.report_progress(
        ProgressStatus::Running,
        update.message.as_deref(),
        Some(normalized_progress),
    )
    .await;
}

fn map_mcp_error(e: McpTransportError) -> ToolError {
    match e {
        McpTransportError::UnknownTool(name) => ToolError::NotFound(name),
        McpTransportError::Timeout(msg) => ToolError::ExecutionFailed(format!("timeout: {}", msg)),
        other => ToolError::ExecutionFailed(other.to_string()),
    }
}

fn transport_type_from_config(config: &McpServerConnectionConfig) -> Option<TransportTypeId> {
    if config.url.is_some() {
        Some(TransportTypeId::Http)
    } else if config.command.is_some() {
        Some(TransportTypeId::Stdio)
    } else {
        None
    }
}

fn server_runtime(slot: &McpServerSlot) -> Result<&McpServerRuntime, McpError> {
    if slot.lifecycle == McpServerLifecycle::Disabled {
        return Err(McpError::ServerDisabled(slot.meta.name.clone()));
    }

    if slot.lifecycle == McpServerLifecycle::PermanentlyFailed {
        return Err(McpError::ServerPermanentlyFailed(slot.meta.name.clone()));
    }

    slot.runtime
        .as_ref()
        .ok_or_else(|| McpError::Transport("connection closed".to_string()))
}

fn resolve_live_transport(
    state: &Weak<McpRegistryState>,
    server_name: &str,
) -> Result<Arc<dyn McpToolTransport>, McpError> {
    let Some(state) = state.upgrade() else {
        return Err(McpError::RuntimeUnavailable);
    };
    let servers = read_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    let runtime = server_runtime(&servers[index])?;
    Ok(runtime.transport.clone())
}

fn should_track_transport_failure(err: &McpTransportError) -> bool {
    matches!(
        err,
        McpTransportError::ConnectionClosed
            | McpTransportError::Timeout(_)
            | McpTransportError::TransportError(_)
            | McpTransportError::ProtocolError(_)
    )
}

// ── Server runtime ──

#[derive(Clone)]
struct McpServerRuntime {
    transport_type: TransportTypeId,
    transport: Arc<dyn McpToolTransport>,
    capabilities: Option<ServerCapabilities>,
}

struct McpServerMetadata {
    name: String,
    config: McpServerConnectionConfig,
}

impl Clone for McpServerMetadata {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            config: self.config.clone(),
        }
    }
}

type PublishedTools = HashMap<String, Arc<dyn Tool>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpServerLifecycle {
    Disabled,
    Connected,
    Disconnected,
    PermanentlyFailed,
}

impl Clone for McpServerSlot {
    fn clone(&self) -> Self {
        Self {
            meta: self.meta.clone(),
            lifecycle: self.lifecycle,
            runtime: self.runtime.clone(),
            health: self.health.clone(),
            reconnect_attempts: self.reconnect_attempts,
            tools_cache: self.tools_cache.clone(),
            published_tools: self.published_tools.clone(),
        }
    }
}

struct McpServerSlot {
    meta: McpServerMetadata,
    lifecycle: McpServerLifecycle,
    runtime: Option<McpServerRuntime>,
    health: McpRefreshHealth,
    reconnect_attempts: u32,
    tools_cache: Vec<McpToolDefinition>,
    published_tools: PublishedTools,
}

// ── Registry snapshot ──

#[derive(Clone, Default)]
struct McpRegistrySnapshot {
    version: u64,
    tools: HashMap<String, Arc<dyn Tool>>,
}

struct McpRegistryState {
    servers: RwLock<Vec<McpServerSlot>>,
    snapshot: RwLock<McpRegistrySnapshot>,
    periodic_refresh: PeriodicRefresher,
    sampling_handler: Option<Arc<dyn SamplingHandler>>,
    lifecycle_lock: AsyncMutex<()>,
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

fn discover_tools(servers: &[McpServerSlot]) -> Result<HashMap<String, Arc<dyn Tool>>, McpError> {
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    for slot in servers {
        if matches!(
            slot.lifecycle,
            McpServerLifecycle::Disabled | McpServerLifecycle::PermanentlyFailed
        ) {
            continue;
        }

        for (tool_id, tool) in &slot.published_tools {
            if tools.contains_key(tool_id) {
                return Err(McpError::ToolIdConflict(tool_id.clone()));
            }
            tools.insert(tool_id.clone(), tool.clone());
        }
    }

    Ok(tools)
}

fn build_published_tools(
    state: Weak<McpRegistryState>,
    server_name: &str,
    defs: &[McpToolDefinition],
    transport_type: TransportTypeId,
) -> Result<PublishedTools, McpError> {
    let mut published = HashMap::with_capacity(defs.len());

    for def in defs {
        let tool_id = to_tool_id(server_name, &def.name)?;
        if published.contains_key(&tool_id) {
            return Err(McpError::ToolIdConflict(tool_id));
        }
        published.insert(
            tool_id.clone(),
            Arc::new(McpTool::new(
                state.clone(),
                tool_id,
                server_name.to_string(),
                def.clone(),
                transport_type,
            )) as Arc<dyn Tool>,
        );
    }

    Ok(published)
}

async fn connect_server(
    slot: &mut McpServerSlot,
    sampling_handler: Option<Arc<dyn SamplingHandler>>,
) -> Result<(), McpError> {
    let attempted_at = SystemTime::now();

    let transport = match connect_transport(&slot.meta.config, sampling_handler).await {
        Ok(transport) => transport,
        Err(err) => {
            let err: McpError = err.into();
            slot.runtime = None;
            slot.lifecycle = McpServerLifecycle::Disconnected;
            mark_server_failure(slot, attempted_at, &err);
            return Err(err);
        }
    };

    let transport_type = transport.transport_type();
    let capabilities = match transport.server_capabilities().await {
        Ok(capabilities) => capabilities,
        Err(err) => {
            let err: McpError = err.into();
            slot.runtime = None;
            slot.lifecycle = McpServerLifecycle::Disconnected;
            mark_server_failure(slot, attempted_at, &err);
            return Err(err);
        }
    };

    slot.runtime = Some(McpServerRuntime {
        transport_type,
        transport,
        capabilities,
    });
    slot.lifecycle = McpServerLifecycle::Connected;

    reset_server_health_on_success(slot, attempted_at);
    Ok(())
}

async fn disconnect_server(slot: &mut McpServerSlot) -> Result<(), McpError> {
    let transport = slot
        .runtime
        .as_ref()
        .map(|runtime| runtime.transport.clone());

    if let Some(transport) = transport {
        transport.close().await?;
        slot.runtime = None;
        if slot.lifecycle == McpServerLifecycle::Connected {
            slot.lifecycle = McpServerLifecycle::Disconnected;
        }
    }

    Ok(())
}

async fn refresh_server(
    slot: &mut McpServerSlot,
    state_weak: &Weak<McpRegistryState>,
) -> Result<(), McpError> {
    let attempted_at = SystemTime::now();
    slot.health.last_attempt_at = Some(attempted_at);

    if slot.lifecycle == McpServerLifecycle::Disabled {
        slot.tools_cache.clear();
        return Ok(());
    }

    if slot.lifecycle == McpServerLifecycle::PermanentlyFailed {
        slot.tools_cache.clear();
        slot.health.permanently_failed = true;
        return Ok(());
    }

    let server_name = slot.meta.name.clone();

    let transport_type = match slot.runtime.as_ref() {
        Some(runtime) => runtime.transport_type,
        None => {
            let err = McpError::Transport(format!("server '{}' is not connected", server_name));
            slot.lifecycle = McpServerLifecycle::Disconnected;
            mark_server_failure(slot, attempted_at, &err);
            return Err(err);
        }
    };
    let transport = slot
        .runtime
        .as_ref()
        .expect("runtime checked")
        .transport
        .clone();

    let mut defs = match transport.list_tools().await {
        Ok(defs) => defs,
        Err(err) => {
            let err: McpError = err.into();
            mark_server_failure(slot, attempted_at, &err);
            return Err(err);
        }
    };

    defs.sort_by(|a, b| a.name.cmp(&b.name));
    slot.tools_cache = defs;
    reset_server_health_on_success(slot, attempted_at);
    slot.published_tools = build_published_tools(
        state_weak.clone(),
        &server_name,
        &slot.tools_cache,
        transport_type,
    )?;

    Ok(())
}

async fn rebuild_snapshot(state: &McpRegistryState) -> Result<u64, McpError> {
    let tools = {
        let servers = read_lock(&state.servers);
        discover_tools(&servers)?
    };

    let mut snapshot = write_lock(&state.snapshot);
    let version = snapshot.version.saturating_add(1);
    *snapshot = McpRegistrySnapshot { version, tools };
    Ok(version)
}

fn find_server_index(servers: &[McpServerSlot], name: &str) -> Result<usize, McpError> {
    servers
        .iter()
        .position(|slot| slot.meta.name == name)
        .ok_or_else(|| McpError::UnknownServer(name.to_string()))
}

fn server_is_active(slot: &McpServerSlot) -> bool {
    slot.lifecycle == McpServerLifecycle::Connected && slot.runtime.is_some()
}

fn server_can_refresh(slot: &McpServerSlot) -> bool {
    matches!(
        slot.lifecycle,
        McpServerLifecycle::Connected | McpServerLifecycle::Disconnected
    )
}

fn reconnect_backoff(attempt: u32) -> Duration {
    const MAX_SHIFT: u32 = 4;
    let shift = attempt.min(MAX_SHIFT);
    if cfg!(test) {
        Duration::from_millis(1_u64 << shift)
    } else {
        Duration::from_secs(1_u64 << shift)
    }
}

fn reset_server_health_on_success(slot: &mut McpServerSlot, attempted_at: SystemTime) {
    slot.health.last_attempt_at = Some(attempted_at);
    slot.health.last_success_at = Some(attempted_at);
    slot.health.last_error = None;
    slot.health.consecutive_failures = 0;
    slot.health.reconnecting = false;
    slot.health.permanently_failed = false;

    slot.reconnect_attempts = 0;
    slot.lifecycle = McpServerLifecycle::Connected;
}

fn mark_server_failure(slot: &mut McpServerSlot, attempted_at: SystemTime, err: &McpError) {
    slot.health.last_attempt_at = Some(attempted_at);
    slot.health.last_error = Some(err.to_string());
    slot.health.consecutive_failures = slot.health.consecutive_failures.saturating_add(1);
    slot.health.reconnecting = false;
    slot.health.permanently_failed = slot.lifecycle == McpServerLifecycle::PermanentlyFailed;
}

fn mark_server_permanent_failure(slot: &mut McpServerSlot) {
    slot.lifecycle = McpServerLifecycle::PermanentlyFailed;
    slot.runtime = None;
    slot.tools_cache.clear();
    slot.published_tools.clear();
    slot.health.permanently_failed = true;
    slot.health.reconnecting = false;
}

fn finish_reconnect_failure(slot: &mut McpServerSlot, err: &McpError) {
    slot.health.last_error = Some(err.to_string());
    slot.reconnect_attempts = slot.reconnect_attempts.saturating_add(1);
    slot.health.reconnecting = false;

    if slot.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
        mark_server_permanent_failure(slot);
    }
}

async fn record_tool_transport_success(state: &Weak<McpRegistryState>, server_name: &str) {
    let Some(state) = state.upgrade() else {
        return;
    };

    let mut servers = read_lock(&state.servers).clone();
    let Ok(index) = find_server_index(&servers, server_name) else {
        return;
    };
    let slot = &mut servers[index];

    if !server_can_refresh(slot) || slot.runtime.is_none() {
        return;
    }

    reset_server_health_on_success(slot, SystemTime::now());
    *write_lock(&state.servers) = servers;
}

async fn record_tool_transport_failure(
    state: &Weak<McpRegistryState>,
    server_name: &str,
    err: &McpTransportError,
) {
    if !should_track_transport_failure(err) {
        return;
    }

    let Some(state) = state.upgrade() else {
        return;
    };

    let _lifecycle_guard = state.lifecycle_lock.lock().await;
    let sampling_handler = state.sampling_handler.clone();
    let mut servers = read_lock(&state.servers).clone();
    let Ok(index) = find_server_index(&servers, server_name) else {
        return;
    };
    let slot = &mut servers[index];

    if !server_can_refresh(slot) {
        return;
    }

    let err = McpError::Transport(err.to_string());
    mark_server_failure(slot, SystemTime::now(), &err);

    let mut should_rebuild = false;
    if slot.health.consecutive_failures >= FAILURE_THRESHOLD {
        should_rebuild = true;
        if let Err(reconnect_err) =
            attempt_reconnect(slot, sampling_handler, Arc::downgrade(&state)).await
        {
            tracing::warn!(
                error = %reconnect_err,
                server = %slot.meta.name,
                attempts = slot.reconnect_attempts,
                "MCP tool-call reconnect failed"
            );
        }
    }

    *write_lock(&state.servers) = servers;
    if should_rebuild {
        let _ = rebuild_snapshot(state.as_ref()).await;
    }
}

async fn refresh_state(state: Arc<McpRegistryState>) -> Result<u64, McpError> {
    let _lifecycle_guard = state.lifecycle_lock.lock().await;
    let sampling_handler = state.sampling_handler.clone();
    let state_weak = Arc::downgrade(&state);

    let mut servers = read_lock(&state.servers).clone();

    for slot in &mut servers {
        if !server_can_refresh(slot) {
            continue;
        }

        if let Err(err) = refresh_server(slot, &state_weak).await {
            tracing::warn!(error = %err, server = %slot.meta.name, "MCP server refresh failed");

            if slot.health.consecutive_failures >= FAILURE_THRESHOLD {
                if let Err(reconnect_err) =
                    attempt_reconnect(slot, sampling_handler.clone(), state_weak.clone()).await
                {
                    tracing::warn!(
                        error = %reconnect_err,
                        server = %slot.meta.name,
                        attempts = slot.reconnect_attempts,
                        "MCP server reconnect failed"
                    );
                }
            }
        }
    }

    *write_lock(&state.servers) = servers;

    rebuild_snapshot(state.as_ref()).await
}

async fn attempt_reconnect(
    slot: &mut McpServerSlot,
    sampling_handler: Option<Arc<dyn SamplingHandler>>,
    state_weak: Weak<McpRegistryState>,
) -> Result<(), McpError> {
    if slot.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
        mark_server_permanent_failure(slot);
        return Err(McpError::ServerPermanentlyFailed(slot.meta.name.clone()));
    }

    slot.health.reconnecting = true;
    let reconnect_result = async {
        disconnect_server(slot).await?;

        let backoff = reconnect_backoff(slot.reconnect_attempts);
        tokio::time::sleep(backoff).await;

        connect_server(slot, sampling_handler).await?;
        refresh_server(slot, &state_weak).await?;
        Ok::<(), McpError>(())
    }
    .await;

    match reconnect_result {
        Ok(()) => {
            slot.reconnect_attempts = 0;
            slot.health.reconnecting = false;
            slot.health.permanently_failed = false;
            Ok(())
        }
        Err(err) => {
            finish_reconnect_failure(slot, &err);
            Err(err)
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
        f.debug_struct("McpToolRegistryManager")
            .field("servers", &read_lock(&self.state.servers).len())
            .field("tools", &snapshot.tools.len())
            .field("version", &snapshot.version)
            .field(
                "periodic_refresh_running",
                &self.state.periodic_refresh.is_running(),
            )
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
        Self::from_tool_transports(entries, sampling_handler).await
    }

    pub async fn from_transports(
        entries: impl IntoIterator<Item = (McpServerConnectionConfig, Arc<dyn McpToolTransport>)>,
    ) -> Result<Self, McpError> {
        Self::from_tool_transports(entries, None).await
    }

    async fn from_tool_transports(
        entries: impl IntoIterator<Item = (McpServerConnectionConfig, Arc<dyn McpToolTransport>)>,
        sampling_handler: Option<Arc<dyn SamplingHandler>>,
    ) -> Result<Self, McpError> {
        let servers = Self::build_servers(entries).await?;
        let state = Arc::new(McpRegistryState {
            servers: RwLock::new(servers),
            snapshot: RwLock::new(McpRegistrySnapshot::default()),
            periodic_refresh: PeriodicRefresher::new(),
            sampling_handler,
            lifecycle_lock: AsyncMutex::new(()),
        });

        {
            let state_weak = Arc::downgrade(&state);
            let mut servers = read_lock(&state.servers).clone();
            for slot in &mut servers {
                refresh_server(slot, &state_weak).await?;
            }
            *write_lock(&state.servers) = servers;
        }

        rebuild_snapshot(state.as_ref()).await?;
        Ok(Self { state })
    }

    async fn build_servers(
        entries: impl IntoIterator<Item = (McpServerConnectionConfig, Arc<dyn McpToolTransport>)>,
    ) -> Result<Vec<McpServerSlot>, McpError> {
        let mut servers = Vec::new();
        let mut names: HashSet<String> = HashSet::new();
        let connected_at = SystemTime::now();

        for (cfg, transport) in entries {
            validate_server_name(&cfg.name)?;
            if !names.insert(cfg.name.clone()) {
                return Err(McpError::DuplicateServerName(cfg.name));
            }
            let capabilities = transport.server_capabilities().await?;

            servers.push(McpServerSlot {
                meta: McpServerMetadata {
                    name: cfg.name.clone(),
                    config: cfg,
                },
                lifecycle: McpServerLifecycle::Connected,
                runtime: Some(McpServerRuntime {
                    transport_type: transport.transport_type(),
                    transport,
                    capabilities,
                }),
                health: McpRefreshHealth {
                    last_attempt_at: Some(connected_at),
                    last_success_at: Some(connected_at),
                    last_error: None,
                    consecutive_failures: 0,
                    reconnecting: false,
                    permanently_failed: false,
                },
                reconnect_attempts: 0,
                tools_cache: Vec::new(),
                published_tools: HashMap::new(),
            });
        }

        servers.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        Ok(servers)
    }

    pub async fn refresh(&self) -> Result<u64, McpError> {
        refresh_state(self.state.clone()).await
    }

    pub fn start_periodic_refresh(&self, interval: Duration) -> Result<(), McpError> {
        let weak_state = Arc::downgrade(&self.state);
        self.state
            .periodic_refresh
            .start(interval, move || {
                let weak = weak_state.clone();
                async move {
                    let Some(state) = weak.upgrade() else {
                        return;
                    };
                    if let Err(err) = refresh_state(state).await {
                        tracing::warn!(error = %err, "MCP periodic refresh failed");
                    }
                }
            })
            .map_err(|msg| match msg.as_str() {
                m if m.contains("non-zero") => McpError::InvalidRefreshInterval,
                m if m.contains("already running") => McpError::PeriodicRefreshAlreadyRunning,
                _ => McpError::RuntimeUnavailable,
            })
    }

    pub async fn stop_periodic_refresh(&self) -> bool {
        self.state.periodic_refresh.stop().await
    }

    pub fn periodic_refresh_running(&self) -> bool {
        self.state.periodic_refresh.is_running()
    }

    pub fn registry(&self) -> McpToolRegistry {
        McpToolRegistry {
            state: self.state.clone(),
        }
    }

    pub fn version(&self) -> u64 {
        read_lock(&self.state.snapshot).version
    }
    pub fn server_health(&self, server_name: &str) -> Result<McpRefreshHealth, McpError> {
        let servers = read_lock(&self.state.servers);
        let index = find_server_index(&servers, server_name)?;
        Ok(servers[index].health.clone())
    }

    pub fn servers(&self) -> Vec<(String, TransportTypeId)> {
        let servers = read_lock(&self.state.servers);

        servers
            .iter()
            .map(|slot| {
                let transport_type = slot
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.transport_type)
                    .or_else(|| transport_type_from_config(&slot.meta.config))
                    .unwrap_or(TransportTypeId::Stdio);

                (slot.meta.name.clone(), transport_type)
            })
            .collect()
    }

    pub async fn list_prompts(&self) -> Result<Vec<McpPromptEntry>, McpError> {
        let mut prompts = Vec::new();

        let servers: Vec<(
            String,
            TransportTypeId,
            Arc<dyn McpToolTransport>,
            Option<ServerCapabilities>,
        )> = {
            let guard = read_lock(&self.state.servers);

            guard
                .iter()
                .filter(|slot| server_is_active(slot))
                .filter_map(|slot| {
                    let runtime = slot.runtime.as_ref()?;
                    Some((
                        slot.meta.name.clone(),
                        runtime.transport_type,
                        runtime.transport.clone(),
                        runtime.capabilities.clone(),
                    ))
                })
                .collect()
        };

        for (server_name, transport_type, transport, capabilities) in servers {
            if !server_supports_prompts(capabilities.as_ref()) {
                continue;
            }

            let mut defs = match transport.list_prompts().await {
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
                server_name: server_name.clone(),
                transport_type,
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
        let (transport, capabilities, resolved_server_name) = {
            let servers = read_lock(&self.state.servers);
            let index = find_server_index(&servers, server_name)?;
            let slot = &servers[index];
            let runtime = server_runtime(slot)?;

            (
                runtime.transport.clone(),
                runtime.capabilities.clone(),
                slot.meta.name.clone(),
            )
        };

        if !server_supports_prompts(capabilities.as_ref()) {
            return Err(McpError::UnsupportedCapability {
                server_name: resolved_server_name,
                capability: "prompts",
            });
        }

        transport
            .get_prompt(prompt_name, arguments)
            .await
            .map_err(Into::into)
    }

    pub async fn list_resources(&self) -> Result<Vec<McpResourceEntry>, McpError> {
        let mut resources = Vec::new();

        let servers: Vec<(
            String,
            TransportTypeId,
            Arc<dyn McpToolTransport>,
            Option<ServerCapabilities>,
        )> = {
            let guard = read_lock(&self.state.servers);

            guard
                .iter()
                .filter(|slot| server_is_active(slot))
                .filter_map(|slot| {
                    let runtime = slot.runtime.as_ref()?;
                    Some((
                        slot.meta.name.clone(),
                        runtime.transport_type,
                        runtime.transport.clone(),
                        runtime.capabilities.clone(),
                    ))
                })
                .collect()
        };

        for (server_name, transport_type, transport, capabilities) in servers {
            if !server_supports_resources(capabilities.as_ref()) {
                continue;
            }

            let mut defs = match transport.list_resources().await {
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
                server_name: server_name.clone(),
                transport_type,
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
        let (transport, capabilities, resolved_server_name) = {
            let servers = read_lock(&self.state.servers);
            let index = find_server_index(&servers, server_name)?;
            let slot = &servers[index];
            let runtime = server_runtime(slot)?;

            (
                runtime.transport.clone(),
                runtime.capabilities.clone(),
                slot.meta.name.clone(),
            )
        };

        if !server_supports_resources(capabilities.as_ref()) {
            return Err(McpError::UnsupportedCapability {
                server_name: resolved_server_name,
                capability: "resources",
            });
        }

        transport.read_resource(uri).await.map_err(Into::into)
    }
    pub async fn reconnect(&self, server_name: &str) -> Result<(), McpError> {
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;
        let sampling_handler = self.state.sampling_handler.clone();
        let mut servers = read_lock(&self.state.servers).clone();
        let index = find_server_index(&servers, server_name)?;
        let slot = &mut servers[index];

        if slot.lifecycle == McpServerLifecycle::Disabled {
            return Err(McpError::ServerDisabled(server_name.to_string()));
        }

        slot.health.reconnecting = true;

        let reconnect_result = async {
            disconnect_server(slot).await?;
            connect_server(slot, sampling_handler).await?;
            slot.reconnect_attempts = 0;
            slot.health.consecutive_failures = 0;
            slot.health.permanently_failed = false;
            slot.health.reconnecting = false;
            refresh_server(slot, &Arc::downgrade(&self.state)).await?;
            Ok::<(), McpError>(())
        }
        .await;

        if reconnect_result.is_err() {
            slot.health.reconnecting = false;
        }

        *write_lock(&self.state.servers) = servers;

        reconnect_result?;
        rebuild_snapshot(self.state.as_ref()).await?;
        Ok(())
    }

    pub async fn toggle(&self, server_name: &str, enabled: bool) -> Result<(), McpError> {
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;
        let sampling_handler = self.state.sampling_handler.clone();
        let mut servers = read_lock(&self.state.servers).clone();
        let index = find_server_index(&servers, server_name)?;
        let slot = &mut servers[index];

        let toggle_result = async {
            if !enabled {
                slot.lifecycle = McpServerLifecycle::Disabled;
                slot.health.reconnecting = false;
                disconnect_server(slot).await?;
                slot.tools_cache.clear();
                slot.published_tools.clear();
                return Ok::<(), McpError>(());
            }

            slot.lifecycle = McpServerLifecycle::Disconnected;
            slot.reconnect_attempts = 0;
            slot.health.consecutive_failures = 0;
            slot.health.last_error = None;
            slot.health.reconnecting = false;
            slot.health.permanently_failed = false;

            connect_server(slot, sampling_handler).await?;
            refresh_server(slot, &Arc::downgrade(&self.state)).await?;
            Ok(())
        }
        .await;

        *write_lock(&self.state.servers) = servers;

        toggle_result?;
        rebuild_snapshot(self.state.as_ref()).await?;
        Ok(())
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
        f.debug_struct("McpToolRegistry")
            .field("servers", &read_lock(&self.state.servers).len())
            .field("tools", &snapshot.tools.len())
            .field("version", &snapshot.version)
            .field(
                "periodic_refresh_running",
                &self.state.periodic_refresh.is_running(),
            )
            .finish()
    }
}

impl McpToolRegistry {
    pub fn version(&self) -> u64 {
        read_lock(&self.state.snapshot).version
    }
    pub fn server_health(&self, server_name: &str) -> Result<McpRefreshHealth, McpError> {
        let servers = read_lock(&self.state.servers);
        let index = find_server_index(&servers, server_name)?;
        Ok(servers[index].health.clone())
    }

    pub fn servers(&self) -> Vec<(String, TransportTypeId)> {
        let servers = read_lock(&self.state.servers);

        servers
            .iter()
            .map(|slot| {
                let transport_type = slot
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.transport_type)
                    .or_else(|| transport_type_from_config(&slot.meta.config))
                    .unwrap_or(TransportTypeId::Stdio);

                (slot.meta.name.clone(), transport_type)
            })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConnectionConfig;
    use crate::progress::McpProgressUpdate;
    use crate::transport::McpToolTransport;
    use async_trait::async_trait;
    use mcp::transport::{McpTransportError, ServerCapabilities, TransportTypeId};
    use mcp::{CallToolResult, McpToolDefinition};
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::sync::{Notify, Semaphore, mpsc};

    // ── Mock transport ──

    #[derive(Debug, Default)]
    struct MockTransport {
        tools: Vec<McpToolDefinition>,
        capabilities: Option<ServerCapabilities>,
    }

    impl MockTransport {
        fn with_tools(tools: Vec<McpToolDefinition>) -> Self {
            Self {
                tools,
                capabilities: None,
            }
        }

        fn tool_def(name: &str) -> McpToolDefinition {
            McpToolDefinition {
                name: name.to_string(),
                title: Some(format!("{name} title")),
                description: Some(format!("{name} desc")),
                input_schema: json!({"type": "object"}),
                group: None,
                meta: None,
                icons: None,
                output_schema: None,
                execution: None,
                annotations: None,
            }
        }
    }

    #[derive(Debug)]
    struct FailingRefreshTransport {
        tools: Vec<McpToolDefinition>,
        failures_remaining: Arc<Mutex<usize>>,
    }

    impl FailingRefreshTransport {
        fn new(tools: Vec<McpToolDefinition>) -> Self {
            Self {
                tools,
                failures_remaining: Arc::new(Mutex::new(0)),
            }
        }

        fn fail_next_refreshes(&self, failures: usize) {
            *self.failures_remaining.lock().unwrap() = failures;
        }
    }

    #[derive(Debug)]
    struct CloseFailingTransport {
        tools: Vec<McpToolDefinition>,
        close_error: &'static str,
    }

    impl CloseFailingTransport {
        fn new(tool_name: &str, close_error: &'static str) -> Self {
            Self {
                tools: vec![MockTransport::tool_def(tool_name)],
                close_error,
            }
        }
    }

    #[async_trait]
    impl McpToolTransport for FailingRefreshTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let mut failures_remaining = self.failures_remaining.lock().unwrap();
            if *failures_remaining > 0 {
                *failures_remaining -= 1;
                return Err(McpTransportError::TransportError(
                    "scripted refresh failure".to_string(),
                ));
            }
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![mcp::ToolContent::Text {
                    text: format!("called {name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    #[async_trait]
    impl McpToolTransport for CloseFailingTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![mcp::ToolContent::Text {
                    text: format!("called {name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }

        async fn close(&self) -> Result<(), McpTransportError> {
            Err(McpTransportError::TransportError(
                self.close_error.to_string(),
            ))
        }
    }

    #[derive(Debug)]
    struct BlockingListTransport {
        tools: Vec<McpToolDefinition>,
        entered: Arc<Semaphore>,
        release: Arc<Notify>,
        call_count: Arc<Mutex<usize>>,
    }

    impl BlockingListTransport {
        fn new(tools: Vec<McpToolDefinition>) -> Self {
            Self {
                tools,
                entered: Arc::new(Semaphore::new(0)),
                release: Arc::new(Notify::new()),
                call_count: Arc::new(Mutex::new(0)),
            }
        }
    }

    #[derive(Debug)]
    struct RecordingTransport {
        tools: Vec<McpToolDefinition>,
        calls: Arc<Mutex<Vec<String>>>,
        response_text: String,
    }

    impl RecordingTransport {
        fn new(tool_name: &str, response_text: &str) -> Self {
            Self {
                tools: vec![MockTransport::tool_def(tool_name)],
                calls: Arc::new(Mutex::new(Vec::new())),
                response_text: response_text.to_string(),
            }
        }
    }

    #[async_trait]
    impl McpToolTransport for RecordingTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<CallToolResult, McpTransportError> {
            self.calls.lock().unwrap().push(name.to_string());
            Ok(CallToolResult {
                content: vec![mcp::ToolContent::Text {
                    text: self.response_text.clone(),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    #[derive(Debug)]
    struct FailingCallTransport {
        tools: Vec<McpToolDefinition>,
        connection_closed: bool,
    }

    impl FailingCallTransport {
        fn connection_closed(tool_name: &str) -> Self {
            Self {
                tools: vec![MockTransport::tool_def(tool_name)],
                connection_closed: true,
            }
        }
    }

    #[async_trait]
    impl McpToolTransport for FailingCallTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<CallToolResult, McpTransportError> {
            if self.connection_closed {
                Err(McpTransportError::ConnectionClosed)
            } else {
                Err(McpTransportError::TransportError(
                    "scripted tool call failure".to_string(),
                ))
            }
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    #[async_trait]
    impl McpToolTransport for BlockingListTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let should_block = {
                let mut call_count = self.call_count.lock().unwrap();
                *call_count += 1;
                *call_count > 1
            };

            if should_block {
                self.entered.add_permits(1);
                self.release.notified().await;
            }
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<CallToolResult, McpTransportError> {
            unreachable!()
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    #[async_trait]
    impl McpToolTransport for MockTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![mcp::ToolContent::Text {
                    text: format!("called {name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }

        async fn server_capabilities(
            &self,
        ) -> Result<Option<ServerCapabilities>, McpTransportError> {
            Ok(self.capabilities.clone())
        }
    }

    fn cfg(name: &str) -> McpServerConnectionConfig {
        McpServerConnectionConfig::stdio(name, "echo", vec!["ok".to_string()])
    }

    async fn make_manager_with(
        entries: Vec<(&str, Vec<McpToolDefinition>)>,
    ) -> McpToolRegistryManager {
        let transports: Vec<(McpServerConnectionConfig, Arc<dyn McpToolTransport>)> = entries
            .into_iter()
            .map(|(name, tools)| {
                (
                    cfg(name),
                    Arc::new(MockTransport::with_tools(tools)) as Arc<dyn McpToolTransport>,
                )
            })
            .collect();
        McpToolRegistryManager::from_transports(transports)
            .await
            .unwrap()
    }

    fn test_slot(name: &str, transport: Arc<dyn McpToolTransport>) -> McpServerSlot {
        McpServerSlot {
            meta: McpServerMetadata {
                name: name.to_string(),
                config: cfg(name),
            },
            lifecycle: McpServerLifecycle::Connected,
            runtime: Some(McpServerRuntime {
                transport_type: transport.transport_type(),
                transport,
                capabilities: None,
            }),
            health: McpRefreshHealth::default(),
            reconnect_attempts: 0,
            tools_cache: vec![MockTransport::tool_def("echo")],
            published_tools: HashMap::new(),
        }
    }

    // ── McpTool descriptor format ──

    #[tokio::test]
    async fn mcp_tool_descriptor_encodes_server_and_tool_name() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        let registry = mgr.registry();
        let tool = registry.get("mcp__srv__echo").unwrap();
        let desc = tool.descriptor();
        assert_eq!(desc.id, "mcp__srv__echo");
        assert_eq!(
            desc.metadata.get("mcp.server").and_then(|v| v.as_str()),
            Some("srv")
        );
        assert_eq!(
            desc.metadata.get("mcp.tool").and_then(|v| v.as_str()),
            Some("echo")
        );
    }

    // ── McpToolRegistry ──

    #[tokio::test]
    async fn mcp_tool_registry_ids_sorted() {
        let mgr = make_manager_with(vec![(
            "srv",
            vec![
                MockTransport::tool_def("beta"),
                MockTransport::tool_def("alpha"),
            ],
        )])
        .await;
        let registry = mgr.registry();
        let ids = registry.ids();
        assert_eq!(
            ids,
            vec!["mcp__srv__alpha".to_string(), "mcp__srv__beta".to_string()]
        );
    }

    #[tokio::test]
    async fn mcp_tool_registry_get_returns_correct_tool() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        let registry = mgr.registry();
        assert!(registry.get("mcp__srv__echo").is_some());
        assert!(registry.get("mcp__srv__missing").is_none());
    }

    #[tokio::test]
    async fn mcp_tool_registry_empty() {
        let mgr = make_manager_with(vec![("srv", Vec::new())]).await;
        let registry = mgr.registry();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.ids().is_empty());
    }

    #[tokio::test]
    async fn mcp_tool_registry_version_starts_at_one() {
        let mgr = make_manager_with(vec![("srv", Vec::new())]).await;
        assert_eq!(mgr.version(), 1);
        assert_eq!(mgr.registry().version(), 1);
    }

    #[tokio::test]
    async fn mcp_tool_registry_snapshot_matches_ids() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("t1")])]).await;
        let registry = mgr.registry();
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key("mcp__srv__t1"));
    }

    // ── McpToolRegistryManager error cases ──

    #[tokio::test]
    async fn manager_rejects_empty_server_name() {
        let result = McpToolRegistryManager::from_transports(vec![(
            cfg(""),
            Arc::new(MockTransport::default()) as Arc<dyn McpToolTransport>,
        )])
        .await;
        // cfg("") still has name="" but validate_server_name checks after
        // The config struct sets name to empty string
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn manager_rejects_duplicate_server_names() {
        let result = McpToolRegistryManager::from_transports(vec![
            (
                cfg("dup"),
                Arc::new(MockTransport::default()) as Arc<dyn McpToolTransport>,
            ),
            (
                cfg("dup"),
                Arc::new(MockTransport::default()) as Arc<dyn McpToolTransport>,
            ),
        ])
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, McpError::DuplicateServerName(_)));
    }

    #[tokio::test]
    async fn manager_rejects_tool_id_conflict() {
        // Two servers with tools that map to the same tool_id after sanitization
        // Create a transport that returns tool "a_b" and another with "a-b"
        // Both sanitize to "a_b", so they'd conflict if on the same server
        // But tool_id includes server name, so we need same server+tool

        #[derive(Debug)]
        struct DupToolTransport;

        #[async_trait]
        impl McpToolTransport for DupToolTransport {
            async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
                Ok(vec![
                    MockTransport::tool_def("echo"),
                    MockTransport::tool_def("echo"),
                ])
            }
            async fn call_tool(
                &self,
                _name: &str,
                _args: Value,
                _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
            ) -> Result<CallToolResult, McpTransportError> {
                unreachable!()
            }
            fn transport_type(&self) -> TransportTypeId {
                TransportTypeId::Stdio
            }
            async fn server_capabilities(
                &self,
            ) -> Result<Option<ServerCapabilities>, McpTransportError> {
                Ok(None)
            }
        }

        let result = McpToolRegistryManager::from_transports(vec![(
            cfg("srv"),
            Arc::new(DupToolTransport) as Arc<dyn McpToolTransport>,
        )])
        .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), McpError::ToolIdConflict(_)));
    }

    // ── Refresh ──

    #[tokio::test]
    async fn manager_refresh_increments_version() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("t1")])]).await;
        assert_eq!(mgr.version(), 1);

        let v = mgr.refresh().await.unwrap();
        assert_eq!(v, 2);
        assert_eq!(mgr.version(), 2);
    }

    #[tokio::test]
    async fn manager_server_health_returns_per_server_state() {
        let mgr = make_manager_with(vec![("srv", Vec::new())]).await;
        let health = mgr.server_health("srv").unwrap();
        assert!(health.last_success_at.is_some());
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.last_error.is_none());
    }
    #[tokio::test]
    async fn manager_server_health_rejects_unknown_server() {
        let mgr = make_manager_with(vec![("srv", Vec::new())]).await;
        let err = mgr.server_health("missing").unwrap_err();
        assert!(matches!(err, McpError::UnknownServer(_)));
    }

    #[tokio::test]
    async fn manager_servers_returns_names_and_types() {
        let mgr = make_manager_with(vec![("alpha", Vec::new()), ("beta", Vec::new())]).await;
        let servers = mgr.servers();
        let names: Vec<&str> = servers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    // ── Runtime server management ──

    #[tokio::test]
    async fn manager_toggle_disable_removes_server_tools_from_snapshot() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        let registry = mgr.registry();
        assert!(registry.get("mcp__srv__echo").is_some());

        mgr.toggle("srv", false).await.unwrap();

        let registry = mgr.registry();
        assert!(registry.get("mcp__srv__echo").is_none());
        assert!(registry.ids().is_empty());

        let health = mgr.server_health("srv").unwrap();
        assert!(!health.reconnecting);

        let servers = read_lock(&mgr.state.servers);
        let index = find_server_index(&servers, "srv").unwrap();
        let slot = &servers[index];
        assert_eq!(slot.lifecycle, McpServerLifecycle::Disabled);
        assert!(slot.runtime.is_none());
        assert!(slot.tools_cache.is_empty());
    }

    #[tokio::test]
    async fn manager_reconnect_rejects_disabled_server() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        mgr.toggle("srv", false).await.unwrap();

        let err = mgr.reconnect("srv").await.unwrap_err();
        assert!(matches!(err, McpError::ServerDisabled(name) if name == "srv"));
    }

    #[tokio::test]
    async fn manager_toggle_disable_is_idempotent() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;

        mgr.toggle("srv", false).await.unwrap();
        mgr.toggle("srv", false).await.unwrap();

        let registry = mgr.registry();
        assert!(registry.ids().is_empty());

        let servers = read_lock(&mgr.state.servers);
        let index = find_server_index(&servers, "srv").unwrap();
        let slot = &servers[index];
        assert_eq!(slot.lifecycle, McpServerLifecycle::Disabled);
        assert!(slot.runtime.is_none());
        assert!(slot.tools_cache.is_empty());
    }

    #[tokio::test]
    async fn disconnect_server_close_failure_preserves_runtime_and_lifecycle() {
        let transport = Arc::new(CloseFailingTransport::new("echo", "scripted close failure"))
            as Arc<dyn McpToolTransport>;
        let mut slot = test_slot("srv", transport);

        let err = disconnect_server(&mut slot).await.unwrap_err();

        assert!(err.to_string().contains("scripted close failure"));
        assert_eq!(slot.lifecycle, McpServerLifecycle::Connected);
        assert!(slot.runtime.is_some());
    }

    #[tokio::test]
    async fn refresh_reconnect_starts_only_after_failure_threshold() {
        let transport = Arc::new(FailingRefreshTransport::new(vec![MockTransport::tool_def(
            "echo",
        )]));
        let mgr = McpToolRegistryManager::from_transports(vec![(
            cfg("srv"),
            transport.clone() as Arc<dyn McpToolTransport>,
        )])
        .await
        .unwrap();
        transport.fail_next_refreshes(3);

        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[0].meta.config.args.clear();
            servers[0].meta.config.timeout_secs = 1;
        }

        mgr.refresh().await.unwrap();
        mgr.refresh().await.unwrap();

        let servers = read_lock(&mgr.state.servers);
        assert_eq!(servers[0].health.consecutive_failures, 2);
        assert_eq!(servers[0].reconnect_attempts, 0);
        assert!(servers[0].runtime.is_some());
        assert!(!servers[0].health.reconnecting);

        drop(servers);

        mgr.refresh().await.unwrap();

        let servers = read_lock(&mgr.state.servers);
        assert_eq!(servers[0].reconnect_attempts, 1);
        assert!(servers[0].runtime.is_none());
        assert!(!servers[0].health.reconnecting);
        assert!(servers[0].published_tools.contains_key("mcp__srv__echo"));
    }

    #[tokio::test]
    async fn refresh_reconnect_budget_continues_until_permanent_failure() {
        let transport = Arc::new(FailingRefreshTransport::new(vec![MockTransport::tool_def(
            "echo",
        )]));
        let mgr = McpToolRegistryManager::from_transports(vec![(
            cfg("srv"),
            transport.clone() as Arc<dyn McpToolTransport>,
        )])
        .await
        .unwrap();
        transport.fail_next_refreshes(usize::MAX);

        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[0].meta.config.args.clear();
            servers[0].meta.config.timeout_secs = 1;
        }

        for _ in 0..7 {
            mgr.refresh().await.unwrap();
        }

        let servers = read_lock(&mgr.state.servers);
        assert_eq!(servers[0].reconnect_attempts, MAX_RECONNECT_ATTEMPTS);
        assert_eq!(servers[0].lifecycle, McpServerLifecycle::PermanentlyFailed);
        assert!(servers[0].health.permanently_failed);
        assert!(!servers[0].health.reconnecting);
        assert!(servers[0].runtime.is_none());
        assert!(servers[0].published_tools.is_empty());
    }

    #[tokio::test]
    async fn attempt_reconnect_close_failure_counts_attempt_and_clears_reconnecting() {
        let transport = Arc::new(CloseFailingTransport::new("echo", "scripted close failure"))
            as Arc<dyn McpToolTransport>;
        let mut slot = test_slot("srv", transport);
        slot.health.consecutive_failures = FAILURE_THRESHOLD;
        slot.health.reconnecting = true;
        slot.health.last_error = Some("previous failure".to_string());

        let err = attempt_reconnect(&mut slot, None, Weak::new())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("scripted close failure"));
        assert_eq!(slot.reconnect_attempts, 1);
        assert!(!slot.health.reconnecting);
        assert!(
            slot.health
                .last_error
                .as_deref()
                .is_some_and(|msg| msg.contains("scripted close failure"))
        );
        assert_eq!(slot.lifecycle, McpServerLifecycle::Connected);
        assert!(slot.runtime.is_some());
    }

    #[tokio::test]
    async fn attempt_reconnect_at_budget_marks_permanent_failure_without_transitioning() {
        let transport = Arc::new(MockTransport::with_tools(vec![MockTransport::tool_def(
            "echo",
        )])) as Arc<dyn McpToolTransport>;
        let mut slot = test_slot("srv", transport);
        slot.reconnect_attempts = MAX_RECONNECT_ATTEMPTS;
        slot.health.reconnecting = true;
        slot.published_tools.insert(
            "mcp__srv__echo".to_string(),
            Arc::new(McpTool::new(
                Weak::new(),
                "mcp__srv__echo".to_string(),
                "srv".to_string(),
                MockTransport::tool_def("echo"),
                TransportTypeId::Stdio,
            )) as Arc<dyn Tool>,
        );

        let err = attempt_reconnect(&mut slot, None, Weak::new())
            .await
            .unwrap_err();

        assert!(matches!(err, McpError::ServerPermanentlyFailed(name) if name == "srv"));
        assert_eq!(slot.lifecycle, McpServerLifecycle::PermanentlyFailed);
        assert!(slot.runtime.is_none());
        assert!(slot.tools_cache.is_empty());
        assert!(slot.published_tools.is_empty());
        assert!(slot.health.permanently_failed);
        assert!(!slot.health.reconnecting);
        assert_eq!(slot.reconnect_attempts, MAX_RECONNECT_ATTEMPTS);
    }

    #[tokio::test]
    async fn attempt_reconnect_close_failure_at_budget_marks_permanent_failure() {
        let transport = Arc::new(CloseFailingTransport::new("echo", "scripted close failure"))
            as Arc<dyn McpToolTransport>;
        let mut slot = test_slot("srv", transport);
        slot.reconnect_attempts = MAX_RECONNECT_ATTEMPTS - 1;
        slot.health.consecutive_failures = FAILURE_THRESHOLD;
        slot.health.reconnecting = true;
        slot.published_tools.insert(
            "mcp__srv__echo".to_string(),
            Arc::new(McpTool::new(
                Weak::new(),
                "mcp__srv__echo".to_string(),
                "srv".to_string(),
                MockTransport::tool_def("echo"),
                TransportTypeId::Stdio,
            )) as Arc<dyn Tool>,
        );

        let err = attempt_reconnect(&mut slot, None, Weak::new())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("scripted close failure"));
        assert_eq!(slot.reconnect_attempts, MAX_RECONNECT_ATTEMPTS);
        assert_eq!(slot.lifecycle, McpServerLifecycle::PermanentlyFailed);
        assert!(slot.runtime.is_none());
        assert!(slot.tools_cache.is_empty());
        assert!(slot.published_tools.is_empty());
        assert!(slot.health.permanently_failed);
        assert!(!slot.health.reconnecting);
    }

    #[test]
    fn reset_server_health_on_success_clears_reconnect_state() {
        let transport = Arc::new(MockTransport::with_tools(vec![MockTransport::tool_def(
            "echo",
        )])) as Arc<dyn McpToolTransport>;
        let mut slot = test_slot("srv", transport);
        let attempted_at = SystemTime::now();

        slot.lifecycle = McpServerLifecycle::Disconnected;
        slot.reconnect_attempts = 3;
        slot.health.last_error = Some("boom".to_string());
        slot.health.consecutive_failures = FAILURE_THRESHOLD;
        slot.health.reconnecting = true;
        slot.health.permanently_failed = true;

        reset_server_health_on_success(&mut slot, attempted_at);

        assert_eq!(slot.lifecycle, McpServerLifecycle::Connected);
        assert_eq!(slot.reconnect_attempts, 0);
        assert_eq!(slot.health.consecutive_failures, 0);
        assert!(!slot.health.reconnecting);
        assert!(!slot.health.permanently_failed);
        assert!(slot.health.last_error.is_none());
        assert_eq!(slot.health.last_success_at, Some(attempted_at));
    }

    #[tokio::test]
    async fn manual_reconnect_close_failure_preserves_runtime_and_clears_reconnecting() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].runtime = Some(McpServerRuntime {
                transport_type: TransportTypeId::Stdio,
                transport: Arc::new(CloseFailingTransport::new(
                    "echo",
                    "scripted reconnect close failure",
                )) as Arc<dyn McpToolTransport>,
                capabilities: None,
            });
            servers[0].lifecycle = McpServerLifecycle::Connected;
        }

        let err = mgr.reconnect("srv").await.unwrap_err();
        assert!(err.to_string().contains("scripted reconnect close failure"));

        let servers = read_lock(&mgr.state.servers);
        let slot = &servers[0];
        assert_eq!(slot.lifecycle, McpServerLifecycle::Connected);
        assert!(slot.runtime.is_some());
        assert!(!slot.health.reconnecting);
        assert_eq!(slot.reconnect_attempts, 0);
    }

    #[tokio::test]
    async fn failed_reconnect_keeps_last_good_snapshot() {
        let transport = Arc::new(FailingRefreshTransport::new(vec![MockTransport::tool_def(
            "echo",
        )]));
        let mgr = McpToolRegistryManager::from_transports(vec![(
            cfg("srv"),
            transport.clone() as Arc<dyn McpToolTransport>,
        )])
        .await
        .unwrap();
        transport.fail_next_refreshes(3);

        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[0].meta.config.args.clear();
            servers[0].meta.config.timeout_secs = 1;
        }

        let registry = mgr.registry();
        assert!(registry.get("mcp__srv__echo").is_some());

        mgr.refresh().await.unwrap();
        mgr.refresh().await.unwrap();
        mgr.refresh().await.unwrap();

        assert!(registry.get("mcp__srv__echo").is_some());
        assert!(registry.ids().iter().any(|id| id == "mcp__srv__echo"));
    }

    #[tokio::test]
    async fn failing_server_does_not_affect_other_servers() {
        let failing = Arc::new(FailingRefreshTransport::new(vec![MockTransport::tool_def(
            "echo",
        )]));
        let healthy = Arc::new(MockTransport::with_tools(vec![MockTransport::tool_def(
            "sum",
        )])) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![
            (cfg("bad"), failing.clone() as Arc<dyn McpToolTransport>),
            (cfg("good"), healthy),
        ])
        .await
        .unwrap();
        failing.fail_next_refreshes(3);

        {
            let mut servers = write_lock(&mgr.state.servers);
            let bad_index = find_server_index(&servers, "bad").unwrap();
            servers[bad_index].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[bad_index].meta.config.args.clear();
            servers[bad_index].meta.config.timeout_secs = 1;
        }

        mgr.refresh().await.unwrap();
        mgr.refresh().await.unwrap();
        mgr.refresh().await.unwrap();

        let registry = mgr.registry();
        assert!(registry.get("mcp__good__sum").is_some());
        assert!(registry.get("mcp__bad__echo").is_some());
    }

    #[tokio::test]
    async fn concurrent_reads_during_refresh_do_not_observe_missing_servers() {
        let blocking = Arc::new(BlockingListTransport::new(vec![MockTransport::tool_def(
            "echo",
        )]));
        let entered = blocking.entered.clone();
        let release = blocking.release.clone();
        let mgr = McpToolRegistryManager::from_transports(vec![(
            cfg("srv"),
            blocking as Arc<dyn McpToolTransport>,
        )])
        .await
        .unwrap();

        let mgr_for_refresh = mgr.clone();
        let refresh_task = tokio::spawn(async move { mgr_for_refresh.refresh().await.unwrap() });

        entered.acquire().await.unwrap().forget();

        let names: Vec<String> = mgr.servers().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["srv".to_string()]);
        assert!(mgr.server_health("srv").is_ok());

        release.notify_waiters();
        refresh_task.await.unwrap();
    }

    // ── Periodic refresh ──

    #[tokio::test]
    async fn manager_periodic_refresh_zero_interval_error() {
        let mgr = make_manager_with(vec![("srv", Vec::new())]).await;
        let err = mgr
            .start_periodic_refresh(std::time::Duration::ZERO)
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidRefreshInterval));
    }

    #[tokio::test]
    async fn manager_periodic_refresh_double_start_error() {
        let mgr = make_manager_with(vec![("srv", Vec::new())]).await;
        mgr.start_periodic_refresh(std::time::Duration::from_secs(60))
            .unwrap();
        let err = mgr
            .start_periodic_refresh(std::time::Duration::from_secs(60))
            .unwrap_err();
        assert!(matches!(err, McpError::PeriodicRefreshAlreadyRunning));
        mgr.stop_periodic_refresh().await;
    }

    #[tokio::test]
    async fn manager_stop_periodic_refresh_when_not_running() {
        let mgr = make_manager_with(vec![("srv", Vec::new())]).await;
        assert!(!mgr.stop_periodic_refresh().await);
    }

    // ── McpTool execute ──

    #[tokio::test]
    async fn mcp_tool_execute_returns_enriched_result() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        let registry = mgr.registry();
        let tool = registry.get("mcp__srv__echo").unwrap();
        let ctx = awaken_contract::contract::tool::ToolCallContext::test_default();

        let output = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(output.result.is_success());
        // MCP metadata is in result.metadata, not result.data
        assert_eq!(output.result.metadata["mcp.server"], "srv");
        assert_eq!(output.result.metadata["mcp.tool"], "echo");
        assert!(output.result.data.get("_mcp").is_none());
    }

    #[tokio::test]
    async fn stale_tool_handle_uses_live_transport_after_runtime_swap() {
        let initial = Arc::new(RecordingTransport::new("echo", "old")) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![(cfg("srv"), initial)])
            .await
            .unwrap();
        let tool = mgr.registry().get("mcp__srv__echo").unwrap();

        let replacement = Arc::new(RecordingTransport::new("echo", "new"));
        let replacement_calls = replacement.calls.clone();
        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].runtime = Some(McpServerRuntime {
                transport_type: TransportTypeId::Stdio,
                transport: replacement.clone() as Arc<dyn McpToolTransport>,
                capabilities: None,
            });
            servers[0].lifecycle = McpServerLifecycle::Connected;
        }

        let ctx = awaken_contract::contract::tool::ToolCallContext::test_default();
        let output = tool.execute(json!({}), &ctx).await.unwrap();

        assert_eq!(output.result.data, Value::String("new".to_string()));
        assert_eq!(replacement_calls.lock().unwrap().as_slice(), ["echo"]);
    }

    #[tokio::test]
    async fn tool_call_transport_failure_updates_health_and_reconnect_state() {
        let transport =
            Arc::new(FailingCallTransport::connection_closed("echo")) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![(cfg("srv"), transport)])
            .await
            .unwrap();
        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[0].meta.config.args.clear();
            servers[0].meta.config.timeout_secs = 1;
        }

        let tool = mgr.registry().get("mcp__srv__echo").unwrap();
        let ctx = awaken_contract::contract::tool::ToolCallContext::test_default();

        for _ in 0..3 {
            let err = tool.execute(json!({}), &ctx).await.unwrap_err();
            assert!(matches!(err, ToolError::ExecutionFailed(_)));
        }

        let health = mgr.server_health("srv").unwrap();
        assert!(health.consecutive_failures >= FAILURE_THRESHOLD);
        let servers = read_lock(&mgr.state.servers);
        assert_eq!(servers[0].lifecycle, McpServerLifecycle::Disconnected);
        assert_eq!(servers[0].reconnect_attempts, 1);
    }

    // ── Helper function tests ──

    #[test]
    fn validate_server_name_rejects_empty() {
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("   ").is_err());
    }

    #[test]
    fn validate_server_name_accepts_valid() {
        assert!(validate_server_name("my-server").is_ok());
        assert!(validate_server_name("a").is_ok());
    }

    #[test]
    fn server_supports_prompts_none_capabilities() {
        assert!(server_supports_prompts(None));
    }

    #[test]
    fn server_supports_resources_none_capabilities() {
        assert!(server_supports_resources(None));
    }

    #[test]
    fn is_unsupported_transport_message_detects_pattern() {
        assert!(is_unsupported_transport_message(
            "list_prompts not supported by this server",
            "list_prompts"
        ));
        assert!(!is_unsupported_transport_message(
            "some other error",
            "list_prompts"
        ));
    }

    #[test]
    fn map_mcp_error_unknown_tool() {
        let err = map_mcp_error(McpTransportError::UnknownTool("t".to_string()));
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[test]
    fn map_mcp_error_timeout() {
        let err = map_mcp_error(McpTransportError::Timeout("30s".to_string()));
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("timeout")));
    }

    #[test]
    fn map_mcp_error_other() {
        let err = map_mcp_error(McpTransportError::TransportError("fail".to_string()));
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn mcp_tool_execute_populates_metadata_server_and_tool() {
        let mgr =
            make_manager_with(vec![("my-srv", vec![MockTransport::tool_def("my-tool")])]).await;
        let registry = mgr.registry();
        let tool_id = registry
            .ids()
            .into_iter()
            .find(|id| id.contains("my_tool"))
            .expect("my-tool");
        let tool = registry.get(&tool_id).unwrap();
        let ctx = awaken_contract::contract::tool::ToolCallContext::test_default();

        let output = tool.execute(json!({}), &ctx).await.unwrap();
        assert_eq!(output.result.metadata["mcp.server"], "my-srv");
        assert_eq!(output.result.metadata["mcp.tool"], "my-tool");
    }

    #[tokio::test]
    async fn mcp_tool_execute_populates_result_content_in_metadata() {
        // MockTransport.call_tool always returns a Text content item
        let mgr = make_manager_with(vec![("s", vec![MockTransport::tool_def("t")])]).await;
        let registry = mgr.registry();
        let tool_id = registry
            .ids()
            .into_iter()
            .find(|id| id.contains("__t"))
            .expect("tool t");
        let tool = registry.get(&tool_id).unwrap();
        let ctx = awaken_contract::contract::tool::ToolCallContext::test_default();

        let output = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(output.result.metadata.contains_key(MCP_META_RESULT_CONTENT));
        assert!(output.result.data.get("_mcp").is_none());
    }

    // ── Progress emission ──

    #[test]
    fn progress_emit_gate_default_state() {
        let gate = ProgressEmitGate::default();
        assert!(gate.last_emit_at.is_none());
        assert!(gate.last_progress.is_none());
        assert!(gate.last_message.is_none());
    }

    #[test]
    fn mcp_refresh_health_default() {
        let health = McpRefreshHealth::default();
        assert!(health.last_attempt_at.is_none());
        assert!(health.last_success_at.is_none());
        assert!(health.last_error.is_none());
        assert_eq!(health.consecutive_failures, 0);
    }
}
