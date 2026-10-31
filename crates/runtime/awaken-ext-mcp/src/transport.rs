// @formal entrypoint credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls
//! MCP tool transport abstraction.
//!
//! `McpToolTransport` is the seam [`McpRawTool`](crate::tool::McpRawTool) calls
//! and that a fake stands in for under test. The concrete wire transports
//! (stdio, HTTP/SSE) that speak the protocol over the `mcp` SDK — plus the
//! notification, sampling, and progress channels — land in later phases; the
//! method set grows additively as they do.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use awaken_sync::LeafLock;

use async_trait::async_trait;
use awaken_mcp_wire::McpTransportError;
use awaken_mcp_wire::{CallToolResult, CreateTaskResult, McpTask, McpToolDefinition};
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::progress::McpProgressUpdate;
use crate::types::{McpPromptDefinition, McpPromptResult, McpResourceDefinition};

/// Final admission rule for the increment-versus-drain race. Both sequentially
/// consistent observations must name an accepting exact generation.
#[must_use]
const fn mcp_call_admitted(
    accepting_before_increment: bool,
    accepting_after_increment: bool,
) -> bool {
    accepting_before_increment && accepting_after_increment
}

struct McpCallFenceState {
    accepting: AtomicBool,
    in_flight: AtomicUsize,
    cancellation: awaken_runtime_contract::CancellationToken,
    quiesced: Notify,
    transport: LeafLock<Option<Arc<dyn McpToolTransport>>>,
}

// Only explicit fence handles own admission. Counted permits retain State for
// local cleanup, and must never prolong the authority to admit another effect.
struct McpCallFenceOwner {
    state: Arc<McpCallFenceState>,
}

impl Drop for McpCallFenceOwner {
    fn drop(&mut self) {
        self.state.close_admission();
    }
}

/// Process-local lifecycle handle for one exact MCP generation transport.
/// Durable desired state remains in the Session aggregate; this handle only
/// closes already-materialized I/O and proves local call quiescence before the
/// generation's drain receipt is acknowledged.
#[derive(Clone)]
pub struct McpCallFence(Arc<McpCallFenceOwner>);

impl McpCallFence {
    /// Open one process-local activity fence. The caller must retain
    /// this exact handle with the generation effect owner; constructing a fence
    /// never creates durable desired state or another generation registry.
    pub fn open() -> Self {
        Self(Arc::new(McpCallFenceOwner {
            state: Arc::new(McpCallFenceState {
                accepting: AtomicBool::new(true),
                in_flight: AtomicUsize::new(0),
                cancellation: awaken_runtime_contract::CancellationToken::new(),
                quiesced: Notify::new(),
                transport: LeafLock::new(None),
            }),
        }))
    }

    /// Enter one already-admitted exact-generation effect. The second
    /// acceptance check closes the increment-versus-drain race: either this
    /// permit is counted by `close_and_wait`, or the effect never begins.
    pub fn try_enter(&self) -> Option<McpCallPermit> {
        self.0.state.try_enter()
    }

    /// Hide the transport from new calls, cancel every in-flight future, and
    /// wait until all local call guards have left. Replays are idempotent.
    pub async fn close_and_wait(&self) {
        self.0.state.close_admission();
        loop {
            let notified = self.0.state.quiesced.notified();
            tokio::pin!(notified);
            // Register before observing the counter so the last guard's
            // `notify_waiters` cannot fall into the check-to-await gap.
            notified.as_mut().enable();
            if self.0.state.in_flight.load(Ordering::SeqCst) == 0 {
                self.0.state.release_transport();
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.0.state.in_flight.load(Ordering::SeqCst)
    }
}

/// Opaque lifetime permit for one exact-generation MCP effect. Dropping the
/// permit is the only completion signal; it grants no authority to reopen or
/// cancel the fence.
pub struct McpCallPermit(Arc<McpCallFenceState>);

impl McpCallPermit {
    /// Wait until the exact generation owner closes this activity fence.
    pub async fn cancelled(&self) {
        self.0.cancellation.cancelled().await;
    }

    /// Observe cancellation at synchronous effect boundaries.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancellation.is_cancelled()
    }
}

impl McpCallFenceState {
    fn close_admission(&self) {
        self.accepting.store(false, Ordering::SeqCst);
        self.cancellation.cancel();
        if self.in_flight.load(Ordering::SeqCst) == 0 {
            self.release_transport();
        }
    }

    fn try_enter(self: &Arc<Self>) -> Option<McpCallPermit> {
        let accepting_before_increment = self.accepting.load(Ordering::SeqCst);
        if !accepting_before_increment {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let permit = McpCallPermit(self.clone());
        let accepting_after_increment = self.accepting.load(Ordering::SeqCst);
        if !mcp_call_admitted(accepting_before_increment, accepting_after_increment) {
            drop(permit);
            return None;
        }
        Some(permit)
    }

    fn release_transport(&self) {
        // Never run a transport destructor under the ownership mutex. A cached
        // consumer is only a weak view; it cannot prolong this local channel.
        let transport = self.transport.lock().take();
        drop(transport);
    }
}

impl Drop for McpCallPermit {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            if !self.0.accepting.load(Ordering::SeqCst) {
                // The last admitted call also completes local release when a
                // close waiter was cancelled. It does not undo a remote effect.
                self.0.release_transport();
            }
            self.0.quiesced.notify_waiters();
        }
    }
}

struct RevocableMcpTransport {
    fence: Weak<McpCallFenceState>,
    supports_task_tools_call: bool,
    supports_task_cancel: bool,
}

struct FencedNotificationHandler {
    fence: Weak<McpCallFenceState>,
    handler: Arc<dyn crate::notification::McpNotificationHandler>,
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    struct Counter(AtomicUsize);
    #[async_trait]
    impl crate::notification::McpNotificationHandler for Counter {
        async fn receive(&self, _method: &str, _params: Value) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn draining_a_generation_also_closes_notification_delivery() {
        // F1 open generation -> callback; F2 close acknowledged -> all later
        // incoming effects refused, including an already-cloned handler.
        use crate::notification::McpNotificationHandler as _;
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let fence = McpCallFence::open();
        let handler = FencedNotificationHandler {
            fence: Arc::downgrade(&fence.0.state),
            handler: counter.clone(),
        };
        handler
            .receive("notifications/phone/input", Value::Null)
            .await;
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        fence.close_and_wait().await;
        handler
            .receive("notifications/phone/input", Value::Null)
            .await;
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }
}

#[async_trait]
impl crate::notification::McpNotificationHandler for FencedNotificationHandler {
    async fn receive(&self, method: &str, params: Value) {
        let Some(state) = self.fence.upgrade() else {
            return;
        };
        let Some(permit) = state.try_enter() else {
            return;
        };
        tokio::select! {
            biased;
            _ = permit.cancelled() => {},
            () = self.handler.receive(method, params) => {},
        }
    }
}

impl RevocableMcpTransport {
    fn transport(&self) -> Result<Arc<dyn McpToolTransport>, McpTransportError> {
        self.fence
            .upgrade()
            .and_then(|state| state.transport.lock().clone())
            .ok_or_else(|| McpTransportError::NotDispatched("MCP generation is draining".into()))
    }

    async fn run<T, F>(&self, future: F) -> Result<T, McpTransportError>
    where
        T: Send,
        F: Future<Output = Result<T, McpTransportError>> + Send,
    {
        let state = self
            .fence
            .upgrade()
            .ok_or_else(|| McpTransportError::NotDispatched("MCP generation is draining".into()))?;
        let guard = state
            .try_enter()
            .ok_or_else(|| McpTransportError::NotDispatched("MCP generation is draining".into()))?;
        tokio::select! {
            biased;
            _ = guard.cancelled() => Err(
                McpTransportError::TransportError("MCP generation was revoked".into())
            ),
            result = future => result,
        }
    }
}

/// Wrap one already-connected MCP transport with the exact generation's local
/// revocation fence. This adds no registry or desired-state owner; the returned
/// handle is retained by the existing Session projection and closed by its
/// canonical drain phase. Cached consumers are weak views: callers must retain
/// the returned fence while the connection is usable. Its last owner closes
/// admission and cancels local calls; their original permits retain cleanup
/// resources until settlement. An explicit drain waits for that settlement.
pub fn revocable_transport(
    inner: Arc<dyn McpToolTransport>,
) -> (Arc<dyn McpToolTransport>, McpCallFence) {
    let fence = McpCallFence::open();
    // Capability facts describe this generation even after admission closes;
    // retaining those facts must not retain the original physical connection.
    let transport = Arc::new(RevocableMcpTransport {
        fence: Arc::downgrade(&fence.0.state),
        supports_task_tools_call: inner.supports_task_tools_call(),
        supports_task_cancel: inner.supports_task_cancel(),
    });
    *fence.0.state.transport.lock() = Some(inner);
    (transport, fence)
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
    /// Install the one connection-owner application notification consumer.
    /// Omitted support is a hard refusal, never a silently inert configuration.
    fn set_notification_handler(
        &self,
        _methods: Vec<String>,
        _handler: Option<Arc<dyn crate::notification::McpNotificationHandler>>,
    ) -> Result<(), McpTransportError> {
        Err(McpTransportError::NotSupported(
            "application notifications".into(),
        ))
    }
    /// Discover the server's tools (`tools/list`).
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError>;

    /// Invoke one tool (`tools/call`). A returned [`CallToolResult`] with
    /// `is_error` set is a *tool* error (model-visible, run continues); an `Err`
    /// is a *transport* error. Only `NotDispatched` proves that no send was
    /// admitted; all other errors conservatively leave execution unknown.
    /// [`McpRawTool`](crate::tool::McpRawTool) owns the neutral mapping.
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
    fn set_notification_handler(
        &self,
        methods: Vec<String>,
        handler: Option<Arc<dyn crate::notification::McpNotificationHandler>>,
    ) -> Result<(), McpTransportError> {
        self.transport()?.set_notification_handler(
            methods,
            handler.map(|handler| {
                Arc::new(FencedNotificationHandler {
                    fence: self.fence.clone(),
                    handler,
                }) as Arc<dyn crate::notification::McpNotificationHandler>
            }),
        )
    }
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        self.run(self.transport()?.list_tools()).await
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpTransportError> {
        self.run(self.transport()?.call_tool(tool_name, arguments))
            .await
    }

    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        progress_tx: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<CallToolResult, McpTransportError> {
        self.run(
            self.transport()?
                .call_tool_with_progress(tool_name, arguments, progress_tx),
        )
        .await
    }

    fn supports_task_tools_call(&self) -> bool {
        // Negotiated semantics are immutable for this generation. Draining
        // rejects operations through `run`; it must not reclassify a detached
        // tool as foreground/model-visible while the registry is being replaced.
        self.supports_task_tools_call
    }

    fn supports_task_cancel(&self) -> bool {
        self.supports_task_cancel
    }

    async fn call_tool_as_task(
        &self,
        tool_name: &str,
        arguments: Value,
        ttl_ms: Option<u64>,
    ) -> Result<CreateTaskResult, McpTransportError> {
        self.run(
            self.transport()?
                .call_tool_as_task(tool_name, arguments, ttl_ms),
        )
        .await
    }

    async fn get_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
        self.run(self.transport()?.get_task(task_id)).await
    }

    async fn get_task_result(&self, task_id: &str) -> Result<CallToolResult, McpTransportError> {
        self.run(self.transport()?.get_task_result(task_id)).await
    }

    async fn cancel_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
        self.run(self.transport()?.cancel_task(task_id)).await
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
        self.run(self.transport()?.list_prompts()).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<McpPromptResult, McpTransportError> {
        self.run(self.transport()?.get_prompt(name, arguments))
            .await
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceDefinition>, McpTransportError> {
        self.run(self.transport()?.list_resources()).await
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        self.run(self.transport()?.read_resource(uri)).await
    }

    fn is_alive(&self) -> bool {
        self.fence.upgrade().is_some_and(|state| {
            state.accepting.load(Ordering::SeqCst)
                && self.transport().is_ok_and(|transport| transport.is_alive())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_mcp_wire::{CallToolResult, ToolContent};

    #[test]
    fn call_admission_requires_both_accepting_observations() {
        // Causes: C1=accepting before the in-flight increment; C2=accepting
        // after it. Effects: E1=admit a counted permit, E2=reject. Complete
        // decision table: TT=>E1; TF, FT, FF=>E2. The async race test below
        // covers the counter rollback and cancellation effects around this rule.
        assert!(mcp_call_admitted(true, true), "TT/E1");
        assert!(!mcp_call_admitted(true, false), "TF/E2");
        assert!(!mcp_call_admitted(false, true), "FT/E2");
        assert!(!mcp_call_admitted(false, false), "FF/E2");
    }

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
            matches!(transport.list_tools().await, Err(McpTransportError::NotDispatched(message)) if message.contains("draining")),
            "R3/E3"
        );
        assert!(transport.supports_task_tools_call(), "R4/E4");
        assert!(transport.supports_task_cancel(), "R4/E4");
    }

    /// A tools-only transport: it implements only the two mandatory methods, so the
    /// prompt/resource/progress/liveness surfaces exercise the trait defaults.
    struct ToolsOnly;

    #[tokio::test]
    async fn revoked_generation_preserves_effect_uncertainty_at_the_raw_tool_boundary() {
        use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError};
        // G1 enter call then revoke => Indeterminate despite local drain.
        // G2 call the same closed generation => known pre-dispatch rejection.
        // G3 a separate generation remains callable. This verifies generation
        // isolation, not attempt-local cancellation within a shared generation.
        let entered = Arc::new(Notify::new());
        let (transport, fence) = revocable_transport(Arc::new(BlockingTransport {
            entered: entered.clone(),
            release: Arc::new(Notify::new()),
        }));
        let tool = Arc::new(crate::tool::McpRawTool::new("srv", "busy", transport).unwrap());
        let call = || ToolCall {
            call_id: "one".into(),
            tool_id: "mcp__srv__busy".into(),
            arguments: Value::Null,
        };
        let pending = tokio::spawn({
            let tool = tool.clone();
            let call = call();
            async move { tool.invoke(call).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
            .await
            .expect("timed out waiting for entered.notified()");
        fence.close_and_wait().await;
        assert!(
            matches!(pending.await.unwrap(), Err(ToolError::Indeterminate(_))),
            "G1"
        );
        assert!(
            matches!(
                tool.invoke(call()).await,
                Err(ToolError::UnavailableBeforeDispatch(_))
            ),
            "G2"
        );
        let (other, _other_owner) = revocable_transport(Arc::new(ToolsOnly));
        assert!(other.call_tool("busy", Value::Null).await.is_ok(), "G3");
    }

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

#[cfg(kani)]
/// @obligation credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls
#[kani::proof]
fn mcp_call_admission_requires_both_accepting_observations() {
    let accepting_before_increment = kani::any::<bool>();
    let accepting_after_increment = kani::any::<bool>();
    let admitted = mcp_call_admitted(accepting_before_increment, accepting_after_increment);

    assert_eq!(
        admitted,
        accepting_before_increment && accepting_after_increment
    );
    if !accepting_before_increment || !accepting_after_increment {
        assert!(!admitted);
    }
}

#[cfg(test)]
mod lifetime_tests;
