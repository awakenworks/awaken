//! MCP tool transport abstraction.
//!
//! `McpToolTransport` is the seam [`McpRawTool`](crate::tool::McpRawTool) calls
//! and that a fake stands in for under test. The concrete wire transports
//! (stdio, HTTP/SSE) that speak the protocol over the `mcp` SDK — plus the
//! notification, sampling, and progress channels — land in later phases; the
//! method set grows additively as they do.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_mcp_wire::McpTransportError;
use awaken_mcp_wire::{CallToolResult, CreateTaskResult, McpTask, McpToolDefinition};
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::progress::McpProgressUpdate;
use crate::types::{McpPromptDefinition, McpPromptResult, McpResourceDefinition};

struct McpCallFenceState {
    accepting: AtomicBool,
    in_flight: AtomicUsize,
    cancellation: awaken_runtime_contract::CancellationToken,
    quiesced: Notify,
}

/// Process-local lifecycle handle for one exact MCP generation transport.
/// Durable desired state remains in the Session aggregate; this handle only
/// closes already-materialized I/O and proves local call quiescence before the
/// generation's drain receipt is acknowledged.
#[derive(Clone)]
pub struct McpCallFence(Arc<McpCallFenceState>);

impl McpCallFence {
    /// Hide the transport from new calls, cancel every in-flight future, and
    /// wait until all local call guards have left. Replays are idempotent.
    pub async fn close_and_wait(&self) {
        self.0.accepting.store(false, Ordering::SeqCst);
        self.0.cancellation.cancel();
        loop {
            let notified = self.0.quiesced.notified();
            tokio::pin!(notified);
            // Register before observing the counter so the last guard's
            // `notify_waiters` cannot fall into the check-to-await gap.
            notified.as_mut().enable();
            if self.0.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.0.in_flight.load(Ordering::SeqCst)
    }
}

struct McpCallGuard(Arc<McpCallFenceState>);

impl Drop for McpCallGuard {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.quiesced.notify_waiters();
        }
    }
}

struct RevocableMcpTransport {
    inner: Arc<dyn McpToolTransport>,
    fence: McpCallFence,
}

impl RevocableMcpTransport {
    fn begin(&self) -> Result<McpCallGuard, McpTransportError> {
        if !self.fence.0.accepting.load(Ordering::SeqCst) {
            return Err(McpTransportError::TransportError(
                "MCP generation is draining".into(),
            ));
        }
        self.fence.0.in_flight.fetch_add(1, Ordering::SeqCst);
        let guard = McpCallGuard(self.fence.0.clone());
        if !self.fence.0.accepting.load(Ordering::SeqCst) {
            drop(guard);
            return Err(McpTransportError::TransportError(
                "MCP generation is draining".into(),
            ));
        }
        Ok(guard)
    }

    async fn run<T, F>(&self, future: F) -> Result<T, McpTransportError>
    where
        T: Send,
        F: Future<Output = Result<T, McpTransportError>> + Send,
    {
        let _guard = self.begin()?;
        tokio::select! {
            biased;
            _ = self.fence.0.cancellation.cancelled() => Err(
                McpTransportError::TransportError("MCP generation was revoked".into())
            ),
            result = future => result,
        }
    }
}

/// Wrap one already-connected MCP transport with the exact generation's local
/// revocation fence. This adds no registry or desired-state owner; the returned
/// handle is retained by the existing Session projection and closed by its
/// canonical drain phase.
pub fn revocable_transport(
    inner: Arc<dyn McpToolTransport>,
) -> (Arc<dyn McpToolTransport>, McpCallFence) {
    let fence = McpCallFence(Arc::new(McpCallFenceState {
        accepting: AtomicBool::new(true),
        in_flight: AtomicUsize::new(0),
        cancellation: awaken_runtime_contract::CancellationToken::new(),
        quiesced: Notify::new(),
    }));
    (
        Arc::new(RevocableMcpTransport {
            inner,
            fence: fence.clone(),
        }),
        fence,
    )
}

/// Which catalog a `notifications/*/list_changed` referred to. Consumed by the
/// dynamic-refresh path (a change advances the server's live tool version).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChangedKind {
    Tools,
    Prompts,
    Resources,
}

/// Raw MCP client transport: the wire operations `McpRawTool` needs to expose an
/// external server's tools as runtime tools, plus the prompt/resource surfaces a
/// host may consult. Tools are mandatory; prompts and resources default to
/// "unsupported" so a tools-only transport (or a test fake) need not implement
/// them.
#[async_trait]
pub trait McpToolTransport: Send + Sync {
    /// Discover the server's tools (`tools/list`).
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError>;

    /// Invoke one tool (`tools/call`). A returned [`CallToolResult`] with
    /// `is_error` set is a *tool* error (model-visible, run continues); an `Err`
    /// is a *transport* error (aborts the call). This three-state distinction is
    /// mapped to the neutral result in [`McpRawTool`](crate::tool::McpRawTool).
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpTransportError>;

    /// Invoke a tool while streaming its progress to `progress_tx`. Defaults to
    /// a plain [`call_tool`](Self::call_tool) (no progress) for transports that
    /// do not support server notifications.
    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        _progress_tx: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<CallToolResult, McpTransportError> {
        self.call_tool(tool_name, arguments).await
    }

    /// Whether `initialize` negotiated task augmentation for `tools/call`.
    /// Defaults to false so legacy and test transports fail closed.
    fn supports_task_tools_call(&self) -> bool {
        false
    }

    /// Whether `initialize` negotiated `tasks/cancel`, independently of task
    /// creation support.
    fn supports_task_cancel(&self) -> bool {
        false
    }

    /// Start `tools/call` with MCP task augmentation. The protocol-management
    /// methods remain adapter-internal and are never registered as model tools.
    async fn call_tool_as_task(
        &self,
        _tool_name: &str,
        _arguments: Value,
        _ttl_ms: Option<u64>,
    ) -> Result<CreateTaskResult, McpTransportError> {
        Err(McpTransportError::NotSupported(
            "task-augmented tools/call".to_string(),
        ))
    }

    /// Read one task's current status (`tasks/get`).
    async fn get_task(&self, _task_id: &str) -> Result<McpTask, McpTransportError> {
        Err(McpTransportError::NotSupported("tasks/get".to_string()))
    }

    /// Retrieve a completed tool task's original `CallToolResult`
    /// (`tasks/result`).
    async fn get_task_result(&self, _task_id: &str) -> Result<CallToolResult, McpTransportError> {
        Err(McpTransportError::NotSupported("tasks/result".to_string()))
    }

    /// Request cancellation and return the server's authoritative task status.
    async fn cancel_task(&self, _task_id: &str) -> Result<McpTask, McpTransportError> {
        Err(McpTransportError::NotSupported("tasks/cancel".to_string()))
    }

    /// List the server's prompts (`prompts/list`). Defaults to none.
    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        Ok(Vec::new())
    }

    /// Render a prompt (`prompts/get`). Defaults to unsupported.
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        Err(McpTransportError::NotSupported("prompts/get".to_string()))
    }

    /// List the server's resources (`resources/list`). Defaults to none.
    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        Ok(Vec::new())
    }

    /// Read a resource by uri (`resources/read`). Defaults to unsupported.
    async fn read_resource(&self, _uri: &str) -> Result<Value, McpTransportError> {
        Err(McpTransportError::NotSupported(
            "resources/read".to_string(),
        ))
    }

    /// Whether the connection is still usable. Defaults to `true` for stateless
    /// transports; a process-backed transport reports its child's liveness.
    fn is_alive(&self) -> bool {
        true
    }
}

#[async_trait]
impl McpToolTransport for RevocableMcpTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        self.run(self.inner.list_tools()).await
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpTransportError> {
        self.run(self.inner.call_tool(tool_name, arguments)).await
    }

    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        progress_tx: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<CallToolResult, McpTransportError> {
        self.run(
            self.inner
                .call_tool_with_progress(tool_name, arguments, progress_tx),
        )
        .await
    }

    fn supports_task_tools_call(&self) -> bool {
        // Negotiated semantics are immutable for this generation. Draining
        // rejects operations through `run`; it must not reclassify a detached
        // tool as foreground/model-visible while the registry is being replaced.
        self.inner.supports_task_tools_call()
    }

    fn supports_task_cancel(&self) -> bool {
        self.inner.supports_task_cancel()
    }

    async fn call_tool_as_task(
        &self,
        tool_name: &str,
        arguments: Value,
        ttl_ms: Option<u64>,
    ) -> Result<CreateTaskResult, McpTransportError> {
        self.run(self.inner.call_tool_as_task(tool_name, arguments, ttl_ms))
            .await
    }

    async fn get_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
        self.run(self.inner.get_task(task_id)).await
    }

    async fn get_task_result(&self, task_id: &str) -> Result<CallToolResult, McpTransportError> {
        self.run(self.inner.get_task_result(task_id)).await
    }

    async fn cancel_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
        self.run(self.inner.cancel_task(task_id)).await
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        self.run(self.inner.list_prompts()).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        self.run(self.inner.get_prompt(name, arguments)).await
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        self.run(self.inner.list_resources()).await
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.run(self.inner.read_resource(uri)).await
    }

    fn is_alive(&self) -> bool {
        self.fence.0.accepting.load(Ordering::SeqCst) && self.inner.is_alive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_mcp_wire::{CallToolResult, ToolContent};

    struct BlockingTransport {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl McpToolTransport for BlockingTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(CallToolResult {
                content: Vec::new(),
                structured_content: None,
                is_error: Some(false),
            })
        }

        fn supports_task_tools_call(&self) -> bool {
            true
        }

        fn supports_task_cancel(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn generation_drain_cancels_busy_calls_and_closes_new_admission() {
        // Cause/effect graph: C1 the generation accepts calls; C2 one call is
        // busy inside the transport; C3 drain closes the exact generation.
        // C4=this generation negotiated task capabilities. Effects: E1 the busy
        // future returns a revocation transport error; E2 close waits for the
        // in-flight guard to reach zero; E3 every later operation is rejected
        // before touching the delegate; E4 negotiated semantic facts stay
        // pinned while admission closes. Decision rules:
        // R1=C1+C2+!C3=>in-flight; R2=C1+C2+C3=>E1+E2;
        // R3=!C1+C3=>E3; R4=C3+C4=>E4. The fence is process-local effect state,
        // never Session desired state or a replacement generation authority.
        let entered = Arc::new(Notify::new());
        let delegate = Arc::new(BlockingTransport {
            entered: entered.clone(),
            release: Arc::new(Notify::new()),
        });
        let (transport, fence) = revocable_transport(delegate);
        let call = tokio::spawn({
            let transport = transport.clone();
            async move { transport.call_tool("busy", Value::Null).await }
        });
        entered.notified().await;
        assert_eq!(fence.in_flight(), 1, "R1");

        tokio::time::timeout(std::time::Duration::from_secs(1), fence.close_and_wait())
            .await
            .expect("R2/E2 drain reaches local quiescence");
        assert!(
            matches!(call.await.unwrap(), Err(McpTransportError::TransportError(message)) if message.contains("revoked")),
            "R2/E1"
        );
        assert_eq!(fence.in_flight(), 0, "R2/E2");
        assert!(
            matches!(transport.list_tools().await, Err(McpTransportError::TransportError(message)) if message.contains("draining")),
            "R3/E3"
        );
        assert!(transport.supports_task_tools_call(), "R4/E4");
        assert!(transport.supports_task_cancel(), "R4/E4");
    }

    /// A tools-only transport: it implements only the two mandatory methods, so the
    /// prompt/resource/progress/liveness surfaces exercise the trait defaults.
    struct ToolsOnly;

    #[async_trait]
    impl McpToolTransport for ToolsOnly {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }
        async fn call_tool(
            &self,
            tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: format!("ran {tool_name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }
    }

    #[tokio::test]
    async fn a_tools_only_transport_defaults_the_optional_surfaces_fail_soft() {
        // Cause/effect graph: C1=transport implements only list/call; C2=no
        // initialize task evidence exists. Effects E1=list surfaces are empty
        // and read surfaces explicitly unsupported; E2=task create/poll/cancel
        // capabilities stay false and operations are rejected. Rules:
        // D1=C1=>E1; D2=C1+C2=>E2. Defaults may preserve compatibility but
        // must never synthesize protocol authority.
        let t = ToolsOnly;
        // List surfaces default to empty (a tools-only server has none), never an error.
        assert!(t.list_prompts().await.expect("prompts default").is_empty());
        assert!(
            t.list_resources()
                .await
                .expect("resources default")
                .is_empty()
        );
        // Get/read surfaces default to an explicit NotSupported, not a panic.
        assert!(matches!(
            t.get_prompt("greet", None).await,
            Err(McpTransportError::NotSupported(m)) if m == "prompts/get"
        ));
        assert!(matches!(
            t.read_resource("file:///x").await,
            Err(McpTransportError::NotSupported(m)) if m == "resources/read"
        ));
        // A stateless transport reports alive by default.
        assert!(t.is_alive());
        // MCP Tasks are capability-negotiated. A legacy transport must never
        // acquire task execution or cancellation through trait defaults.
        assert!(!t.supports_task_tools_call());
        assert!(!t.supports_task_cancel());
        assert!(matches!(
            t.call_tool_as_task("echo", Value::Null, None).await,
            Err(McpTransportError::NotSupported(method)) if method == "task-augmented tools/call"
        ));
        assert!(matches!(
            t.get_task("remote-1").await,
            Err(McpTransportError::NotSupported(method)) if method == "tasks/get"
        ));
    }

    #[tokio::test]
    async fn call_tool_with_progress_defaults_to_a_plain_call() {
        // A transport with no server-notification support falls back to `call_tool`,
        // so a progress-aware caller still gets the result (just no progress events).
        let t = ToolsOnly;
        let (tx, mut rx) = mpsc::channel(1);
        let result = t
            .call_tool_with_progress("echo", Value::Null, tx)
            .await
            .expect("delegates to call_tool");
        assert!(matches!(result.is_error, Some(false)));
        // No progress was emitted on the fallback path.
        assert!(rx.try_recv().is_err());
    }
}
