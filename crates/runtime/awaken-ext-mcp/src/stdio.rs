//! Stdio MCP transport.
//!
//! Spawns an MCP server subprocess and drives it through the [`JsonRpcPeer`]
//! demux, so — unlike the SDK's client — server notifications
//! (`progress`, `tools/list_changed`, `resources/updated`) and server->client
//! requests (`sampling`) are surfaced rather than dropped. `tools/list` and
//! `tools/call` go through the peer's request path, which preserves the raw
//! [`CallToolResult`] (and its `isError` flag) for the three-state mapping in
//! [`McpRawTool`](crate::tool::McpRawTool).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_mcp_wire::McpTransportError;
use awaken_mcp_wire::{
    CallToolParams, CallToolResult, InitializeParams, ListToolsResult, McpToolDefinition,
};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::jsonrpc::{JsonRpcPeer, ServerRequestHandler};
use crate::progress::McpProgressUpdate;
use crate::router::{NotificationSinks, spawn_router};
use crate::transport::{ListChangedKind, McpToolTransport};
use crate::types::{
    ListPromptsResult, ListResourcesResult, McpPromptDefinition, McpPromptResult,
    McpResourceDefinition,
};

/// Per-request timeout used when a caller does not supply one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A stdio-spawned MCP server connection presented as an [`McpToolTransport`].
pub struct StdioTransport {
    peer: JsonRpcPeer,
    timeout: Duration,
    /// The child is kept alive here (its stdio is owned by the peer's tasks);
    /// `kill_on_drop` reaps it when this transport is dropped.
    child: Mutex<Child>,
    /// Typed notification sinks fed by the background router.
    sinks: Arc<NotificationSinks>,
    /// Allocates a unique `progressToken` per progress-tracked call.
    next_progress_token: AtomicI64,
}

impl StdioTransport {
    /// Spawn `command args...` and complete the MCP handshake.
    pub async fn connect(
        command: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Self, McpTransportError> {
        Self::spawn_and_init(command, args, HashMap::new(), None, timeout, None).await
    }

    /// Spawn with extra environment variables and an optional `initialize`
    /// config payload.
    pub async fn connect_with_env(
        command: &str,
        args: &[String],
        env: HashMap<String, String>,
        config: Option<Value>,
        timeout: Duration,
    ) -> Result<Self, McpTransportError> {
        Self::spawn_and_init(command, args, env, config, timeout, None).await
    }

    /// Spawn with a handler for server->client requests (e.g. sampling).
    pub async fn connect_with_handler(
        command: &str,
        args: &[String],
        env: HashMap<String, String>,
        config: Option<Value>,
        timeout: Duration,
        request_handler: Arc<dyn ServerRequestHandler>,
    ) -> Result<Self, McpTransportError> {
        Self::spawn_and_init(command, args, env, config, timeout, Some(request_handler)).await
    }

    /// Spawn with a sampling handler bound: server-initiated
    /// `sampling/createMessage` runs through `sampling`, everything else is
    /// rejected method-not-found.
    pub async fn connect_with_sampling(
        command: &str,
        args: &[String],
        env: HashMap<String, String>,
        config: Option<Value>,
        timeout: Duration,
        sampling: Arc<dyn crate::sampling::SamplingHandler>,
    ) -> Result<Self, McpTransportError> {
        let handler: Arc<dyn ServerRequestHandler> =
            Arc::new(crate::sampling::SamplingBridge::new(sampling));
        Self::spawn_and_init(command, args, env, config, timeout, Some(handler)).await
    }

    async fn spawn_and_init(
        command: &str,
        args: &[String],
        env: HashMap<String, String>,
        config: Option<Value>,
        timeout: Duration,
        request_handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> Result<Self, McpTransportError> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().map_err(|e| {
            McpTransportError::TransportError(format!("failed to spawn '{command}': {e}"))
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpTransportError::TransportError("child has no stdout".to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpTransportError::TransportError("child has no stdin".to_string()))?;

        let (peer, notifications) = JsonRpcPeer::new(stdout, stdin, request_handler);
        let sinks = Arc::new(NotificationSinks::new());
        spawn_router(notifications, Arc::clone(&sinks));
        let transport = Self {
            peer,
            timeout,
            child: Mutex::new(child),
            sinks,
            next_progress_token: AtomicI64::new(1),
        };
        transport.initialize(config).await?;
        Ok(transport)
    }

    /// MCP lifecycle handshake: the `initialize` request, then the
    /// `notifications/initialized` acknowledgement.
    async fn initialize(&self, config: Option<Value>) -> Result<(), McpTransportError> {
        let params = InitializeParams::new(config);
        self.peer
            .request("initialize", serde_json::to_value(&params)?, self.timeout)
            .await?;
        self.peer
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Subscribe to `list_changed` signals (tool/prompt/resource catalog
    /// changes) — the dynamic-refresh trigger.
    pub fn subscribe_list_changed(&self) -> broadcast::Receiver<ListChangedKind> {
        self.sinks.list_changed.subscribe()
    }

    /// Subscribe to `resources/updated` uris.
    pub fn subscribe_resource_updated(&self) -> broadcast::Receiver<String> {
        self.sinks.resource_updated.subscribe()
    }

    /// Whether the connection is still open.
    pub fn is_alive(&self) -> bool {
        self.peer.is_alive()
    }

    /// Terminate the child process.
    pub async fn stop(&self) -> Result<(), McpTransportError> {
        self.child
            .lock()
            .await
            .kill()
            .await
            .map_err(|e| McpTransportError::TransportError(e.to_string()))
    }
}

#[async_trait]
impl McpToolTransport for StdioTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        let result = self
            .peer
            .request("tools/list", serde_json::json!({}), self.timeout)
            .await?;
        let parsed: ListToolsResult = serde_json::from_value(result)?;
        Ok(parsed.tools)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpTransportError> {
        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: Some(arguments),
            task: None,
            meta: None,
        };
        let result = self
            .peer
            .request("tools/call", serde_json::to_value(&params)?, self.timeout)
            .await?;
        // Preserve the raw result — including `isError` — so the three-state
        // mapping in `McpRawTool` stays intact.
        let call_result: CallToolResult = serde_json::from_value(result)?;
        Ok(call_result)
    }

    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        progress_tx: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<CallToolResult, McpTransportError> {
        // Register the per-call progress channel under a fresh token, thread the
        // token through `_meta`, and clean up once the call returns.
        let token = self.next_progress_token.fetch_add(1, Ordering::SeqCst);
        self.sinks.progress.lock().await.insert(token, progress_tx);
        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: Some(arguments),
            task: None,
            meta: Some(serde_json::json!({ "progressToken": token })),
        };
        let result = self
            .peer
            .request("tools/call", serde_json::to_value(&params)?, self.timeout)
            .await;
        self.sinks.progress.lock().await.remove(&token);
        let call_result: CallToolResult = serde_json::from_value(result?)?;
        Ok(call_result)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        let result = self
            .peer
            .request("prompts/list", serde_json::json!({}), self.timeout)
            .await?;
        let parsed: ListPromptsResult = serde_json::from_value(result)?;
        Ok(parsed.prompts)
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        let result = self
            .peer
            .request(
                "prompts/get",
                serde_json::json!({ "name": name, "arguments": arguments }),
                self.timeout,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        let result = self
            .peer
            .request("resources/list", serde_json::json!({}), self.timeout)
            .await?;
        let parsed: ListResourcesResult = serde_json::from_value(result)?;
        Ok(parsed.resources)
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.peer
            .request(
                "resources/read",
                serde_json::json!({ "uri": uri }),
                self.timeout,
            )
            .await
    }

    fn is_alive(&self) -> bool {
        self.peer.is_alive()
    }
}
