//! Operator tool to inspect and recover dead-lettered durable messages.
//!
//! `send_message`'s durable routes dead-letter to [`FailedDurableMessageKey`]
//! after exhausting retries (or when no durable transport is configured). This
//! tool is the recovery surface for that list: `list` shows it, `replay`
//! re-enqueues it to the outbox so the dispatcher retries (e.g. after the sink
//! recovers), and `clear` discards it. It only reads the snapshot and emits
//! state mutations through the existing outbox/dead-letter reducers — it holds
//! no transport, so the same "no callable sink in feature code" invariant holds.

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_runtime_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

use crate::state::StateCommand;

use super::send_message_tool::{
    FailedDurableMessageKey, FailedDurableMessageUpdate, MessageOutboxKey, MessageOutboxUpdate,
    OutboxEntry, OutboxRoute,
};

pub const RECOVER_FAILED_MESSAGES_TOOL_ID: &str = "recover_failed_messages";

/// Tool that inspects and recovers the durable-message dead-letter list.
#[derive(Default)]
pub struct RecoverFailedMessagesTool;

impl RecoverFailedMessagesTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for RecoverFailedMessagesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            RECOVER_FAILED_MESSAGES_TOOL_ID,
            RECOVER_FAILED_MESSAGES_TOOL_ID,
            "Inspect and recover durable messages that failed delivery: list them, \
             replay (re-enqueue all for delivery), or clear them.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "replay", "clear"],
                    "description": "list (default): show dead-lettered messages; replay: re-enqueue all for delivery; clear: discard all"
                }
            }
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let failed = ctx
            .state::<FailedDurableMessageKey>()
            .cloned()
            .unwrap_or_default();

        match action {
            "list" => {
                let messages: Vec<Value> = failed
                    .messages
                    .iter()
                    .map(|m| {
                        json!({
                            "message_id": m.request.message_id,
                            "recipient_thread_id": m.request.recipient_thread_id,
                            "recipient_agent_id": m.request.recipient_agent_id,
                            "error": m.error,
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    RECOVER_FAILED_MESSAGES_TOOL_ID,
                    json!({ "action": "list", "count": messages.len(), "messages": messages }),
                )
                .into())
            }
            "replay" => {
                let count = failed.messages.len();
                let mut command = StateCommand::new();
                for message in failed.messages {
                    // Re-enqueue with a fresh retry budget; the dispatcher picks
                    // it up at the next StepEnd. Keeps the same message_id so the
                    // recipient still deduplicates.
                    command.update::<MessageOutboxKey>(MessageOutboxUpdate::Enqueue(OutboxEntry {
                        id: message.request.message_id.clone(),
                        route: OutboxRoute::Durable(message.request),
                        attempts: 0,
                    }));
                }
                command.update::<FailedDurableMessageKey>(FailedDurableMessageUpdate::Clear);
                Ok(ToolOutput::with_command(
                    ToolResult::success(
                        RECOVER_FAILED_MESSAGES_TOOL_ID,
                        json!({ "action": "replay", "count": count }),
                    ),
                    command,
                ))
            }
            "clear" => {
                let count = failed.messages.len();
                let mut command = StateCommand::new();
                command.update::<FailedDurableMessageKey>(FailedDurableMessageUpdate::Clear);
                Ok(ToolOutput::with_command(
                    ToolResult::success(
                        RECOVER_FAILED_MESSAGES_TOOL_ID,
                        json!({ "action": "clear", "count": count }),
                    ),
                    command,
                ))
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::background::{
        BackgroundTaskManager, BackgroundTaskPlugin, DurableMessageRequest, DurableMessageSink,
        FailedDurableMessage,
    };
    use crate::state::{StateKey, StateStore};
    use awaken_runtime_contract::contract::identity::{RunIdentity, RunOrigin};
    use awaken_runtime_contract::registry_spec::AgentSpec;
    use std::sync::Arc;

    #[derive(Default)]
    struct NoopSink;
    #[async_trait]
    impl DurableMessageSink for NoopSink {
        async fn send_agent_message(
            &self,
            _request: DurableMessageRequest,
        ) -> Result<String, String> {
            Ok("ok".into())
        }
    }

    /// Store with the messaging keys registered, pre-seeded with two dead-letters.
    fn store_with_dead_letters() -> StateStore {
        use crate::phase::ExecutionEnv;
        use crate::plugins::Plugin;
        let store = StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_store(store.clone());
        let sink: Arc<dyn DurableMessageSink> = Arc::new(NoopSink);
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::with_messaging(manager, sink));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        store.register_keys(&env.key_registrations).unwrap();

        let mut seed = StateCommand::new();
        for id in ["m1", "m2"] {
            seed.update::<FailedDurableMessageKey>(FailedDurableMessageUpdate::Push(
                FailedDurableMessage {
                    request: DurableMessageRequest {
                        message_id: id.into(),
                        recipient_thread_id: "thread-2".into(),
                        recipient_agent_id: None,
                        sender_agent_id: "sender".into(),
                        message: "hello".into(),
                    },
                    error: "sink down".into(),
                },
            ));
        }
        store.commit(seed.patch).unwrap();
        store
    }

    fn ctx(store: &StateStore) -> ToolCallContext {
        ToolCallContext {
            call_id: "call-1".into(),
            tool_name: RECOVER_FAILED_MESSAGES_TOOL_ID.into(),
            run_identity: RunIdentity::new(
                "thread-1".into(),
                None,
                "run-1".into(),
                None,
                "operator".into(),
                RunOrigin::User,
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

    #[tokio::test]
    async fn list_reports_dead_letters() {
        let store = store_with_dead_letters();
        let out = RecoverFailedMessagesTool::new()
            .execute(json!({"action": "list"}), &ctx(&store))
            .await
            .unwrap();
        assert_eq!(out.result.data["count"], 2);
        assert!(out.command.patch.is_empty(), "list is read-only");
    }

    #[tokio::test]
    async fn replay_re_enqueues_and_clears() {
        let store = store_with_dead_letters();
        let out = RecoverFailedMessagesTool::new()
            .execute(json!({"action": "replay"}), &ctx(&store))
            .await
            .unwrap();
        assert_eq!(out.result.data["count"], 2);
        store.commit(out.command.patch).unwrap();

        // Dead-letters cleared; both messages back in the outbox with their ids.
        assert!(
            store
                .read::<FailedDurableMessageKey>()
                .unwrap_or_default()
                .messages
                .is_empty()
        );
        let outbox = store.read::<MessageOutboxKey>().unwrap_or_default();
        assert_eq!(outbox.pending.len(), 2);
        let ids: Vec<&str> = outbox.pending.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"m1") && ids.contains(&"m2"));
    }

    #[tokio::test]
    async fn clear_discards_without_enqueue() {
        let store = store_with_dead_letters();
        let out = RecoverFailedMessagesTool::new()
            .execute(json!({"action": "clear"}), &ctx(&store))
            .await
            .unwrap();
        assert_eq!(out.result.data["count"], 2);
        store.commit(out.command.patch).unwrap();

        assert!(
            store
                .read::<FailedDurableMessageKey>()
                .unwrap_or_default()
                .messages
                .is_empty()
        );
        assert!(
            store
                .read::<MessageOutboxKey>()
                .unwrap_or_default()
                .pending
                .is_empty(),
            "clear must not re-enqueue"
        );
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let store = store_with_dead_letters();
        let err = RecoverFailedMessagesTool::new()
            .execute(json!({"action": "nope"}), &ctx(&store))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
