//! Unified `send_message` tool for agent-to-agent communication.
//!
//! Single agent-facing tool. Routing is automatic:
//! - `child` → live inbox (low latency, in-process)
//! - `parent` / `agent` → durable mailbox (persistent, cross-process)
//!
//! Transport selection is automatic — the caller does not specify delivery mode.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use awaken_contract::contract::mailbox::{
    MailboxJob, MailboxJobOrigin, MailboxJobStatus, MailboxStore,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::now_ms;

use super::manager::BackgroundTaskManager;
use super::state::BackgroundTaskStateKey;

pub const SEND_MESSAGE_TOOL_ID: &str = "send_message";

// ── Types ────────────────────────────────────────────────────────────

/// Recipient selector — who receives the message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "relation", rename_all = "snake_case")]
#[allow(dead_code)] // used by future typed API; current impl parses JSON directly
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

// ── Tool ─────────────────────────────────────────────────────────────

/// Unified message-sending tool exposed to LLMs.
pub struct SendMessageTool {
    manager: Arc<BackgroundTaskManager>,
    mailbox: Arc<dyn MailboxStore>,
}

impl SendMessageTool {
    pub fn new(manager: Arc<BackgroundTaskManager>, mailbox: Arc<dyn MailboxStore>) -> Self {
        Self { manager, mailbox }
    }

    async fn send_via_mailbox(
        &self,
        recipient_thread_id: &str,
        recipient_agent_id: &str,
        sender_agent_id: &str,
        message: &str,
    ) -> Result<String, String> {
        let now = now_ms();
        let job_id = uuid::Uuid::now_v7().to_string();
        let job = MailboxJob {
            job_id: job_id.clone(),
            mailbox_id: recipient_thread_id.to_string(),
            agent_id: recipient_agent_id.to_string(),
            messages: vec![Message::internal_user(format!(
                "<agent-message from=\"{sender_agent_id}\">\n{message}\n</agent-message>"
            ))],
            origin: MailboxJobOrigin::Internal,
            sender_id: Some(sender_agent_id.to_string()),
            parent_run_id: None,
            request_extras: None,
            priority: 128,
            dedupe_key: None,
            generation: 0,
            status: MailboxJobStatus::Queued,
            available_at: now,
            attempt_count: 0,
            max_attempts: 3,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            created_at: now,
            updated_at: now,
        };
        self.mailbox
            .enqueue(&job)
            .await
            .map(|_| job_id)
            .map_err(|e| e.to_string())
    }

    fn resolve_child(
        &self,
        name: &str,
        owner_thread_id: &str,
        ctx: &ToolCallContext,
    ) -> Option<String> {
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
            status: "accepted",
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
        let to = &args["to"];
        let relation = to["relation"].as_str().unwrap_or_default();
        let message = args["message"].as_str().unwrap_or_default();
        let sender = &ctx.run_identity.agent_id;
        let thread_id = &ctx.run_identity.thread_id;
        let msg_id = uuid::Uuid::now_v7().to_string();

        // Routing is automatic — runtime picks the transport:
        //   child  → live inbox (in-process)
        //   parent → durable mailbox (cross-thread)
        //   agent  → durable mailbox (cross-thread)
        let receipt = match relation {
            "child" => {
                let name = to["name"].as_str().unwrap_or_default();
                match self.resolve_child(name, thread_id, ctx) {
                    Some(task_id) => {
                        match self
                            .manager
                            .send_task_inbox_message(&task_id, thread_id, sender, message)
                            .await
                        {
                            Ok(()) => Self::make_receipt(msg_id.clone()),
                            Err(e) => {
                                use super::manager::SendError;
                                Self::make_error(match e {
                                    SendError::TaskNotFound => MessageError::RecipientNotFound,
                                    SendError::NotOwner => MessageError::PermissionDenied,
                                    SendError::TaskTerminated(_) | SendError::InboxClosed => {
                                        MessageError::RecipientUnavailable
                                    }
                                    SendError::NoInbox => MessageError::RecipientUnavailable,
                                })
                            }
                        }
                    }
                    None => Self::make_error(MessageError::RecipientNotFound),
                }
            }
            "parent" => match ctx.run_identity.parent_thread_id.as_deref() {
                Some(parent_tid) => {
                    // Leave agent_id empty — the runner will infer the correct
                    // agent from the thread's latest run record. We don't have
                    // parent_agent_id in RunIdentity yet.
                    match self.send_via_mailbox(parent_tid, "", sender, message).await {
                        Ok(job_id) => Self::make_receipt(job_id),
                        Err(e) => Self::make_error(MessageError::TransportFailed(e)),
                    }
                }
                None => Self::make_error(MessageError::RecipientUnavailable),
            },
            "agent" => {
                let target_thread = to["thread_id"].as_str().unwrap_or_default();
                let target_agent = to["agent_id"].as_str().unwrap_or_default();
                match self
                    .send_via_mailbox(target_thread, target_agent, sender, message)
                    .await
                {
                    Ok(job_id) => Self::make_receipt(job_id),
                    Err(e) => Self::make_error(MessageError::TransportFailed(e)),
                }
            }
            _ => Self::make_error(MessageError::RecipientNotFound),
        };

        Ok(ToolResult::success(
            SEND_MESSAGE_TOOL_ID,
            serde_json::to_value(&receipt).unwrap_or_default(),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::background::{
        BackgroundTaskPlugin, TaskParentContext, TaskResult as BgTaskResult,
    };
    use crate::state::StateStore;
    use awaken_contract::contract::identity::RunIdentity;
    use awaken_contract::registry_spec::AgentSpec;
    use awaken_stores::InMemoryMailboxStore;

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
                awaken_contract::contract::identity::RunOrigin::User,
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

    fn make_manager_and_store() -> (Arc<BackgroundTaskManager>, StateStore) {
        use crate::phase::ExecutionEnv;
        use crate::plugins::Plugin;
        let store = StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_store(store.clone());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        store.register_keys(&env.key_registrations).unwrap();
        (manager, store)
    }

    fn make_tool(manager: Arc<BackgroundTaskManager>) -> SendMessageTool {
        SendMessageTool::new(manager, Arc::new(InMemoryMailboxStore::new()))
    }

    // -- child by name --

    #[tokio::test]
    async fn child_by_name_delivers_live() {
        let (manager, store) = make_manager_and_store();
        manager
            .spawn_agent(
                "thread-1",
                Some("researcher"),
                "desc",
                TaskParentContext::default(),
                |cancel, _s, mut rx| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if rx.try_recv().is_some() {
                        BgTaskResult::Success(json!({"got": true}))
                    } else {
                        cancel.cancelled().await;
                        BgTaskResult::Cancelled
                    }
                },
            )
            .await
            .unwrap();

        let tool = make_tool(manager.clone());
        // Take snapshot AFTER spawn so the task metadata is visible
        let ctx = make_ctx_with_store("thread-1", "parent", &store);
        let r = tool
            .execute(
                json!({"to": {"relation": "child", "name": "researcher"}, "message": "hi"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(r.result.data["status"], "accepted");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // -- child wrong thread --

    #[tokio::test]
    async fn child_wrong_thread_permission_denied() {
        let (manager, store) = make_manager_and_store();
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

        let tool = make_tool(manager.clone());
        let ctx = make_ctx_with_store("thread-WRONG", "attacker", &store);
        let r = tool
            .execute(
                json!({"to": {"relation": "child", "name": "worker"}, "message": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(r.result.data["status"], "failed");
        manager.cancel_all("thread-1").await;
    }

    // -- child durable rejected --

    // -- agent via mailbox --

    #[tokio::test]
    async fn agent_delivers_durable() {
        let (manager, _store) = make_manager_and_store();
        let mailbox = Arc::new(InMemoryMailboxStore::new());
        let tool = SendMessageTool::new(manager, mailbox.clone());
        let ctx = make_ctx("thread-1", "sender");
        let r = tool
            .execute(
                json!({"to": {"relation": "agent", "thread_id": "thread-2"}, "message": "hello"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(r.result.data["status"], "accepted");

        let jobs = mailbox.list_jobs("thread-2", None, 10, 0).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].messages.len(), 1);
        assert_eq!(
            jobs[0].messages[0].role,
            awaken_contract::contract::message::Role::User
        );
        assert_eq!(
            jobs[0].messages[0].visibility,
            awaken_contract::contract::message::Visibility::Internal
        );
    }

    // -- agent live rejected --

    // -- parent --

    #[tokio::test]
    async fn parent_no_context_unavailable() {
        let (manager, _store) = make_manager_and_store();
        let tool = make_tool(manager);
        let ctx = make_ctx("thread-1", "child");
        let r = tool
            .execute(
                json!({"to": {"relation": "parent"}, "message": "done"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(r.result.data["status"], "failed");
        assert!(
            r.result.data["error"]
                .as_str()
                .unwrap()
                .contains("recipient_unavailable")
        );
    }

    #[tokio::test]
    async fn parent_with_thread_id_delivers() {
        let (manager, _store) = make_manager_and_store();
        let mailbox = Arc::new(InMemoryMailboxStore::new());
        let tool = SendMessageTool::new(manager, mailbox.clone());

        let mut ctx = make_ctx("thread-child", "child-agent");
        ctx.run_identity = awaken_contract::contract::identity::RunIdentity::new(
            "thread-child".into(),
            Some("thread-parent".into()),
            "run-child".into(),
            Some("run-parent".into()),
            "child-agent".into(),
            awaken_contract::contract::identity::RunOrigin::Subagent,
        );

        let r = tool
            .execute(
                json!({"to": {"relation": "parent"}, "message": "analysis complete"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(r.result.data["status"], "accepted");

        // Verify the mailbox job routes to the PARENT's thread, not the child's
        let jobs = mailbox
            .list_jobs("thread-parent", None, 10, 0)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1, "job should be queued on parent's thread");
        // agent_id should be empty (inferred by runner), NOT "run-parent"
        assert!(
            jobs[0].agent_id.is_empty(),
            "parent job agent_id should be empty for inference, got: '{}'",
            jobs[0].agent_id
        );
        assert_eq!(
            jobs[0].messages[0].role,
            awaken_contract::contract::message::Role::User
        );
        assert_eq!(
            jobs[0].messages[0].visibility,
            awaken_contract::contract::message::Visibility::Internal
        );
    }

    // -- validation --

    #[test]
    fn rejects_missing_relation() {
        let (m, _) = make_manager_and_store();
        let t = make_tool(m);
        assert!(
            t.validate_args(&json!({"to": {}, "message": "hi"}))
                .is_err()
        );
    }

    #[test]
    fn rejects_child_without_name() {
        let (m, _) = make_manager_and_store();
        let t = make_tool(m);
        assert!(
            t.validate_args(&json!({"to": {"relation": "child"}, "message": "hi"}))
                .is_err()
        );
    }

    #[test]
    fn rejects_agent_without_thread_id() {
        let (m, _) = make_manager_and_store();
        let t = make_tool(m);
        assert!(
            t.validate_args(&json!({"to": {"relation": "agent"}, "message": "hi"}))
                .is_err()
        );
    }

    #[test]
    fn accepts_valid_child() {
        let (m, _) = make_manager_and_store();
        let t = make_tool(m);
        assert!(
            t.validate_args(&json!({"to": {"relation": "child", "name": "r"}, "message": "hi"}))
                .is_ok()
        );
    }

    #[test]
    fn accepts_valid_parent() {
        let (m, _) = make_manager_and_store();
        let t = make_tool(m);
        assert!(
            t.validate_args(&json!({"to": {"relation": "parent"}, "message": "hi"}))
                .is_ok()
        );
    }

    #[test]
    fn accepts_valid_agent() {
        let (m, _) = make_manager_and_store();
        let t = make_tool(m);
        assert!(
            t.validate_args(
                &json!({"to": {"relation": "agent", "thread_id": "t1"}, "message": "hi"})
            )
            .is_ok()
        );
    }

    // -- parent routing returns unavailable (no parent_thread_id yet) --

    #[tokio::test]
    async fn parent_without_thread_id_returns_unavailable() {
        let (manager, _store) = make_manager_and_store();
        let tool = make_tool(manager);

        // parent_run_id is set but parent_thread_id is None — routing fails.
        let mut ctx = make_ctx("thread-1", "child");
        ctx.run_identity = awaken_contract::contract::identity::RunIdentity::new(
            "thread-1".into(),
            None,
            "run-child".into(),
            Some("run-parent".into()), // parent_run_id exists
            "child".into(),
            awaken_contract::contract::identity::RunOrigin::Subagent,
        );

        let r = tool
            .execute(
                json!({"to": {"relation": "parent"}, "message": "hello parent"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(r.result.data["status"], "failed");
        assert!(
            r.result.data["error"]
                .as_str()
                .unwrap()
                .contains("recipient_unavailable")
        );
    }

    // -- agent routing includes agent_id in MailboxJob --

    #[tokio::test]
    async fn agent_routing_includes_agent_id() {
        let (manager, _store) = make_manager_and_store();
        let mailbox = Arc::new(InMemoryMailboxStore::new());
        let tool = SendMessageTool::new(manager, mailbox.clone());
        let ctx = make_ctx("thread-1", "sender");

        let r = tool
            .execute(
                json!({
                    "to": {"relation": "agent", "thread_id": "thread-target", "agent_id": "reviewer"},
                    "message": "please review"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(r.result.data["status"], "accepted");

        // Verify the mailbox job has the correct agent_id
        let jobs = mailbox
            .list_jobs("thread-target", None, 10, 0)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].agent_id, "reviewer");
    }
}
