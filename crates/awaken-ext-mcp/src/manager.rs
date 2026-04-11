//! MCP tool registry manager: server lifecycle, tool discovery, periodic refresh.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use awaken_contract::PeriodicRefresher;
use awaken_contract::contract::progress::ProgressStatus;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use futures::future::join_all;
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct McpHealthBudget {
    last_attempt_at: Option<SystemTime>,
    last_success_at: Option<SystemTime>,
    last_error: Option<String>,
    consecutive_failures: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpRuntimeOpKind {
    Discovery,
    Rpc,
}

#[derive(Debug)]
enum RuntimeOperationError {
    Mcp(McpError),
    Transport(McpTransportError),
}

impl From<RuntimeOperationError> for McpError {
    fn from(err: RuntimeOperationError) -> Self {
        match err {
            RuntimeOperationError::Mcp(err) => err,
            RuntimeOperationError::Transport(err) => err.into(),
        }
    }
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
        let Some(state) = self.state.upgrade() else {
            return Err(ToolError::ExecutionFailed(
                McpError::RuntimeUnavailable.to_string(),
            ));
        };
        let tool_name = self.tool_name.clone();
        let res = match with_runtime_lease(
            &state,
            &self.server_name,
            McpRuntimeOpKind::Rpc,
            |_| Ok(()),
            move |runtime| async move {
                let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
                let mut call = Box::pin(runtime.transport.call_tool(
                    &tool_name,
                    args,
                    Some(progress_tx),
                ));
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

                while let Ok(update) = progress_rx.try_recv() {
                    emit_mcp_progress(ctx, &mut gate, update).await;
                }

                res
            },
        )
        .await
        {
            Ok(result) => result,
            Err(RuntimeOperationError::Transport(err)) => return Err(map_mcp_error(err)),
            Err(RuntimeOperationError::Mcp(err)) => {
                return Err(ToolError::ExecutionFailed(err.to_string()));
            }
        };

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
            && let Some(content) = fetch_ui_resource(&state, &self.server_name, uri).await
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
    state: &Arc<McpRegistryState>,
    server_name: &str,
    uri: &str,
) -> Option<UiResourceContent> {
    let value = with_runtime_lease(
        state,
        server_name,
        McpRuntimeOpKind::Rpc,
        |_| Ok(()),
        |runtime| async move { runtime.transport.read_resource(uri).await },
    )
    .await
    .ok()?;
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

#[derive(Clone)]
struct McpRuntimeLease {
    server_name: String,
    generation: u64,
    transport_type: TransportTypeId,
    transport: Arc<dyn McpToolTransport>,
    capabilities: Option<ServerCapabilities>,
}

fn runtime_lease(slot: &McpServerSlot) -> Result<McpRuntimeLease, McpError> {
    let runtime = server_runtime(slot)?;
    Ok(McpRuntimeLease {
        server_name: slot.meta.name.clone(),
        generation: runtime.generation,
        transport_type: runtime.transport_type,
        transport: runtime.transport.clone(),
        capabilities: runtime.capabilities.clone(),
    })
}

fn resolve_live_runtime(
    state: &Weak<McpRegistryState>,
    server_name: &str,
) -> Result<McpRuntimeLease, McpError> {
    let Some(state) = state.upgrade() else {
        return Err(McpError::RuntimeUnavailable);
    };
    let servers = read_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    runtime_lease(&servers[index])
}

fn should_track_transport_failure(err: &McpTransportError) -> bool {
    matches!(
        err,
        McpTransportError::ConnectionClosed
            | McpTransportError::Timeout(_)
            | McpTransportError::ProtocolError(_)
    ) || matches!(
        err,
        McpTransportError::TransportError(message)
            if !message.contains("not supported")
    )
}

// ── Server runtime ──

#[derive(Clone)]
struct McpServerRuntime {
    generation: u64,
    transport_type: TransportTypeId,
    transport: Arc<dyn McpToolTransport>,
    capabilities: Option<ServerCapabilities>,
}

#[derive(Clone, Default)]
struct McpPublishedCatalog {
    _tool_defs: Vec<McpToolDefinition>,
    tools: PublishedTools,
}

#[derive(Clone)]
struct McpPublishedSnapshot {
    generation: u64,
    catalog: McpPublishedCatalog,
}

#[derive(Clone)]
struct McpServerMetadata {
    name: String,
    config: McpServerConnectionConfig,
}

type PublishedTools = HashMap<String, Arc<dyn Tool>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpServerLifecycle {
    Connecting,
    Disabled,
    Connected,
    Reconnecting,
    Disabling,
    Disconnected,
    PermanentlyFailed,
}

#[derive(Clone)]
struct McpServerSlot {
    meta: McpServerMetadata,
    lifecycle: McpServerLifecycle,
    runtime: Option<McpServerRuntime>,
    health: McpRefreshHealth,
    discovery_health: McpHealthBudget,
    rpc_health: McpHealthBudget,
    reconnect_attempts: u32,
    next_generation: u64,
    published_snapshot: Option<McpPublishedSnapshot>,
    lifecycle_lock: Arc<AsyncMutex<()>>,
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

fn unsupported_capability(server_name: impl Into<String>, capability: &'static str) -> McpError {
    McpError::UnsupportedCapability {
        server_name: server_name.into(),
        capability,
    }
}

fn transport_operation_not_supported(err: &McpTransportError, operation: &str) -> bool {
    matches!(
        err,
        McpTransportError::TransportError(message)
            if is_unsupported_transport_message(message, operation)
    )
}

fn transient_lifecycle_mcp_error(err: &McpError) -> bool {
    matches!(
        err,
        McpError::UnknownServer(_)
            | McpError::ServerDisabled(_)
            | McpError::ServerPermanentlyFailed(_)
            | McpError::RuntimeUnavailable
    ) || matches!(
        err,
        McpError::Transport(message) if message.contains("connection closed")
    )
}

fn aggregate_list_should_skip(err: &RuntimeOperationError, operation: &str) -> bool {
    match err {
        RuntimeOperationError::Mcp(McpError::UnsupportedCapability { .. }) => true,
        RuntimeOperationError::Mcp(err) if transient_lifecycle_mcp_error(err) => true,
        RuntimeOperationError::Transport(McpTransportError::ConnectionClosed) => true,
        RuntimeOperationError::Transport(err) => transport_operation_not_supported(err, operation),
        RuntimeOperationError::Mcp(_) => false,
    }
}

fn map_capability_operation_error(
    server_name: &str,
    capability: &'static str,
    operation: &str,
    err: RuntimeOperationError,
) -> McpError {
    match err {
        RuntimeOperationError::Transport(err)
            if transport_operation_not_supported(&err, operation) =>
        {
            unsupported_capability(server_name, capability)
        }
        other => other.into(),
    }
}

fn server_supports_prompts(capabilities: Option<&ServerCapabilities>) -> bool {
    capabilities.is_none_or(|capabilities| capabilities.prompts.is_some())
}

fn server_supports_resources(capabilities: Option<&ServerCapabilities>) -> bool {
    capabilities.is_none_or(|capabilities| capabilities.resources.is_some())
}

fn require_prompts(runtime: &McpRuntimeLease) -> Result<(), McpError> {
    if server_supports_prompts(runtime.capabilities.as_ref()) {
        return Ok(());
    }
    Err(unsupported_capability(&runtime.server_name, "prompts"))
}

fn require_resources(runtime: &McpRuntimeLease) -> Result<(), McpError> {
    if server_supports_resources(runtime.capabilities.as_ref()) {
        return Ok(());
    }
    Err(unsupported_capability(&runtime.server_name, "resources"))
}

fn discover_tools(servers: &[McpServerSlot]) -> Result<HashMap<String, Arc<dyn Tool>>, McpError> {
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    for slot in servers {
        if matches!(
            slot.lifecycle,
            McpServerLifecycle::Disabled
                | McpServerLifecycle::Disabling
                | McpServerLifecycle::PermanentlyFailed
        ) {
            continue;
        }

        if let Some(snapshot) = &slot.published_snapshot {
            tracing::trace!(
                server = %slot.meta.name,
                generation = snapshot.generation,
                tool_count = snapshot.catalog.tools.len(),
                "including published snapshot in discovery"
            );
            for (tool_id, tool) in &snapshot.catalog.tools {
                if tools.contains_key(tool_id) {
                    return Err(McpError::ToolIdConflict(tool_id.clone()));
                }
                tools.insert(tool_id.clone(), tool.clone());
            }
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

async fn close_runtime(runtime: McpServerRuntime) -> Result<(), McpError> {
    runtime.transport.close().await?;
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

fn server_lifecycle_lock(
    state: &McpRegistryState,
    server_name: &str,
) -> Result<Arc<AsyncMutex<()>>, McpError> {
    let servers = read_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    Ok(servers[index].lifecycle_lock.clone())
}

fn active_server_names(state: &McpRegistryState) -> Vec<String> {
    let servers = read_lock(&state.servers);
    servers
        .iter()
        .filter(|slot| server_is_active(slot))
        .map(|slot| slot.meta.name.clone())
        .collect()
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

fn latest_time(left: Option<SystemTime>, right: Option<SystemTime>) -> Option<SystemTime> {
    match (left, right) {
        (Some(left), Some(right)) => {
            if right.duration_since(left).is_ok() {
                Some(right)
            } else {
                Some(left)
            }
        }
        (Some(time), None) | (None, Some(time)) => Some(time),
        (None, None) => None,
    }
}

fn health_budget_mut(slot: &mut McpServerSlot, op_kind: McpRuntimeOpKind) -> &mut McpHealthBudget {
    match op_kind {
        McpRuntimeOpKind::Discovery => &mut slot.discovery_health,
        McpRuntimeOpKind::Rpc => &mut slot.rpc_health,
    }
}

fn health_budget(slot: &McpServerSlot, op_kind: McpRuntimeOpKind) -> &McpHealthBudget {
    match op_kind {
        McpRuntimeOpKind::Discovery => &slot.discovery_health,
        McpRuntimeOpKind::Rpc => &slot.rpc_health,
    }
}

fn budget_error(budget: &McpHealthBudget) -> Option<(Option<SystemTime>, String)> {
    if budget.consecutive_failures == 0 {
        return None;
    }
    budget
        .last_error
        .clone()
        .map(|error| (budget.last_attempt_at, error))
}

fn latest_budget_error(left: &McpHealthBudget, right: &McpHealthBudget) -> Option<String> {
    match (budget_error(left), budget_error(right)) {
        (Some((left_time, left_error)), Some((right_time, right_error))) => {
            if latest_time(left_time, right_time) == right_time {
                Some(right_error)
            } else {
                Some(left_error)
            }
        }
        (Some((_, error)), None) | (None, Some((_, error))) => Some(error),
        (None, None) => None,
    }
}

fn sync_server_health(slot: &mut McpServerSlot) {
    let reconnecting = slot.health.reconnecting;
    let permanently_failed = slot.health.permanently_failed;
    slot.health = McpRefreshHealth {
        last_attempt_at: latest_time(
            slot.discovery_health.last_attempt_at,
            slot.rpc_health.last_attempt_at,
        ),
        last_success_at: latest_time(
            slot.discovery_health.last_success_at,
            slot.rpc_health.last_success_at,
        ),
        last_error: latest_budget_error(&slot.discovery_health, &slot.rpc_health),
        consecutive_failures: slot
            .discovery_health
            .consecutive_failures
            .max(slot.rpc_health.consecutive_failures),
        reconnecting,
        permanently_failed,
    };
}

fn record_budget_success(budget: &mut McpHealthBudget, attempted_at: SystemTime) {
    budget.last_attempt_at = Some(attempted_at);
    budget.last_success_at = Some(attempted_at);
    budget.last_error = None;
    budget.consecutive_failures = 0;
}

fn record_budget_failure(budget: &mut McpHealthBudget, attempted_at: SystemTime, err: &McpError) {
    budget.last_attempt_at = Some(attempted_at);
    budget.last_error = Some(err.to_string());
    budget.consecutive_failures = budget.consecutive_failures.saturating_add(1);
}

fn reset_all_server_health_on_success(slot: &mut McpServerSlot, attempted_at: SystemTime) {
    record_budget_success(&mut slot.discovery_health, attempted_at);
    record_budget_success(&mut slot.rpc_health, attempted_at);
    slot.health.reconnecting = false;
    slot.health.permanently_failed = false;
    sync_server_health(slot);
    slot.reconnect_attempts = 0;
    slot.lifecycle = McpServerLifecycle::Connected;
}

fn mark_server_success(
    slot: &mut McpServerSlot,
    op_kind: McpRuntimeOpKind,
    attempted_at: SystemTime,
) {
    record_budget_success(health_budget_mut(slot, op_kind), attempted_at);
    slot.health.reconnecting = false;
    slot.health.permanently_failed = false;
    if op_kind == McpRuntimeOpKind::Discovery {
        slot.reconnect_attempts = 0;
        slot.lifecycle = McpServerLifecycle::Connected;
    }
    sync_server_health(slot);
}

fn mark_server_failure(
    slot: &mut McpServerSlot,
    op_kind: McpRuntimeOpKind,
    attempted_at: SystemTime,
    err: &McpError,
) {
    record_budget_failure(health_budget_mut(slot, op_kind), attempted_at, err);
    slot.health.reconnecting = false;
    slot.health.permanently_failed = slot.lifecycle == McpServerLifecycle::PermanentlyFailed;
    sync_server_health(slot);
}

fn server_failure_count(slot: &McpServerSlot, op_kind: McpRuntimeOpKind) -> u64 {
    health_budget(slot, op_kind).consecutive_failures
}

fn clear_health_budgets(slot: &mut McpServerSlot) {
    slot.discovery_health = McpHealthBudget::default();
    slot.rpc_health = McpHealthBudget::default();
    sync_server_health(slot);
}

fn mark_server_permanent_failure(slot: &mut McpServerSlot) {
    slot.lifecycle = McpServerLifecycle::PermanentlyFailed;
    slot.runtime = None;
    slot.published_snapshot = None;
    slot.health.permanently_failed = true;
    slot.health.reconnecting = false;
}

fn finish_reconnect_failure(slot: &mut McpServerSlot, err: &McpError) {
    slot.health.last_error = Some(err.to_string());
    slot.reconnect_attempts = slot.reconnect_attempts.saturating_add(1);
    slot.health.reconnecting = false;

    if slot.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
        mark_server_permanent_failure(slot);
    } else {
        slot.lifecycle = McpServerLifecycle::Disconnected;
    }
}

/// Like [`finish_reconnect_failure`] but does **not** increment
/// `reconnect_attempts`.  Used when the old transport's `close()` fails
/// before a new connection is even attempted — burning a reconnect budget
/// slot for a teardown error would be unfair.
fn finish_reconnect_close_failure(slot: &mut McpServerSlot, err: &McpError) {
    slot.health.last_error = Some(err.to_string());
    slot.health.reconnecting = false;
    slot.lifecycle = McpServerLifecycle::Disconnected;
}

fn reserve_generation(slot: &mut McpServerSlot) -> u64 {
    let generation = slot.next_generation;
    slot.next_generation = slot.next_generation.saturating_add(1);
    generation
}

async fn connect_runtime(
    config: &McpServerConnectionConfig,
    sampling_handler: Option<Arc<dyn SamplingHandler>>,
    generation: u64,
) -> Result<McpServerRuntime, McpError> {
    let transport = connect_transport(config, sampling_handler).await?;
    let transport_type = transport.transport_type();
    let capabilities = transport.server_capabilities().await?;
    Ok(McpServerRuntime {
        generation,
        transport_type,
        transport,
        capabilities,
    })
}

async fn discover_catalog_from_lease(
    state: Weak<McpRegistryState>,
    runtime: &McpRuntimeLease,
) -> Result<McpPublishedCatalog, McpError> {
    let mut tool_defs = runtime.transport.list_tools().await?;
    tool_defs.sort_by(|a, b| a.name.cmp(&b.name));
    let tools = build_published_tools(
        state,
        &runtime.server_name,
        &tool_defs,
        runtime.transport_type,
    )?;
    Ok(McpPublishedCatalog {
        _tool_defs: tool_defs,
        tools,
    })
}

fn apply_catalog_if_current(
    state: &McpRegistryState,
    server_name: &str,
    generation: u64,
    attempted_at: SystemTime,
    catalog: McpPublishedCatalog,
) -> Result<bool, McpError> {
    let mut servers = write_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    let slot = &mut servers[index];
    let Some(runtime) = slot.runtime.as_ref() else {
        return Ok(false);
    };
    if runtime.generation != generation {
        return Ok(false);
    }
    slot.published_snapshot = Some(McpPublishedSnapshot {
        generation,
        catalog,
    });
    mark_server_success(slot, McpRuntimeOpKind::Discovery, attempted_at);
    Ok(true)
}

fn mark_runtime_operation_result_if_current(
    state: &McpRegistryState,
    server_name: &str,
    generation: u64,
    op_kind: McpRuntimeOpKind,
    attempted_at: SystemTime,
    result: Result<(), &McpError>,
) -> bool {
    let mut servers = write_lock(&state.servers);
    let Ok(index) = find_server_index(&servers, server_name) else {
        return false;
    };
    let slot = &mut servers[index];
    let Some(runtime) = slot.runtime.as_ref() else {
        return false;
    };
    if runtime.generation != generation || slot.lifecycle != McpServerLifecycle::Connected {
        return false;
    }
    match result {
        Ok(()) => mark_server_success(slot, op_kind, attempted_at),
        Err(err) => mark_server_failure(slot, op_kind, attempted_at, err),
    }
    true
}

fn begin_reconnect_transition(
    state: &McpRegistryState,
    server_name: &str,
) -> Result<
    (
        McpServerConnectionConfig,
        u64,
        u32,
        Option<McpServerRuntime>,
    ),
    McpError,
> {
    let mut servers = write_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    let slot = &mut servers[index];

    if slot.lifecycle == McpServerLifecycle::Disabled {
        return Err(McpError::ServerDisabled(server_name.to_string()));
    }
    if slot.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
        mark_server_permanent_failure(slot);
        return Err(McpError::ServerPermanentlyFailed(server_name.to_string()));
    }

    let detached_runtime = slot.runtime.take();
    slot.lifecycle = if detached_runtime.is_some() {
        McpServerLifecycle::Reconnecting
    } else {
        McpServerLifecycle::Connecting
    };
    slot.health.reconnecting = true;
    slot.health.permanently_failed = false;

    Ok((
        slot.meta.config.clone(),
        reserve_generation(slot),
        slot.reconnect_attempts,
        detached_runtime,
    ))
}

fn finish_reconnect_success(
    state: &McpRegistryState,
    server_name: &str,
    runtime: McpServerRuntime,
    catalog: McpPublishedCatalog,
    attempted_at: SystemTime,
) -> Result<(), McpError> {
    let mut servers = write_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    let slot = &mut servers[index];
    let generation = runtime.generation;
    slot.runtime = Some(runtime);
    slot.published_snapshot = Some(McpPublishedSnapshot {
        generation,
        catalog,
    });
    reset_all_server_health_on_success(slot, attempted_at);
    Ok(())
}

fn finish_reconnect_error(
    state: &McpRegistryState,
    server_name: &str,
    err: &McpError,
) -> Result<(), McpError> {
    let mut servers = write_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    finish_reconnect_failure(&mut servers[index], err);
    Ok(())
}

fn finish_reconnect_close_error(
    state: &McpRegistryState,
    server_name: &str,
    err: &McpError,
) -> Result<(), McpError> {
    let mut servers = write_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    finish_reconnect_close_failure(&mut servers[index], err);
    Ok(())
}

fn begin_disable_transition(
    state: &McpRegistryState,
    server_name: &str,
) -> Result<Option<McpServerRuntime>, McpError> {
    let mut servers = write_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    let slot = &mut servers[index];
    if slot.lifecycle == McpServerLifecycle::Disabled {
        slot.health.reconnecting = false;
        return Ok(None);
    }

    slot.lifecycle = McpServerLifecycle::Disabling;
    slot.health.reconnecting = false;
    Ok(slot.runtime.take())
}

fn finish_disable_transition(
    state: &McpRegistryState,
    server_name: &str,
    close_error: Option<&McpError>,
) -> Result<(), McpError> {
    let mut servers = write_lock(&state.servers);
    let index = find_server_index(&servers, server_name)?;
    let slot = &mut servers[index];
    slot.lifecycle = McpServerLifecycle::Disabled;
    slot.runtime = None;
    slot.published_snapshot = None;
    slot.health.reconnecting = false;
    slot.health.permanently_failed = false;
    clear_health_budgets(slot);
    slot.health.last_attempt_at = Some(SystemTime::now());
    slot.health.last_error = close_error.map(ToString::to_string);
    Ok(())
}

async fn reconnect_server_locked(
    state: &Arc<McpRegistryState>,
    server_name: &str,
) -> Result<(), McpError> {
    let sampling_handler = state.sampling_handler.clone();
    let (config, generation, attempt, detached_runtime) =
        begin_reconnect_transition(state.as_ref(), server_name)?;

    if let Some(runtime) = detached_runtime
        && let Err(err) = close_runtime(runtime).await
    {
        finish_reconnect_close_error(state.as_ref(), server_name, &err)?;
        return Err(err);
    }

    tokio::time::sleep(reconnect_backoff(attempt)).await;

    let runtime = match connect_runtime(&config, sampling_handler, generation).await {
        Ok(runtime) => runtime,
        Err(err) => {
            finish_reconnect_error(state.as_ref(), server_name, &err)?;
            return Err(err);
        }
    };

    let lease = McpRuntimeLease {
        server_name: server_name.to_string(),
        generation,
        transport_type: runtime.transport_type,
        transport: runtime.transport.clone(),
        capabilities: runtime.capabilities.clone(),
    };
    let catalog = match discover_catalog_from_lease(Arc::downgrade(state), &lease).await {
        Ok(catalog) => catalog,
        Err(err) => {
            finish_reconnect_error(state.as_ref(), server_name, &err)?;
            return Err(err);
        }
    };

    finish_reconnect_success(
        state.as_ref(),
        server_name,
        runtime,
        catalog,
        SystemTime::now(),
    )?;
    Ok(())
}

async fn reconnect_server(
    state: &Arc<McpRegistryState>,
    server_name: &str,
) -> Result<(), McpError> {
    let lifecycle_lock = server_lifecycle_lock(state.as_ref(), server_name)?;
    let _lifecycle_guard = lifecycle_lock.lock().await;
    reconnect_server_locked(state, server_name).await
}

async fn disable_server(state: &Arc<McpRegistryState>, server_name: &str) -> Result<(), McpError> {
    let detached_runtime = begin_disable_transition(state.as_ref(), server_name)?;
    let _ = rebuild_snapshot(state.as_ref()).await;

    let close_error = match detached_runtime {
        Some(runtime) => close_runtime(runtime).await.err(),
        None => None,
    };
    finish_disable_transition(state.as_ref(), server_name, close_error.as_ref())?;

    if let Some(err) = close_error {
        return Err(err);
    }
    Ok(())
}

async fn record_runtime_operation_success(
    state: &Weak<McpRegistryState>,
    server_name: &str,
    generation: u64,
    op_kind: McpRuntimeOpKind,
) {
    let Some(state) = state.upgrade() else {
        return;
    };

    let _ = mark_runtime_operation_result_if_current(
        state.as_ref(),
        server_name,
        generation,
        op_kind,
        SystemTime::now(),
        Ok(()),
    );
}

async fn record_runtime_operation_failure(
    state: &Weak<McpRegistryState>,
    server_name: &str,
    generation: u64,
    op_kind: McpRuntimeOpKind,
    err: &McpTransportError,
) {
    if !should_track_transport_failure(err) {
        return;
    }

    let Some(state) = state.upgrade() else {
        return;
    };

    let Ok(lifecycle_lock) = server_lifecycle_lock(state.as_ref(), server_name) else {
        return;
    };
    let _lifecycle_guard = lifecycle_lock.lock().await;
    let err = McpError::Transport(err.to_string());
    if !mark_runtime_operation_result_if_current(
        state.as_ref(),
        server_name,
        generation,
        op_kind,
        SystemTime::now(),
        Err(&err),
    ) {
        return;
    }

    let should_reconnect = {
        let servers = read_lock(&state.servers);
        let Ok(index) = find_server_index(&servers, server_name) else {
            return;
        };
        server_failure_count(&servers[index], op_kind) >= FAILURE_THRESHOLD
    };

    if should_reconnect {
        if let Err(reconnect_err) = reconnect_server_locked(&state, server_name).await {
            tracing::warn!(
                error = %reconnect_err,
                server = %server_name,
                "MCP runtime operation reconnect failed"
            );
        }
        let _ = rebuild_snapshot(state.as_ref()).await;
    }
}

async fn with_runtime_lease<T, Fut, F, P>(
    state: &Arc<McpRegistryState>,
    server_name: &str,
    op_kind: McpRuntimeOpKind,
    preflight: P,
    operation: F,
) -> Result<T, RuntimeOperationError>
where
    F: FnOnce(McpRuntimeLease) -> Fut,
    Fut: Future<Output = Result<T, McpTransportError>>,
    P: FnOnce(&McpRuntimeLease) -> Result<(), McpError>,
{
    let runtime = {
        let servers = read_lock(&state.servers);
        let index = find_server_index(&servers, server_name).map_err(RuntimeOperationError::Mcp)?;
        runtime_lease(&servers[index]).map_err(RuntimeOperationError::Mcp)?
    };
    preflight(&runtime).map_err(RuntimeOperationError::Mcp)?;

    let generation = runtime.generation;
    match operation(runtime).await {
        Ok(value) => {
            record_runtime_operation_success(
                &Arc::downgrade(state),
                server_name,
                generation,
                op_kind,
            )
            .await;
            Ok(value)
        }
        Err(err) => {
            record_runtime_operation_failure(
                &Arc::downgrade(state),
                server_name,
                generation,
                op_kind,
                &err,
            )
            .await;
            Err(RuntimeOperationError::Transport(err))
        }
    }
}

async fn refresh_server(state: Arc<McpRegistryState>, server_name: String) -> Result<(), McpError> {
    let lifecycle_lock = server_lifecycle_lock(state.as_ref(), &server_name)?;
    let Ok(_lifecycle_guard) = lifecycle_lock.try_lock() else {
        tracing::trace!(
            server = %server_name,
            "skipping MCP refresh because server lifecycle is already active"
        );
        return Ok(());
    };

    let slot_state = {
        let servers = read_lock(&state.servers);
        let index = find_server_index(&servers, &server_name)?;
        servers[index].lifecycle
    };

    if matches!(
        slot_state,
        McpServerLifecycle::Disabled
            | McpServerLifecycle::Disabling
            | McpServerLifecycle::Connecting
            | McpServerLifecycle::Reconnecting
            | McpServerLifecycle::PermanentlyFailed
    ) {
        return Ok(());
    }

    if slot_state == McpServerLifecycle::Disconnected {
        if let Err(reconnect_err) = reconnect_server_locked(&state, &server_name).await {
            tracing::warn!(
                error = %reconnect_err,
                server = %server_name,
                "MCP server reconnect failed"
            );
        }
        return Ok(());
    }

    let attempted_at = SystemTime::now();
    let runtime = match resolve_live_runtime(&Arc::downgrade(&state), &server_name) {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::warn!(error = %err, server = %server_name, "MCP server refresh failed");
            return Ok(());
        }
    };

    match discover_catalog_from_lease(Arc::downgrade(&state), &runtime).await {
        Ok(catalog) => {
            let _ = apply_catalog_if_current(
                state.as_ref(),
                &server_name,
                runtime.generation,
                attempted_at,
                catalog,
            )?;
        }
        Err(err) => {
            tracing::warn!(error = %err, server = %server_name, "MCP server refresh failed");
            let _ = mark_runtime_operation_result_if_current(
                state.as_ref(),
                &server_name,
                runtime.generation,
                McpRuntimeOpKind::Discovery,
                attempted_at,
                Err(&err),
            );

            let should_reconnect = {
                let servers = read_lock(&state.servers);
                let index = find_server_index(&servers, &server_name)?;
                server_failure_count(&servers[index], McpRuntimeOpKind::Discovery)
                    >= FAILURE_THRESHOLD
            };

            if should_reconnect
                && let Err(reconnect_err) = reconnect_server_locked(&state, &server_name).await
            {
                tracing::warn!(
                    error = %reconnect_err,
                    server = %server_name,
                    "MCP server reconnect failed"
                );
            }
        }
    }

    Ok(())
}

async fn refresh_state(state: Arc<McpRegistryState>) -> Result<u64, McpError> {
    let server_names: Vec<String> = {
        let servers = read_lock(&state.servers);
        servers.iter().map(|slot| slot.meta.name.clone()).collect()
    };

    let results = join_all(
        server_names
            .into_iter()
            .map(|server_name| refresh_server(state.clone(), server_name)),
    )
    .await;
    for result in results {
        result?;
    }

    rebuild_snapshot(state.as_ref()).await
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
        });

        let server_names: Vec<String> = {
            let servers = read_lock(&state.servers);
            servers.iter().map(|slot| slot.meta.name.clone()).collect()
        };
        for server_name in server_names {
            let attempted_at = SystemTime::now();
            let runtime = resolve_live_runtime(&Arc::downgrade(&state), &server_name)?;
            let catalog = discover_catalog_from_lease(Arc::downgrade(&state), &runtime).await?;
            let _ = apply_catalog_if_current(
                state.as_ref(),
                &server_name,
                runtime.generation,
                attempted_at,
                catalog,
            )?;
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
                    generation: 1,
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
                discovery_health: McpHealthBudget {
                    last_attempt_at: Some(connected_at),
                    last_success_at: Some(connected_at),
                    last_error: None,
                    consecutive_failures: 0,
                },
                rpc_health: McpHealthBudget::default(),
                reconnect_attempts: 0,
                next_generation: 2,
                published_snapshot: None,
                lifecycle_lock: Arc::new(AsyncMutex::new(())),
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
        for server_name in active_server_names(self.state.as_ref()) {
            let (transport_type, mut defs) = match with_runtime_lease(
                &self.state,
                &server_name,
                McpRuntimeOpKind::Rpc,
                require_prompts,
                |runtime| async move {
                    let transport_type = runtime.transport_type;
                    let defs = runtime.transport.list_prompts().await?;
                    Ok((transport_type, defs))
                },
            )
            .await
            {
                Ok(defs) => defs,
                Err(err) if aggregate_list_should_skip(&err, "list_prompts") => {
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
        let prompt_name = prompt_name.to_string();
        with_runtime_lease(
            &self.state,
            server_name,
            McpRuntimeOpKind::Rpc,
            require_prompts,
            move |runtime| async move {
                runtime
                    .transport
                    .get_prompt(&prompt_name, arguments)
                    .await
            },
        )
        .await
        .map_err(|err| map_capability_operation_error(server_name, "prompts", "get_prompt", err))
    }

    pub async fn list_resources(&self) -> Result<Vec<McpResourceEntry>, McpError> {
        let mut resources = Vec::new();
        for server_name in active_server_names(self.state.as_ref()) {
            let (transport_type, mut defs) = match with_runtime_lease(
                &self.state,
                &server_name,
                McpRuntimeOpKind::Rpc,
                require_resources,
                |runtime| async move {
                    let transport_type = runtime.transport_type;
                    let defs = runtime.transport.list_resources().await?;
                    Ok((transport_type, defs))
                },
            )
            .await
            {
                Ok(defs) => defs,
                Err(err) if aggregate_list_should_skip(&err, "list_resources") => {
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
        with_runtime_lease(
            &self.state,
            server_name,
            McpRuntimeOpKind::Rpc,
            require_resources,
            |runtime| async move { runtime.transport.read_resource(uri).await },
        )
        .await
        .map_err(|err| {
            map_capability_operation_error(server_name, "resources", "read_resource", err)
        })
    }
    pub async fn reconnect(&self, server_name: &str) -> Result<(), McpError> {
        reconnect_server(&self.state, server_name).await?;
        rebuild_snapshot(self.state.as_ref()).await?;
        Ok(())
    }

    pub async fn toggle(&self, server_name: &str, enabled: bool) -> Result<(), McpError> {
        let lifecycle_lock = server_lifecycle_lock(self.state.as_ref(), server_name)?;
        let _lifecycle_guard = lifecycle_lock.lock().await;
        if !enabled {
            return disable_server(&self.state, server_name).await;
        }

        {
            let mut servers = write_lock(&self.state.servers);
            let index = find_server_index(&servers, server_name)?;
            let slot = &mut servers[index];
            slot.lifecycle = McpServerLifecycle::Disconnected;
            slot.reconnect_attempts = 0;
            slot.health.reconnecting = false;
            slot.health.permanently_failed = false;
            clear_health_budgets(slot);
        }

        reconnect_server_locked(&self.state, server_name).await?;
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

    #[derive(Debug)]
    struct BlockingCloseTransport {
        tools: Vec<McpToolDefinition>,
        entered: Arc<Semaphore>,
        release: Arc<Notify>,
    }

    impl BlockingCloseTransport {
        fn new(tool_name: &str) -> Self {
            Self {
                tools: vec![MockTransport::tool_def(tool_name)],
                entered: Arc::new(Semaphore::new(0)),
                release: Arc::new(Notify::new()),
            }
        }
    }

    #[derive(Debug)]
    struct FailingCatalogRpcTransport {
        tools: Vec<McpToolDefinition>,
    }

    impl FailingCatalogRpcTransport {
        fn new(tool_name: &str) -> Self {
            Self {
                tools: vec![MockTransport::tool_def(tool_name)],
            }
        }
    }

    #[derive(Debug)]
    struct StaticCatalogTransport {
        prompts: Vec<McpPromptDefinition>,
        resources: Vec<McpResourceDefinition>,
    }

    impl StaticCatalogTransport {
        fn with_catalog(
            prompts: Vec<McpPromptDefinition>,
            resources: Vec<McpResourceDefinition>,
        ) -> Self {
            Self { prompts, resources }
        }
    }

    #[derive(Debug)]
    struct BlockingCatalogLifecycleTransport {
        prompts: Vec<McpPromptDefinition>,
        resources: Vec<McpResourceDefinition>,
        entered: Arc<Semaphore>,
        release: Arc<Notify>,
        closed: Arc<Mutex<bool>>,
    }

    impl BlockingCatalogLifecycleTransport {
        fn with_catalog(
            prompts: Vec<McpPromptDefinition>,
            resources: Vec<McpResourceDefinition>,
        ) -> Self {
            Self {
                prompts,
                resources,
                entered: Arc::new(Semaphore::new(0)),
                release: Arc::new(Notify::new()),
                closed: Arc::new(Mutex::new(false)),
            }
        }
    }

    #[derive(Debug)]
    struct UnsupportedCatalogOperationTransport;

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

    #[async_trait]
    impl McpToolTransport for BlockingCloseTransport {
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
            self.entered.add_permits(1);
            self.release.notified().await;
            Ok(())
        }
    }

    #[async_trait]
    impl McpToolTransport for FailingCatalogRpcTransport {
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

        async fn get_prompt(
            &self,
            _name: &str,
            _arguments: Option<HashMap<String, String>>,
        ) -> Result<McpPromptResult, McpTransportError> {
            Err(McpTransportError::ConnectionClosed)
        }

        async fn read_resource(&self, _uri: &str) -> Result<Value, McpTransportError> {
            Err(McpTransportError::ConnectionClosed)
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    #[async_trait]
    impl McpToolTransport for StaticCatalogTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
            Ok(self.prompts.clone())
        }

        async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
            Ok(self.resources.clone())
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
    impl McpToolTransport for BlockingCatalogLifecycleTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
            self.entered.add_permits(1);
            self.release.notified().await;
            if *self.closed.lock().unwrap() {
                return Err(McpTransportError::ConnectionClosed);
            }
            Ok(self.prompts.clone())
        }

        async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
            self.entered.add_permits(1);
            self.release.notified().await;
            if *self.closed.lock().unwrap() {
                return Err(McpTransportError::ConnectionClosed);
            }
            Ok(self.resources.clone())
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
            *self.closed.lock().unwrap() = true;
            Ok(())
        }
    }

    #[async_trait]
    impl McpToolTransport for UnsupportedCatalogOperationTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn get_prompt(
            &self,
            _name: &str,
            _arguments: Option<HashMap<String, String>>,
        ) -> Result<McpPromptResult, McpTransportError> {
            Err(McpTransportError::TransportError(
                "get_prompt not supported by this server".to_string(),
            ))
        }

        async fn read_resource(&self, _uri: &str) -> Result<Value, McpTransportError> {
            Err(McpTransportError::TransportError(
                "read_resource not supported by this server".to_string(),
            ))
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

    fn prompt_def(name: &str) -> McpPromptDefinition {
        McpPromptDefinition {
            name: name.to_string(),
            title: None,
            description: None,
            arguments: Vec::new(),
        }
    }

    fn resource_def(uri: &str) -> McpResourceDefinition {
        McpResourceDefinition {
            uri: uri.to_string(),
            name: uri.to_string(),
            title: None,
            description: None,
            mime_type: None,
            size: None,
        }
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
                generation: 1,
                transport_type: transport.transport_type(),
                transport,
                capabilities: None,
            }),
            health: McpRefreshHealth::default(),
            discovery_health: McpHealthBudget::default(),
            rpc_health: McpHealthBudget::default(),
            reconnect_attempts: 0,
            next_generation: 2,
            published_snapshot: Some(McpPublishedSnapshot {
                generation: 1,
                catalog: McpPublishedCatalog {
                    _tool_defs: vec![MockTransport::tool_def("echo")],
                    tools: HashMap::new(),
                },
            }),
            lifecycle_lock: Arc::new(AsyncMutex::new(())),
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
        assert_eq!(mgr.version(), 1);

        mgr.toggle("srv", false).await.unwrap();

        let registry = mgr.registry();
        assert_eq!(mgr.version(), 2);
        assert!(registry.get("mcp__srv__echo").is_none());
        assert!(registry.ids().is_empty());

        let health = mgr.server_health("srv").unwrap();
        assert!(!health.reconnecting);

        let servers = read_lock(&mgr.state.servers);
        let index = find_server_index(&servers, "srv").unwrap();
        let slot = &servers[index];
        assert_eq!(slot.lifecycle, McpServerLifecycle::Disabled);
        assert!(slot.runtime.is_none());
        assert!(slot.published_snapshot.is_none());
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
        assert!(slot.published_snapshot.is_none());
    }

    #[tokio::test]
    async fn manager_toggle_disable_close_failure_still_disables_and_unpublishes() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        let registry = mgr.registry();

        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].runtime = Some(McpServerRuntime {
                generation: 1,
                transport_type: TransportTypeId::Stdio,
                transport: Arc::new(CloseFailingTransport::new(
                    "echo",
                    "scripted disable close failure",
                )) as Arc<dyn McpToolTransport>,
                capabilities: None,
            });
            servers[0].lifecycle = McpServerLifecycle::Connected;
            servers[0].health.reconnecting = true;
        }

        let err = mgr.toggle("srv", false).await.unwrap_err();
        assert!(err.to_string().contains("scripted disable close failure"));

        let after = read_lock(&mgr.state.servers)[0].clone();
        assert_eq!(after.lifecycle, McpServerLifecycle::Disabled);
        assert!(after.runtime.is_none());
        assert!(after.published_snapshot.is_none());
        assert!(
            after
                .health
                .last_error
                .as_deref()
                .is_some_and(|msg| msg.contains("scripted disable close failure"))
        );
        assert!(registry.get("mcp__srv__echo").is_none());
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

        {
            let servers = read_lock(&mgr.state.servers);
            assert_eq!(servers[0].health.consecutive_failures, 2);
            assert_eq!(servers[0].reconnect_attempts, 0);
            assert!(servers[0].runtime.is_some());
            assert!(!servers[0].health.reconnecting);
        }

        mgr.refresh().await.unwrap();

        let servers = read_lock(&mgr.state.servers);
        assert_eq!(servers[0].reconnect_attempts, 1);
        assert!(servers[0].runtime.is_none());
        assert_eq!(servers[0].lifecycle, McpServerLifecycle::Disconnected);
        assert!(!servers[0].health.reconnecting);
        assert!(
            servers[0]
                .published_snapshot
                .as_ref()
                .map(|s| s.catalog.tools.contains_key("mcp__srv__echo"))
                .unwrap_or(false)
        );
        // Snapshot retains the generation from the last successful catalog discovery.
        assert_eq!(
            servers[0].published_snapshot.as_ref().map(|s| s.generation),
            Some(1)
        );
    }

    #[tokio::test]
    async fn tool_success_does_not_clear_discovery_failure_budget() {
        let transport = Arc::new(FailingRefreshTransport::new(vec![MockTransport::tool_def(
            "echo",
        )]));
        let mgr = McpToolRegistryManager::from_transports(vec![(
            cfg("srv"),
            transport.clone() as Arc<dyn McpToolTransport>,
        )])
        .await
        .unwrap();
        let tool = mgr.registry().get("mcp__srv__echo").unwrap();
        let ctx = awaken_contract::contract::tool::ToolCallContext::test_default();
        transport.fail_next_refreshes(3);

        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[0].meta.config.args.clear();
            servers[0].meta.config.timeout_secs = 1;
        }

        mgr.refresh().await.unwrap();
        tool.execute(json!({}), &ctx).await.unwrap();
        mgr.refresh().await.unwrap();
        tool.execute(json!({}), &ctx).await.unwrap();

        {
            let servers = read_lock(&mgr.state.servers);
            assert_eq!(servers[0].discovery_health.consecutive_failures, 2);
            assert_eq!(servers[0].rpc_health.consecutive_failures, 0);
            assert_eq!(servers[0].reconnect_attempts, 0);
        }

        mgr.refresh().await.unwrap();

        let servers = read_lock(&mgr.state.servers);
        assert_eq!(
            servers[0].discovery_health.consecutive_failures,
            FAILURE_THRESHOLD
        );
        assert_eq!(servers[0].rpc_health.consecutive_failures, 0);
        assert_eq!(servers[0].reconnect_attempts, 1);
        assert_eq!(servers[0].lifecycle, McpServerLifecycle::Disconnected);
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
        assert!(servers[0].published_snapshot.is_none());
    }

    #[test]
    fn reset_all_server_health_on_success_clears_reconnect_state() {
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
        slot.discovery_health.consecutive_failures = FAILURE_THRESHOLD;
        slot.discovery_health.last_error = Some("discovery boom".to_string());
        slot.rpc_health.consecutive_failures = FAILURE_THRESHOLD;
        slot.rpc_health.last_error = Some("rpc boom".to_string());

        reset_all_server_health_on_success(&mut slot, attempted_at);

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
                generation: 1,
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
        assert_eq!(slot.lifecycle, McpServerLifecycle::Disconnected);
        assert!(slot.runtime.is_none());
        assert!(!slot.health.reconnecting);
        // Close failure does not consume reconnect budget.
        assert_eq!(slot.reconnect_attempts, 0);
        assert!(
            slot.published_snapshot
                .as_ref()
                .map(|s| s.catalog.tools.contains_key("mcp__srv__echo"))
                .unwrap_or(false)
        );
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
    async fn busy_bad_server_lifecycle_does_not_block_good_refresh_or_toggle() {
        let bad = Arc::new(BlockingCloseTransport::new("echo"));
        let entered = bad.entered.clone();
        let release = bad.release.clone();
        let good = Arc::new(MockTransport::with_tools(vec![MockTransport::tool_def(
            "sum",
        )])) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![
            (cfg("bad"), bad as Arc<dyn McpToolTransport>),
            (cfg("good"), good),
        ])
        .await
        .unwrap();

        {
            let mut servers = write_lock(&mgr.state.servers);
            let bad_index = find_server_index(&servers, "bad").unwrap();
            servers[bad_index].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[bad_index].meta.config.args.clear();
            servers[bad_index].meta.config.timeout_secs = 1;
        }

        let mgr_for_reconnect = mgr.clone();
        let reconnect_task = tokio::spawn(async move { mgr_for_reconnect.reconnect("bad").await });
        entered.acquire().await.unwrap().forget();

        tokio::time::timeout(Duration::from_millis(100), mgr.refresh())
            .await
            .expect("good refresh should not wait for bad reconnect")
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), mgr.toggle("good", false))
            .await
            .expect("good toggle should not wait for bad reconnect")
            .unwrap();

        release.notify_waiters();
        let _ = reconnect_task.await.unwrap();
    }

    #[tokio::test]
    async fn list_prompts_skips_server_closed_during_aggregate_listing() {
        let bad = Arc::new(BlockingCatalogLifecycleTransport::with_catalog(
            vec![prompt_def("bad")],
            Vec::new(),
        ));
        let entered = bad.entered.clone();
        let release = bad.release.clone();
        let good = Arc::new(StaticCatalogTransport::with_catalog(
            vec![prompt_def("good")],
            Vec::new(),
        )) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![
            (cfg("bad"), bad as Arc<dyn McpToolTransport>),
            (cfg("good"), good),
        ])
        .await
        .unwrap();

        let mgr_for_list = mgr.clone();
        let list_task = tokio::spawn(async move { mgr_for_list.list_prompts().await.unwrap() });
        entered.acquire().await.unwrap().forget();

        mgr.toggle("bad", false).await.unwrap();
        release.notify_waiters();

        let prompts = list_task.await.unwrap();
        let names: Vec<&str> = prompts
            .iter()
            .map(|entry| entry.prompt.name.as_str())
            .collect();
        assert_eq!(names, vec!["good"]);
    }

    #[tokio::test]
    async fn list_resources_skips_server_closed_during_aggregate_listing() {
        let bad = Arc::new(BlockingCatalogLifecycleTransport::with_catalog(
            Vec::new(),
            vec![resource_def("file://bad.md")],
        ));
        let entered = bad.entered.clone();
        let release = bad.release.clone();
        let good = Arc::new(StaticCatalogTransport::with_catalog(
            Vec::new(),
            vec![resource_def("file://good.md")],
        )) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![
            (cfg("bad"), bad as Arc<dyn McpToolTransport>),
            (cfg("good"), good),
        ])
        .await
        .unwrap();

        let mgr_for_list = mgr.clone();
        let list_task = tokio::spawn(async move { mgr_for_list.list_resources().await.unwrap() });
        entered.acquire().await.unwrap().forget();

        mgr.toggle("bad", false).await.unwrap();
        release.notify_waiters();

        let resources = list_task.await.unwrap();
        let uris: Vec<&str> = resources
            .iter()
            .map(|entry| entry.resource.uri.as_str())
            .collect();
        assert_eq!(uris, vec!["file://good.md"]);
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
                generation: 2,
                transport_type: TransportTypeId::Stdio,
                transport: replacement.clone() as Arc<dyn McpToolTransport>,
                capabilities: None,
            });
            servers[0].next_generation = 3;
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
        assert_eq!(servers[0].discovery_health.consecutive_failures, 0);
        assert_eq!(
            servers[0].rpc_health.consecutive_failures,
            FAILURE_THRESHOLD
        );
        assert_eq!(servers[0].lifecycle, McpServerLifecycle::Disconnected);
        assert_eq!(servers[0].reconnect_attempts, 1);
    }

    #[tokio::test]
    async fn prompt_and_resource_failures_share_rpc_reconnect_budget() {
        let transport =
            Arc::new(FailingCatalogRpcTransport::new("echo")) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![(cfg("srv"), transport)])
            .await
            .unwrap();
        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].meta.config.command = Some("__missing_mcp_command__".to_string());
            servers[0].meta.config.args.clear();
            servers[0].meta.config.timeout_secs = 1;
        }

        for _ in 0..2 {
            let err = mgr.get_prompt("srv", "review", None).await.unwrap_err();
            assert!(matches!(err, McpError::Transport(_)));
        }
        let err = mgr
            .read_resource("srv", "file://guide.md")
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Transport(_)));

        let servers = read_lock(&mgr.state.servers);
        assert_eq!(servers[0].discovery_health.consecutive_failures, 0);
        assert_eq!(
            servers[0].rpc_health.consecutive_failures,
            FAILURE_THRESHOLD
        );
        assert_eq!(servers[0].lifecycle, McpServerLifecycle::Disconnected);
        assert_eq!(servers[0].reconnect_attempts, 1);
    }

    #[tokio::test]
    async fn prompt_and_resource_not_supported_errors_are_capability_errors() {
        let transport = Arc::new(UnsupportedCatalogOperationTransport) as Arc<dyn McpToolTransport>;
        let mgr = McpToolRegistryManager::from_transports(vec![(cfg("srv"), transport)])
            .await
            .unwrap();

        let prompt_err = mgr.get_prompt("srv", "review", None).await.unwrap_err();
        assert!(matches!(
            prompt_err,
            McpError::UnsupportedCapability {
                server_name,
                capability: "prompts",
            } if server_name == "srv"
        ));

        let resource_err = mgr
            .read_resource("srv", "file://guide.md")
            .await
            .unwrap_err();
        assert!(matches!(
            resource_err,
            McpError::UnsupportedCapability {
                server_name,
                capability: "resources",
            } if server_name == "srv"
        ));
    }

    #[tokio::test]
    async fn stale_generation_failure_does_not_mutate_current_runtime_health() {
        let mgr = make_manager_with(vec![("srv", vec![MockTransport::tool_def("echo")])]).await;
        let old_generation = {
            let servers = read_lock(&mgr.state.servers);
            servers[0].runtime.as_ref().unwrap().generation
        };

        {
            let mut servers = write_lock(&mgr.state.servers);
            servers[0].runtime = Some(McpServerRuntime {
                generation: old_generation + 1,
                transport_type: TransportTypeId::Stdio,
                transport: Arc::new(MockTransport::with_tools(vec![MockTransport::tool_def(
                    "echo",
                )])) as Arc<dyn McpToolTransport>,
                capabilities: None,
            });
            servers[0].next_generation = old_generation + 2;
            servers[0].lifecycle = McpServerLifecycle::Connected;
            servers[0].health = McpRefreshHealth::default();
            servers[0].discovery_health = McpHealthBudget::default();
            servers[0].rpc_health = McpHealthBudget::default();
        }

        record_runtime_operation_failure(
            &Arc::downgrade(&mgr.state),
            "srv",
            old_generation,
            McpRuntimeOpKind::Rpc,
            &McpTransportError::ConnectionClosed,
        )
        .await;

        let servers = read_lock(&mgr.state.servers);
        let slot = &servers[0];
        assert_eq!(
            slot.runtime.as_ref().unwrap().generation,
            old_generation + 1
        );
        assert_eq!(slot.health.consecutive_failures, 0);
        assert!(slot.health.last_error.is_none());
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
