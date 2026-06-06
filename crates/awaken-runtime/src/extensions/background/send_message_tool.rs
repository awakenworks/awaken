//! Unified `send_message` tool for agent-to-agent communication.
//!
//! The tool can do exactly one thing: validate/resolve the recipient against the
//! read-only snapshot and **commit an entry to the [`MessageOutboxKey`] outbox**.
//! It holds no transport (manager/sink), so it cannot send through a lossy,
//! uncommitted path — the "durable side effect on a non-durable channel" bug is
//! unrepresentable here. The transports live solely in [`MessageDispatchHook`],
//! the privileged dispatcher that drains the committed outbox at `StepEnd`.
//!
//! Routing is automatic — the caller does not specify delivery mode; it is data
//! the dispatcher acts on:
//! - `child` → live inbox (in-process, best-effort)
//! - `parent` / `agent` → durable mailbox (persistent, cross-process, at-least-once)

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use awaken_runtime_contract::StateError;
use awaken_runtime_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

use crate::hooks::{PhaseContext, PhaseHook};
use crate::state::{StateCommand, StateKey};

use super::manager::BackgroundTaskManager;
use super::state::BackgroundTaskStateKey;

pub const SEND_MESSAGE_TOOL_ID: &str = "send_message";

// ── Types ────────────────────────────────────────────────────────────

/// Recipient selector — who receives the message.
///
/// Retained as a public type for downstream callers that build typed
/// requests. The current `send_message_tool` body parses arguments from
/// raw JSON; this enum mirrors that wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "relation", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum RecipientRef {
    /// Send to the parent agent that spawned the current task/agent.
    Parent,
    /// Send to a child background task by name or task_id.
    Child {
        /// Task name (e.g. "researcher") or task_id (e.g. "bg_0").
        name: String,
    },
    /// Send to another agent by thread_id (team/swarm messaging).
    Agent {
        /// Target agent's thread ID.
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
}

/// Result returned to the LLM after sending.
///
/// `status` is `"queued"` on success — the message was committed to the outbox
/// (see [`MessageOutboxKey`]), **not** confirmed delivered. `message_id` is a
/// client-side correlation id (an agent cannot act on a transport dispatch id,
/// so none is returned).
///
/// The intent is committed with the run, so it survives a checkpoint/crash and
/// is re-dispatched by [`MessageDispatchHook`] (at-least-once for durable
/// routes; best-effort for the live `child` route). Durable rejections are
/// dead-lettered to [`FailedDurableMessageKey`] rather than dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageReceipt {
    pub message_id: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Unified error codes for message delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageError {
    RecipientNotFound,
    PermissionDenied,
    RecipientUnavailable,
    TransportFailed(String),
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecipientNotFound => write!(f, "recipient_not_found"),
            Self::PermissionDenied => write!(f, "permission_denied"),
            Self::RecipientUnavailable => write!(f, "recipient_unavailable"),
            Self::TransportFailed(e) => write!(f, "transport_failed: {e}"),
        }
    }
}

/// Durable cross-thread message request emitted by the runtime.
///
/// The runtime does not know how this becomes durable work. A host can map it
/// to a mailbox dispatch, an A2A submission, or another transport without
/// making runtime depend on mailbox storage internals.
///
/// `message_id` is the stable, sender-side id of this message (the outbox entry
/// id). Delivery is at-least-once, so the host/recipient MUST use it to
/// deduplicate redelivered messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableMessageRequest {
    pub message_id: String,
    pub recipient_thread_id: String,
    pub recipient_agent_id: Option<String>,
    pub sender_agent_id: String,
    pub message: String,
}

/// Host-provided sink for durable cross-thread messages.
#[async_trait]
pub trait DurableMessageSink: Send + Sync {
    async fn send_agent_message(&self, request: DurableMessageRequest) -> Result<String, String>;
}

// ── Outbox: the single committed channel for all message delivery ─────
//
// Feature code (the tool) can ONLY enqueue an entry here — a plain state
// mutation committed with the run. It holds no transport (manager/sink), so it
// cannot send through a lossy, uncommitted path. The transports live solely in
// [`MessageDispatchHook`] below, which is the privileged dispatcher. This makes
// the "durable side effect on a non-durable channel" bug unrepresentable in
// feature code: there is no channel to choose and no sink to call.

/// How a queued message is delivered. Resolved by the tool from the read-only
/// snapshot; consumed only by [`MessageDispatchHook`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutboxRoute {
    /// Live in-process delivery to a child task's inbox (best-effort: the inbox
    /// is ephemeral, so a dead target is dropped).
    ChildInbox {
        task_id: String,
        owner_thread_id: String,
        sender_agent_id: String,
        message: String,
    },
    /// Durable cross-thread delivery via the host sink (at-least-once).
    Durable(DurableMessageRequest),
}

/// A queued message awaiting delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub id: String,
    pub route: OutboxRoute,
    /// Durable delivery attempts already made (for bounded retry).
    #[serde(default)]
    pub attempts: u32,
}

/// Persisted message outbox — the only way to send a message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageOutbox {
    pub pending: Vec<OutboxEntry>,
}

/// Reducer action for [`MessageOutboxKey`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageOutboxUpdate {
    Enqueue(OutboxEntry),
    Remove { id: String },
}

impl MessageOutbox {
    pub(crate) fn reduce(&mut self, update: MessageOutboxUpdate) {
        match update {
            MessageOutboxUpdate::Enqueue(entry) => self.pending.push(entry),
            MessageOutboxUpdate::Remove { id } => self.pending.retain(|entry| entry.id != id),
        }
    }
}

/// State key for the message outbox (Thread-scoped, persistent — so an entry
/// undelivered at run end is re-dispatched by a later run on the thread).
pub struct MessageOutboxKey;

impl StateKey for MessageOutboxKey {
    const KEY: &'static str = "background_message_outbox";
    type Value = MessageOutbox;
    type Update = MessageOutboxUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

/// A durable message whose delivery the sink rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedDurableMessage {
    pub request: DurableMessageRequest,
    pub error: String,
}

/// Persisted dead-letter list for durable messages the sink rejected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailedDurableMessageState {
    pub messages: Vec<FailedDurableMessage>,
}

/// Reducer action for [`FailedDurableMessageKey`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FailedDurableMessageUpdate {
    Push(FailedDurableMessage),
}

impl FailedDurableMessageState {
    pub(crate) fn reduce(&mut self, update: FailedDurableMessageUpdate) {
        match update {
            FailedDurableMessageUpdate::Push(message) => self.messages.push(message),
        }
    }
}

/// State key for the durable-message dead-letter list (Thread-scoped, persistent).
///
/// Each dead-letter is also logged (`tracing::warn` with `message_id`) so it is
/// observable. A consumer/replay/cleanup surface and a metric counter are a
/// tracked follow-up — without one this list will accumulate silently.
pub struct FailedDurableMessageKey;

impl StateKey for FailedDurableMessageKey {
    const KEY: &'static str = "background_failed_durable_messages";
    type Value = FailedDurableMessageState;
    type Update = FailedDurableMessageUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

/// Durable delivery attempts before a message is dead-lettered.
const MAX_DURABLE_ATTEMPTS: u32 = 5;

/// Privileged message dispatcher — the only code that holds the delivery
/// transports (live manager + optional host durable sink).
///
/// Runs at `StepEnd`, drains the committed [`MessageOutboxKey`], and delivers
/// each entry by route:
/// - `child` (live inbox): best-effort; a dead target is dropped.
/// - `durable`: delivered through the sink. On a transient rejection the entry
///   is kept (retried next `StepEnd`) up to [`MAX_DURABLE_ATTEMPTS`], then
///   dead-lettered to [`FailedDurableMessageKey`]. With no sink configured,
///   durable routes are dead-lettered immediately (child still works).
///
/// Delivered entries are removed in the returned command, so on a crash before
/// that commit an undelivered entry stays in the outbox and is re-dispatched
/// (at-least-once; the recipient deduplicates on `DurableMessageRequest.message_id`).
pub struct MessageDispatchHook {
    manager: Arc<BackgroundTaskManager>,
    durable_sink: Option<Arc<dyn DurableMessageSink>>,
}

impl MessageDispatchHook {
    pub fn new(
        manager: Arc<BackgroundTaskManager>,
        durable_sink: Option<Arc<dyn DurableMessageSink>>,
    ) -> Self {
        Self {
            manager,
            durable_sink,
        }
    }
}

#[async_trait]
impl PhaseHook for MessageDispatchHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let outbox = ctx.state::<MessageOutboxKey>().cloned().unwrap_or_default();
        let mut cmd = StateCommand::new();
        for entry in outbox.pending {
            let OutboxEntry {
                id,
                route,
                attempts,
            } = entry;
            match route {
                OutboxRoute::ChildInbox {
                    task_id,
                    owner_thread_id,
                    sender_agent_id,
                    message,
                } => {
                    if let Err(error) = self
                        .manager
                        .send_task_inbox_message(
                            &task_id,
                            &owner_thread_id,
                            &sender_agent_id,
                            &message,
                        )
                        .await
                    {
                        // The inbox is ephemeral; a dead target is best-effort dropped.
                        tracing::debug!(?error, task_id, "child inbox delivery dropped");
                    }
                    cmd.update::<MessageOutboxKey>(MessageOutboxUpdate::Remove { id });
                }
                OutboxRoute::Durable(request) => {
                    let Some(sink) = &self.durable_sink else {
                        tracing::warn!(
                            message_id = %request.message_id,
                            "no durable transport configured; dead-lettering"
                        );
                        cmd.update::<FailedDurableMessageKey>(FailedDurableMessageUpdate::Push(
                            FailedDurableMessage {
                                request,
                                error: "no durable transport configured".into(),
                            },
                        ));
                        cmd.update::<MessageOutboxKey>(MessageOutboxUpdate::Remove { id });
                        continue;
                    };
                    match sink.send_agent_message(request.clone()).await {
                        Ok(dispatch_id) => {
                            tracing::debug!(
                                dispatch_id,
                                message_id = %request.message_id,
                                "durable message dispatched"
                            );
                            cmd.update::<MessageOutboxKey>(MessageOutboxUpdate::Remove { id });
                        }
                        Err(error) => {
                            let next = attempts + 1;
                            if next < MAX_DURABLE_ATTEMPTS {
                                // Keep it for retry on a later StepEnd.
                                tracing::warn!(
                                    message_id = %request.message_id,
                                    attempt = next,
                                    %error,
                                    "durable delivery failed; will retry"
                                );
                                cmd.update::<MessageOutboxKey>(MessageOutboxUpdate::Remove {
                                    id: id.clone(),
                                });
                                cmd.update::<MessageOutboxKey>(MessageOutboxUpdate::Enqueue(
                                    OutboxEntry {
                                        id,
                                        route: OutboxRoute::Durable(request),
                                        attempts: next,
                                    },
                                ));
                            } else {
                                tracing::warn!(
                                    message_id = %request.message_id,
                                    attempts = next,
                                    %error,
                                    "durable delivery exhausted; dead-lettering"
                                );
                                cmd.update::<FailedDurableMessageKey>(
                                    FailedDurableMessageUpdate::Push(FailedDurableMessage {
                                        request,
                                        error,
                                    }),
                                );
                                cmd.update::<MessageOutboxKey>(MessageOutboxUpdate::Remove { id });
                            }
                        }
                    }
                }
            }
        }
        Ok(cmd)
    }
}

// ── Tool ─────────────────────────────────────────────────────────────

/// Unified message-sending tool exposed to LLMs.
///
/// Stateless: delivery runs through [`SendMessageEffectHandler`], registered
/// next to this tool by [`super::plugin::BackgroundTaskPlugin`].
#[derive(Default)]
pub struct SendMessageTool;

impl SendMessageTool {
    pub fn new() -> Self {
        Self
    }

    /// Resolve a live, same-thread child task by name or id against the
    /// read-only snapshot. Returns the resolved `task_id`.
    fn resolve_child(name: &str, owner_thread_id: &str, ctx: &ToolCallContext) -> Option<String> {
        let snap = ctx.state::<BackgroundTaskStateKey>()?;
        if let Some(meta) = snap.tasks.get(name)
            && meta.owner_thread_id == owner_thread_id
            && !meta.status.is_terminal()
        {
            return Some(name.to_string());
        }
        for meta in snap.tasks.values() {
            if meta.owner_thread_id == owner_thread_id
                && !meta.status.is_terminal()
                && meta.name.as_deref() == Some(name)
            {
                return Some(meta.task_id.clone());
            }
        }
        None
    }

    fn make_receipt(msg_id: String) -> SendMessageReceipt {
        SendMessageReceipt {
            message_id: msg_id,
            // "queued", not "delivered": the effect is dispatched post-commit
            // and not yet confirmed. See `SendMessageReceipt` docs.
            status: "queued",
            error: None,
        }
    }

    fn make_error(code: MessageError) -> SendMessageReceipt {
        SendMessageReceipt {
            message_id: String::new(),
            status: "failed",
            error: Some(code.to_string()),
        }
    }

    /// A failed receipt is reported to the LLM as a successful tool call
    /// carrying a structured failure status (no effect is emitted).
    fn failed_output(code: MessageError) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success(
            SEND_MESSAGE_TOOL_ID,
            serde_json::to_value(Self::make_error(code))
                .map_err(|e| ToolError::Internal(e.to_string()))?,
        )
        .into())
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            SEND_MESSAGE_TOOL_ID,
            SEND_MESSAGE_TOOL_ID,
            "Send a message to a child task, parent agent, or team member.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "to": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "relation": { "const": "parent" }
                            },
                            "required": ["relation"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "relation": { "const": "child" },
                                "name": { "type": "string", "description": "Task name or ID" }
                            },
                            "required": ["relation", "name"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "relation": { "const": "agent" },
                                "thread_id": { "type": "string" },
                                "agent_id": { "type": "string" }
                            },
                            "required": ["relation", "thread_id"]
                        }
                    ]
                },
                "message": { "type": "string" }
            },
            "required": ["to", "message"]
        }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        let to = args
            .get("to")
            .ok_or_else(|| ToolError::InvalidArguments("missing 'to'".into()))?;
        let relation = to
            .get("relation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("missing 'to.relation'".into()))?;
        match relation {
            "child" => {
                if to.get("name").and_then(Value::as_str).is_none() {
                    return Err(ToolError::InvalidArguments("child requires 'name'".into()));
                }
            }
            "agent" => {
                if to.get("thread_id").and_then(Value::as_str).is_none() {
                    return Err(ToolError::InvalidArguments(
                        "agent requires 'thread_id'".into(),
                    ));
                }
            }
            "parent" => {}
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown relation '{other}'"
                )));
            }
        }
        if args.get("message").and_then(Value::as_str).is_none() {
            return Err(ToolError::InvalidArguments("missing 'message'".into()));
        }
        Ok(())
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        self.validate_args(&args)?;
        let to = args
            .get("to")
            .ok_or_else(|| ToolError::InvalidArguments("missing 'to'".into()))?;
        let relation = to
            .get("relation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("missing 'to.relation'".into()))?;
        let message = args
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("missing 'message'".into()))?
            .to_string();
        let sender = ctx.run_identity.agent_id.clone();
        let thread_id = ctx.run_identity.thread_id.clone();
        // Stable id for this message: it is the outbox entry id, carried into the
        // durable request so the recipient can dedup an at-least-once redelivery.
        let msg_id = uuid::Uuid::now_v7().to_string();

        // Resolve the recipient from the read-only snapshot, then commit the
        // delivery as an outbox entry. The tool holds no transport; routing
        // (live vs durable) is data the dispatcher acts on, not a channel the
        // tool picks.
        let route = match relation {
            "child" => {
                let name = to
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::InvalidArguments("child requires 'name'".into()))?;
                match Self::resolve_child(name, &thread_id, ctx) {
                    Some(task_id) => OutboxRoute::ChildInbox {
                        task_id,
                        owner_thread_id: thread_id,
                        sender_agent_id: sender,
                        message,
                    },
                    None => return Self::failed_output(MessageError::RecipientNotFound),
                }
            }
            "parent" => match ctx.run_identity.parent_thread_id.as_deref() {
                Some(parent_tid) => OutboxRoute::Durable(DurableMessageRequest {
                    message_id: msg_id.clone(),
                    recipient_thread_id: parent_tid.to_string(),
                    // The runner infers the parent agent from the thread's
                    // latest run record; we don't carry parent_agent_id here.
                    recipient_agent_id: None,
                    sender_agent_id: sender,
                    message,
                }),
                None => return Self::failed_output(MessageError::RecipientUnavailable),
            },
            "agent" => {
                let target_thread =
                    to.get("thread_id").and_then(Value::as_str).ok_or_else(|| {
                        ToolError::InvalidArguments("agent requires 'thread_id'".into())
                    })?;
                let target_agent = to
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                OutboxRoute::Durable(DurableMessageRequest {
                    message_id: msg_id.clone(),
                    recipient_thread_id: target_thread.to_string(),
                    recipient_agent_id: target_agent,
                    sender_agent_id: sender,
                    message,
                })
            }
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown relation '{other}'"
                )));
            }
        };

        let mut command = StateCommand::new();
        command.update::<MessageOutboxKey>(MessageOutboxUpdate::Enqueue(OutboxEntry {
            id: msg_id.clone(),
            route,
            attempts: 0,
        }));
        Ok(ToolOutput::with_command(
            ToolResult::success(
                SEND_MESSAGE_TOOL_ID,
                serde_json::to_value(Self::make_receipt(msg_id))
                    .map_err(|e| ToolError::Internal(e.to_string()))?,
            ),
            command,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::background::{
        BackgroundTaskPlugin, TaskParentContext, TaskResult as BgTaskResult,
    };
    use crate::state::StateStore;
    use awaken_runtime_contract::contract::identity::RunIdentity;
    use awaken_runtime_contract::model::Phase;
    use awaken_runtime_contract::registry_spec::AgentSpec;
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct RecordingDurableSink {
        requests: Mutex<Vec<DurableMessageRequest>>,
    }

    #[async_trait]
    impl DurableMessageSink for RecordingDurableSink {
        async fn send_agent_message(
            &self,
            request: DurableMessageRequest,
        ) -> Result<String, String> {
            let mut requests = self.requests.lock().await;
            requests.push(request);
            Ok(format!("durable-{}", requests.len()))
        }
    }

    fn make_ctx_with_store(thread_id: &str, agent_id: &str, store: &StateStore) -> ToolCallContext {
        ToolCallContext {
            call_id: "call-1".into(),
            tool_name: SEND_MESSAGE_TOOL_ID.into(),
            run_identity: RunIdentity::new(
                thread_id.to_string(),
                None,
                "run-1".to_string(),
                None,
                agent_id.to_string(),
                awaken_runtime_contract::contract::identity::RunOrigin::User,
            ),
            agent_spec: Arc::new(AgentSpec::default()),
            snapshot: store.snapshot(),
            activity_sink: None,
            cancellation_token: None,
            resume_input: None,
            suspension_id: None,
            suspension_reason: None,
        }
    }

    fn make_ctx(thread_id: &str, agent_id: &str) -> ToolCallContext {
        make_ctx_with_store(thread_id, agent_id, &StateStore::new())
    }

    struct FailingSink;

    #[async_trait]
    impl DurableMessageSink for FailingSink {
        async fn send_agent_message(
            &self,
            _request: DurableMessageRequest,
        ) -> Result<String, String> {
            Err("sink down".into())
        }
    }

    /// Build a messaging-enabled env (registers the outbox + dead-letter keys)
    /// and a store wired to the manager.
    fn messaging_env() -> (
        Arc<BackgroundTaskManager>,
        Arc<RecordingDurableSink>,
        StateStore,
    ) {
        use crate::phase::ExecutionEnv;
        use crate::plugins::Plugin;
        let store = StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_store(store.clone());
        let sink = Arc::new(RecordingDurableSink::default());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::with_messaging(
            manager.clone(),
            sink.clone(),
        ));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        store.register_keys(&env.key_registrations).unwrap();
        (manager, sink, store)
    }

    /// Commit a tool output's patch and read back the committed outbox.
    fn outbox_of(store: &StateStore, out: ToolOutput) -> MessageOutbox {
        store.commit(out.command.patch).unwrap();
        store.read::<MessageOutboxKey>().unwrap_or_default()
    }

    fn enqueue(store: &StateStore, id: &str, route: OutboxRoute) {
        let mut cmd = StateCommand::new();
        cmd.update::<MessageOutboxKey>(MessageOutboxUpdate::Enqueue(OutboxEntry {
            id: id.into(),
            route,
            attempts: 0,
        }));
        store.commit(cmd.patch).unwrap();
    }

    fn durable_req(message_id: &str, recipient_thread_id: &str) -> DurableMessageRequest {
        DurableMessageRequest {
            message_id: message_id.into(),
            recipient_thread_id: recipient_thread_id.into(),
            recipient_agent_id: None,
            sender_agent_id: "sender".into(),
            message: "hello".into(),
        }
    }

    // -- tool: enqueue child route --

    #[tokio::test]
    async fn child_enqueues_child_route() {
        let (manager, _sink, store) = messaging_env();
        manager
            .spawn_agent(
                "thread-1",
                Some("researcher"),
                "desc",
                TaskParentContext::default(),
                |cancel, _s, _r| async move {
                    cancel.cancelled().await;
                    BgTaskResult::Cancelled
                },
            )
            .await
            .unwrap();

        let tool = SendMessageTool::new();
        // Snapshot AFTER spawn so the task metadata is visible.
        let ctx = make_ctx_with_store("thread-1", "parent", &store);
        let out = tool
            .execute(
                json!({"to": {"relation": "child", "name": "researcher"}, "message": "hi"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.result.data["status"], "queued");
        let outbox = outbox_of(&store, out);
        assert_eq!(outbox.pending.len(), 1);
        match &outbox.pending[0].route {
            OutboxRoute::ChildInbox {
                owner_thread_id,
                sender_agent_id,
                message,
                ..
            } => {
                assert_eq!(owner_thread_id, "thread-1");
                assert_eq!(sender_agent_id, "parent");
                assert_eq!(message, "hi");
            }
            other => panic!("expected child route, got {other:?}"),
        }
        manager.cancel_all("thread-1").await;
    }

    #[tokio::test]
    async fn child_wrong_thread_fails_without_enqueue() {
        let (manager, _sink, store) = messaging_env();
        manager
            .spawn_agent(
                "thread-1",
                Some("worker"),
                "desc",
                TaskParentContext::default(),
                |cancel, _s, _r| async move {
                    cancel.cancelled().await;
                    BgTaskResult::Cancelled
                },
            )
            .await
            .unwrap();

        let tool = SendMessageTool::new();
        let ctx = make_ctx_with_store("thread-WRONG", "attacker", &store);
        let out = tool
            .execute(
                json!({"to": {"relation": "child", "name": "worker"}, "message": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.result.data["status"], "failed");
        assert!(outbox_of(&store, out).pending.is_empty());
        manager.cancel_all("thread-1").await;
    }

    // -- tool: enqueue durable route (agent/parent) --

    #[tokio::test]
    async fn agent_enqueues_durable_route() {
        let (_manager, _sink, store) = messaging_env();
        let tool = SendMessageTool::new();
        let ctx = make_ctx_with_store("thread-1", "sender", &store);
        let out = tool
            .execute(
                json!({"to": {"relation": "agent", "thread_id": "thread-2"}, "message": "hello"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.result.data["status"], "queued");
        let outbox = outbox_of(&store, out);
        assert_eq!(outbox.pending.len(), 1);
        match &outbox.pending[0].route {
            OutboxRoute::Durable(req) => {
                assert_eq!(req.recipient_thread_id, "thread-2");
                assert_eq!(req.recipient_agent_id, None);
                assert_eq!(req.sender_agent_id, "sender");
                assert_eq!(req.message, "hello");
            }
            other => panic!("expected durable route, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_routing_includes_agent_id() {
        let (_manager, _sink, store) = messaging_env();
        let tool = SendMessageTool::new();
        let ctx = make_ctx_with_store("thread-1", "sender", &store);
        let out = tool
            .execute(
                json!({
                    "to": {"relation": "agent", "thread_id": "thread-target", "agent_id": "reviewer"},
                    "message": "please review"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.result.data["status"], "queued");
        let outbox = outbox_of(&store, out);
        match &outbox.pending[0].route {
            OutboxRoute::Durable(req) => {
                assert_eq!(req.recipient_agent_id.as_deref(), Some("reviewer"))
            }
            other => panic!("expected durable route, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parent_with_thread_id_enqueues_durable() {
        let (_manager, _sink, store) = messaging_env();
        let tool = SendMessageTool::new();
        let mut ctx = make_ctx_with_store("thread-child", "child-agent", &store);
        ctx.run_identity = RunIdentity::new(
            "thread-child".into(),
            Some("thread-parent".into()),
            "run-child".into(),
            Some("run-parent".into()),
            "child-agent".into(),
            awaken_runtime_contract::contract::identity::RunOrigin::Subagent,
        );

        let out = tool
            .execute(
                json!({"to": {"relation": "parent"}, "message": "analysis complete"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.result.data["status"], "queued");
        let outbox = outbox_of(&store, out);
        match &outbox.pending[0].route {
            OutboxRoute::Durable(req) => {
                assert_eq!(req.recipient_thread_id, "thread-parent");
                assert_eq!(req.recipient_agent_id, None);
                assert_eq!(req.sender_agent_id, "child-agent");
                assert_eq!(req.message, "analysis complete");
            }
            other => panic!("expected durable route, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parent_without_thread_id_returns_unavailable() {
        let (_manager, _sink, store) = messaging_env();
        let tool = SendMessageTool::new();
        let mut ctx = make_ctx_with_store("thread-1", "child", &store);
        ctx.run_identity = RunIdentity::new(
            "thread-1".into(),
            None,
            "run-child".into(),
            Some("run-parent".into()),
            "child".into(),
            awaken_runtime_contract::contract::identity::RunOrigin::Subagent,
        );

        let out = tool
            .execute(
                json!({"to": {"relation": "parent"}, "message": "hello parent"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.result.data["status"], "failed");
        assert!(
            out.result.data["error"]
                .as_str()
                .unwrap()
                .contains("recipient_unavailable")
        );
        assert!(outbox_of(&store, out).pending.is_empty());
    }

    // -- dispatcher: drains the committed outbox --

    #[tokio::test]
    async fn dispatch_delivers_durable_and_clears_outbox() {
        let (manager, sink, store) = messaging_env();
        enqueue(
            &store,
            "m1",
            OutboxRoute::Durable(durable_req("m1", "thread-2")),
        );

        let hook = MessageDispatchHook::new(manager.clone(), Some(sink.clone()));
        let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot());
        let cmd = hook.run(&ctx).await.unwrap();
        store.commit(cmd.patch).unwrap();

        let requests = sink.requests.lock().await;
        assert_eq!(requests.len(), 1);
        // The sender-side message id reaches the sink for dedup.
        assert_eq!(requests[0].message_id, "m1");
        assert!(
            store
                .read::<MessageOutboxKey>()
                .unwrap_or_default()
                .pending
                .is_empty()
        );
    }

    #[tokio::test]
    async fn dispatch_retries_then_dead_letters_durable() {
        let (manager, _sink, store) = messaging_env();
        enqueue(
            &store,
            "m1",
            OutboxRoute::Durable(durable_req("m1", "thread-2")),
        );
        let hook = MessageDispatchHook::new(manager.clone(), Some(Arc::new(FailingSink)));

        // Each StepEnd: a transient failure keeps the entry (attempts++) until
        // MAX_DURABLE_ATTEMPTS, then it is dead-lettered — not dropped, not retried forever.
        for _ in 0..MAX_DURABLE_ATTEMPTS {
            let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot());
            let cmd = hook.run(&ctx).await.unwrap();
            store.commit(cmd.patch).unwrap();
        }

        assert!(
            store
                .read::<MessageOutboxKey>()
                .unwrap_or_default()
                .pending
                .is_empty(),
            "exhausted entry leaves the outbox"
        );
        assert_eq!(
            store
                .read::<FailedDurableMessageKey>()
                .unwrap_or_default()
                .messages
                .len(),
            1,
            "exhausted entry is dead-lettered exactly once"
        );
    }

    #[tokio::test]
    async fn dispatch_dead_letters_durable_without_sink() {
        // No durable transport: child still works, durable is dead-lettered.
        let (manager, _sink, store) = messaging_env();
        enqueue(
            &store,
            "m1",
            OutboxRoute::Durable(durable_req("m1", "thread-2")),
        );

        let hook = MessageDispatchHook::new(manager.clone(), None);
        let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot());
        let cmd = hook.run(&ctx).await.unwrap();
        store.commit(cmd.patch).unwrap();

        assert_eq!(
            store
                .read::<FailedDurableMessageKey>()
                .unwrap_or_default()
                .messages
                .len(),
            1
        );
        assert!(
            store
                .read::<MessageOutboxKey>()
                .unwrap_or_default()
                .pending
                .is_empty()
        );
    }

    #[tokio::test]
    async fn redelivery_after_crash_keeps_same_message_id() {
        let (manager, sink, store) = messaging_env();
        enqueue(
            &store,
            "m1",
            OutboxRoute::Durable(durable_req("m1", "thread-2")),
        );
        let hook = MessageDispatchHook::new(manager.clone(), Some(sink.clone()));

        // First dispatch delivers, but the returned remove is NOT committed
        // (simulate a crash between sink delivery and the outbox commit).
        let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot());
        let _dropped = hook.run(&ctx).await.unwrap();

        // The entry is still committed in the outbox → it is redelivered.
        let ctx2 = PhaseContext::new(Phase::StepEnd, store.snapshot());
        let cmd = hook.run(&ctx2).await.unwrap();
        store.commit(cmd.patch).unwrap();

        let requests = sink.requests.lock().await;
        assert_eq!(requests.len(), 2, "redelivered after the dropped commit");
        assert!(
            requests.iter().all(|r| r.message_id == "m1"),
            "redelivery carries the same message_id so the recipient can dedup"
        );
    }

    #[tokio::test]
    async fn dispatch_delivers_child_without_sink() {
        let (manager, _sink, store) = messaging_env();
        let task_id = manager
            .spawn_agent(
                "thread-1",
                Some("researcher"),
                "desc",
                TaskParentContext::default(),
                |cancel, _s, _r| async move {
                    cancel.cancelled().await;
                    BgTaskResult::Cancelled
                },
            )
            .await
            .unwrap();
        enqueue(
            &store,
            "m1",
            OutboxRoute::ChildInbox {
                task_id,
                owner_thread_id: "thread-1".into(),
                sender_agent_id: "parent".into(),
                message: "hi".into(),
            },
        );

        // No durable sink configured — child delivery must still work.
        let hook = MessageDispatchHook::new(manager.clone(), None);
        let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot());
        let cmd = hook.run(&ctx).await.unwrap();
        store.commit(cmd.patch).unwrap();

        assert!(
            store
                .read::<MessageOutboxKey>()
                .unwrap_or_default()
                .pending
                .is_empty(),
            "child entry delivered and removed even without a durable sink"
        );
        manager.cancel_all("thread-1").await;
    }

    // -- ambient sink seam: scoped by the host, read by the per-run plugin --

    #[tokio::test]
    async fn ambient_durable_sink_seam_round_trips() {
        use crate::extensions::background::{
            current_durable_message_sink, scope_durable_message_sink,
        };
        let sink: Arc<dyn DurableMessageSink> = Arc::new(RecordingDurableSink::default());
        assert!(
            current_durable_message_sink().is_none(),
            "unset outside any scope"
        );
        let visible = scope_durable_message_sink(sink.clone(), async {
            current_durable_message_sink().is_some()
        })
        .await;
        assert!(
            visible,
            "the scoped sink is visible to code running inside the run"
        );
        assert!(
            current_durable_message_sink().is_none(),
            "and is gone again once the scope ends"
        );
    }

    // -- closed loop: enqueue, then the real StepEnd phase drives delivery --

    #[tokio::test]
    async fn closed_loop_dispatches_durable_at_step_end() {
        use crate::phase::{ExecutionEnv, PhaseRuntime};
        use crate::plugins::Plugin;
        let store = StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_store(store.clone());
        let sink = Arc::new(RecordingDurableSink::default());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::with_messaging(
            manager.clone(),
            sink.clone(),
        ));
        // LoopStatePlugin registers PendingWorkKey, which the background StepEnd
        // sync hook writes — required for a full StepEnd phase run.
        let loop_plugin: Arc<dyn Plugin> = Arc::new(crate::loop_runner::LoopStatePlugin);
        let env = ExecutionEnv::from_plugins(&[plugin, loop_plugin], &Default::default()).unwrap();
        store.register_keys(&env.key_registrations).unwrap();

        // Enqueue as the tool would, then run the *real* StepEnd phase: the engine
        // must invoke the registered dispatcher hook (not a direct call).
        enqueue(
            &store,
            "m1",
            OutboxRoute::Durable(durable_req("m1", "thread-2")),
        );
        let runtime = PhaseRuntime::new(store.clone()).unwrap();
        runtime.run_phase(&env, Phase::StepEnd).await.unwrap();

        assert_eq!(
            sink.requests.lock().await.len(),
            1,
            "dispatcher fired at StepEnd through the engine"
        );
        assert!(
            store
                .read::<MessageOutboxKey>()
                .unwrap_or_default()
                .pending
                .is_empty()
        );
    }

    #[tokio::test]
    async fn dispatch_delivers_child_inbox() {
        let (manager, sink, store) = messaging_env();
        let task_id = manager
            .spawn_agent(
                "thread-1",
                Some("researcher"),
                "desc",
                TaskParentContext::default(),
                |cancel, _s, _r| async move {
                    cancel.cancelled().await;
                    BgTaskResult::Cancelled
                },
            )
            .await
            .unwrap();
        enqueue(
            &store,
            "m1",
            OutboxRoute::ChildInbox {
                task_id,
                owner_thread_id: "thread-1".into(),
                sender_agent_id: "parent".into(),
                message: "hi".into(),
            },
        );

        let hook = MessageDispatchHook::new(manager.clone(), Some(sink.clone()));
        let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot());
        let cmd = hook.run(&ctx).await.unwrap();
        store.commit(cmd.patch).unwrap();

        // Child delivery goes through the manager, not the durable sink.
        assert!(sink.requests.lock().await.is_empty());
        assert!(
            store
                .read::<MessageOutboxKey>()
                .unwrap_or_default()
                .pending
                .is_empty()
        );
        manager.cancel_all("thread-1").await;
    }

    // -- validation --

    #[test]
    fn rejects_missing_relation() {
        let t = SendMessageTool::new();
        assert!(
            t.validate_args(&json!({"to": {}, "message": "hi"}))
                .is_err()
        );
    }

    #[test]
    fn rejects_child_without_name() {
        let t = SendMessageTool::new();
        assert!(
            t.validate_args(&json!({"to": {"relation": "child"}, "message": "hi"}))
                .is_err()
        );
    }

    #[test]
    fn rejects_agent_without_thread_id() {
        let t = SendMessageTool::new();
        assert!(
            t.validate_args(&json!({"to": {"relation": "agent"}, "message": "hi"}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn execute_rejects_invalid_args() {
        let tool = SendMessageTool::new();
        let ctx = make_ctx("thread-1", "agent-1");
        let error = tool
            .execute(json!({"to": {"relation": "child"}, "message": "hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn accepts_valid_child() {
        let t = SendMessageTool::new();
        assert!(
            t.validate_args(&json!({"to": {"relation": "child", "name": "r"}, "message": "hi"}))
                .is_ok()
        );
    }

    #[test]
    fn accepts_valid_parent() {
        let t = SendMessageTool::new();
        assert!(
            t.validate_args(&json!({"to": {"relation": "parent"}, "message": "hi"}))
                .is_ok()
        );
    }

    #[test]
    fn accepts_valid_agent() {
        let t = SendMessageTool::new();
        assert!(
            t.validate_args(
                &json!({"to": {"relation": "agent", "thread_id": "t1"}, "message": "hi"})
            )
            .is_ok()
        );
    }

    // -- registration: send_message is available with or without a sink --
    // (the child route needs no durable transport; durable degrades to dead-letter)

    #[test]
    fn send_message_registered_with_sink() {
        use crate::phase::ExecutionEnv;
        use crate::plugins::Plugin;
        let manager = Arc::new(BackgroundTaskManager::new());
        let sink: Arc<dyn DurableMessageSink> = Arc::new(RecordingDurableSink::default());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::with_messaging(manager, sink));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        assert!(env.tools.contains_key(SEND_MESSAGE_TOOL_ID));
    }

    #[test]
    fn send_message_registered_without_sink() {
        use crate::phase::ExecutionEnv;
        use crate::plugins::Plugin;
        let manager = Arc::new(BackgroundTaskManager::new());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        // Child messaging works without a durable sink — the tool is still registered.
        assert!(env.tools.contains_key(SEND_MESSAGE_TOOL_ID));
    }
}
