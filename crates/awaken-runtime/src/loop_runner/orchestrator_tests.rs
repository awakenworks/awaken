use super::*;

#[test]
fn latest_run_response_uses_current_run_non_tool_output() {
    let messages = vec![
        std::sync::Arc::new(Message::user("current request")),
        std::sync::Arc::new(Message::assistant("previous context response")),
        std::sync::Arc::new(Message::assistant("checking")),
        std::sync::Arc::new(Message::tool("call-1", "tool result")),
        std::sync::Arc::new(Message::assistant("final answer")),
        std::sync::Arc::new(Message::tool("call-2", "late tool result")),
    ];

    assert_eq!(latest_run_response(&messages, 2), "final answer");
    assert_eq!(latest_run_response(&messages, messages.len()), "");
}

mod pending_boundary_tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct RecordingBoundaryHandler {
        boundaries: Mutex<Vec<DeliveryBoundary>>,
        messages: Vec<Message>,
    }

    #[async_trait]
    impl PendingBoundaryHandler for RecordingBoundaryHandler {
        async fn stage_pending_messages(
            &self,
            boundary: DeliveryBoundary,
            messages: Vec<Message>,
        ) -> Result<(), AgentLoopError> {
            self.boundaries.lock().unwrap().push(boundary);
            assert!(messages.is_empty());
            Ok(())
        }

        async fn freeze_pending_boundary(
            &self,
            boundary: DeliveryBoundary,
        ) -> Result<Option<crate::loop_runner::PendingBoundaryFreeze>, AgentLoopError> {
            self.boundaries.lock().unwrap().push(boundary);
            Ok(Some(crate::loop_runner::PendingBoundaryFreeze {
                messages: self.messages.clone(),
            }))
        }
    }

    struct StagingBoundaryHandler {
        staged_boundaries: Mutex<Vec<DeliveryBoundary>>,
        staged_messages: Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl PendingBoundaryHandler for StagingBoundaryHandler {
        async fn stage_pending_messages(
            &self,
            boundary: DeliveryBoundary,
            messages: Vec<Message>,
        ) -> Result<(), AgentLoopError> {
            self.staged_boundaries.lock().unwrap().push(boundary);
            self.staged_messages.lock().unwrap().extend(messages);
            Ok(())
        }

        async fn freeze_pending_boundary(
            &self,
            boundary: DeliveryBoundary,
        ) -> Result<Option<crate::loop_runner::PendingBoundaryFreeze>, AgentLoopError> {
            self.staged_boundaries.lock().unwrap().push(boundary);
            Ok(Some(crate::loop_runner::PendingBoundaryFreeze {
                messages: self.staged_messages.lock().unwrap().clone(),
            }))
        }
    }

    #[tokio::test]
    async fn apply_pending_boundary_appends_frozen_messages() {
        let handler = std::sync::Arc::new(RecordingBoundaryHandler {
            boundaries: Mutex::new(Vec::new()),
            messages: vec![Message::user("pending")],
        });
        let handler: std::sync::Arc<dyn PendingBoundaryHandler> = handler;
        let mut messages = vec![std::sync::Arc::new(Message::user("original"))];

        let appended =
            apply_pending_boundary(Some(&handler), DeliveryBoundary::NextStep, &mut messages)
                .await
                .unwrap();

        assert!(appended);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "pending");
    }

    #[tokio::test]
    async fn apply_pending_boundary_without_handler_is_noop() {
        let mut messages = vec![std::sync::Arc::new(Message::user("original"))];

        let appended = apply_pending_boundary(None, DeliveryBoundary::OnNaturalEnd, &mut messages)
            .await
            .unwrap();

        assert!(!appended);
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn inbox_payloads_stage_to_pending_before_freeze_when_handler_exists() {
        let handler = std::sync::Arc::new(StagingBoundaryHandler {
            staged_boundaries: Mutex::new(Vec::new()),
            staged_messages: Mutex::new(Vec::new()),
        });
        let handler: std::sync::Arc<dyn PendingBoundaryHandler> = handler;
        let mut messages = vec![std::sync::Arc::new(Message::user("original"))];
        let payload = crate::inbox::inbox_messages_payload(vec![Message::user("live")]);
        let store = StateStore::new();

        let appended = apply_inbox_payloads_at_boundary(
            Some(&handler),
            DeliveryBoundary::NextStep,
            &mut messages,
            vec![payload],
            &store,
        )
        .await
        .unwrap();

        assert!(appended);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "live");
    }

    #[tokio::test]
    async fn inbox_payloads_append_directly_without_pending_handler() {
        let mut messages = vec![std::sync::Arc::new(Message::user("original"))];
        let payload = crate::inbox::inbox_messages_payload(vec![Message::user("live")]);
        let store = StateStore::new();

        let appended = apply_inbox_payloads_at_boundary(
            None,
            DeliveryBoundary::NextStep,
            &mut messages,
            vec![payload],
            &store,
        )
        .await
        .unwrap();

        assert!(appended);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "live");
    }
}

mod pending_work_tests {
    use super::*;
    use crate::agent::state::PendingWorkKey;

    fn store_with_loop_state() -> StateStore {
        let store = StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .unwrap();
        store
    }

    #[test]
    fn default_no_pending_work() {
        let store = store_with_loop_state();
        assert!(!has_pending_work(&store));
    }

    #[test]
    fn pending_work_set_true() {
        let store = store_with_loop_state();
        let mut batch = MutationBatch::new();
        batch.update::<PendingWorkKey>(true);
        store.commit(batch).unwrap();
        assert!(has_pending_work(&store));
    }

    #[test]
    fn pending_work_cleared() {
        let store = store_with_loop_state();
        let mut batch = MutationBatch::new();
        batch.update::<PendingWorkKey>(true);
        store.commit(batch).unwrap();
        assert!(has_pending_work(&store));

        let mut batch2 = MutationBatch::new();
        batch2.update::<PendingWorkKey>(false);
        store.commit(batch2).unwrap();
        assert!(!has_pending_work(&store));
    }
}

mod check_termination_tests {
    use super::*;
    use crate::agent::state::{RunLifecycle, RunLifecycleUpdate};
    use crate::loop_runner::checkpoint::check_termination;
    use awaken_contract::contract::lifecycle::TerminationReason;

    fn store_with_lifecycle() -> StateStore {
        let store = StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .unwrap();
        store
    }

    #[test]
    fn running_returns_none() {
        let store = store_with_lifecycle();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        assert!(check_termination(&store).is_none());
    }

    #[test]
    fn done_returns_termination_reason() {
        let store = store_with_lifecycle();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Done {
                done_reason: "natural".into(),
                updated_at: 200,
            },
        )
        .unwrap();
        assert!(matches!(
            check_termination(&store),
            Some(TerminationReason::NaturalEnd)
        ));
    }

    #[test]
    fn waiting_suspended_returns_suspended() {
        let store = store_with_lifecycle();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 200,
                pause_reason: "suspended".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            check_termination(&store),
            Some(TerminationReason::Suspended)
        ));
    }

    #[test]
    fn waiting_awaiting_tasks_returns_none() {
        let store = store_with_lifecycle();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 200,
                pause_reason: "awaiting_tasks".into(),
            },
        )
        .unwrap();
        // awaiting_tasks is handled by orchestrator, not check_termination
        assert!(
            check_termination(&store).is_none(),
            "awaiting_tasks should return None"
        );
    }
}

mod termination_sequence_tests {
    use super::*;
    use crate::agent::state::{RunLifecycle, RunLifecycleUpdate};
    fn store_with_lifecycle() -> StateStore {
        let store = StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .unwrap();
        store
    }

    #[test]
    fn waiting_state_not_overwritten_by_done() {
        let store = store_with_lifecycle();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        // Simulate orchestrator setting Waiting before break
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 200,
                pause_reason: "awaiting_tasks".into(),
            },
        )
        .unwrap();

        // Termination sequence: should NOT overwrite Waiting with Done
        let lifecycle_now = store.read::<RunLifecycle>().map(|s| s.status);
        let termination = TerminationReason::NaturalEnd;
        let (target_status, _) = termination.to_run_status();
        if target_status.is_terminal() && lifecycle_now != Some(RunStatus::Waiting) {
            panic!("should not reach here — lifecycle is Waiting");
        }
        // Verify state is still Waiting
        let state = store.read::<RunLifecycle>().unwrap();
        assert_eq!(state.status, RunStatus::Waiting);
        assert_eq!(state.status_reason.as_deref(), Some("awaiting_tasks"));
    }
}

mod persist_checkpoint_tests {
    use super::*;
    use crate::agent::state::{RunLifecycle, RunLifecycleUpdate};
    use awaken_contract::model::{
        PendingScheduledActions, Phase, ScheduledAction, ScheduledActionEnvelope,
        ScheduledActionQueueUpdate,
    };

    fn store_with_lifecycle() -> StateStore {
        let store = StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .unwrap();
        store
    }

    #[test]
    fn lifecycle_stores_status_reason_for_waiting() {
        let store = store_with_lifecycle();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 200,
                pause_reason: "awaiting_tasks".into(),
            },
        )
        .unwrap();

        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status_reason.as_deref(), Some("awaiting_tasks"));
    }

    #[test]
    fn lifecycle_stores_status_reason_for_done() {
        let store = store_with_lifecycle();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Done {
                done_reason: "natural".into(),
                updated_at: 200,
            },
        )
        .unwrap();

        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status_reason.as_deref(), Some("natural"));
    }

    #[test]
    fn terminal_run_clears_pending_scheduled_actions() {
        let store = store_with_lifecycle();
        let _runtime = crate::phase::PhaseRuntime::new(store.clone()).unwrap();
        let mut batch = MutationBatch::new();
        batch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Push(
            ScheduledActionEnvelope {
                id: 42,
                action: ScheduledAction::new(
                    Phase::AfterInference,
                    "test.stale.action",
                    serde_json::json!({"run_id": "r1", "message_id": "missing-message"}),
                ),
            },
        ));
        store.commit(batch).unwrap();
        assert_eq!(store.read::<PendingScheduledActions>().unwrap().len(), 1);

        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        )
        .unwrap();
        crate::loop_runner::commit_update::<RunLifecycle>(
            &store,
            RunLifecycleUpdate::Done {
                done_reason: "natural".into(),
                updated_at: 200,
            },
        )
        .unwrap();

        assert!(
            store.read::<PendingScheduledActions>().unwrap().is_empty(),
            "terminal runs must not leave stale scheduled actions for later dispatches"
        );
    }
}

mod inbox_drain_tests {
    use crate::inbox::inbox_channel;

    #[test]
    fn drain_returns_empty_when_no_messages() {
        let (_tx, mut rx) = inbox_channel();
        let msgs = rx.drain();
        assert!(msgs.is_empty());
    }

    #[test]
    fn drain_returns_all_pending_messages() {
        let (tx, mut rx) = inbox_channel();
        tx.send(serde_json::json!({"event": "a"}));
        tx.send(serde_json::json!({"event": "b"}));
        tx.send(serde_json::json!({"event": "c"}));

        let msgs = rx.drain();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["event"], "a");
        assert_eq!(msgs[2]["event"], "c");

        // Second drain is empty
        assert!(rx.drain().is_empty());
    }

    #[test]
    fn drain_after_sender_drop_returns_buffered() {
        let (tx, mut rx) = inbox_channel();
        tx.send(serde_json::json!("buffered"));
        drop(tx);

        let msgs = rx.drain();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "buffered");
    }

    #[test]
    fn inbox_events_injected_as_internal_user_messages() {
        let (tx, mut rx) = inbox_channel();
        tx.send(serde_json::json!({"kind": "custom", "event_type": "progress", "task_id": "bg_0"}));

        let msgs = rx.drain();
        for msg in &msgs {
            let m = crate::inbox::inbox_event_message(msg);
            assert_eq!(m.role, awaken_contract::contract::message::Role::User);
            assert_eq!(
                m.visibility,
                awaken_contract::contract::message::Visibility::Internal
            );
        }
    }

    #[test]
    fn inbox_events_wrapped_in_background_task_event_tag() {
        let event = serde_json::json!({
            "kind": "custom",
            "task_id": "bg_42",
            "event_type": "data_ready",
            "payload": {"rows": 100}
        });
        let m = crate::inbox::inbox_event_message(&event);
        let text = m.text();
        assert!(
            text.contains("<background-task-event"),
            "should have opening tag: {text}"
        );
        assert!(
            text.contains("</background-task-event>"),
            "should have closing tag: {text}"
        );
        assert!(
            text.contains("kind=\"custom\""),
            "tag should contain kind: {text}"
        );
        assert!(
            text.contains("task_id=\"bg_42\""),
            "tag should contain task_id: {text}"
        );
    }
}
