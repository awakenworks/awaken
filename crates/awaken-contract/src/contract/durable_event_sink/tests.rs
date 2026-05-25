use serde_json::json;

use super::*;
use crate::contract::commit_coordinator::CanonicalEventStager;
use crate::contract::event_sink::VecEventSink;
use crate::contract::identity::{RunIdentity, RunOrigin};
use crate::contract::suspension::{PendingToolCall, SuspendTicket, Suspension};
use crate::contract::tool::ToolResult;

#[derive(Default)]
struct RecordingStager {
    drafts: Mutex<Vec<CanonicalEventDraft>>,
}

impl CanonicalEventStager for RecordingStager {
    fn stage(&self, draft: CanonicalEventDraft) {
        self.drafts.lock().push(draft);
    }
}

fn run_context() -> AgentEventNormalizationContext {
    AgentEventNormalizationContext::new("t1", "r1", "server")
        .unwrap()
        .with_correlation_id("trace-1")
}

fn durable_sink(
    stager: Arc<RecordingStager>,
    inner: Arc<VecEventSink>,
    mode: RuntimeEventDurability,
) -> DurableEventSink {
    DurableEventSink::new(
        inner,
        stager,
        Arc::new(ScopedAgentEventNormalizer::new(run_context())),
        mode,
    )
}

#[tokio::test]
async fn compacted_mode_stages_committed_event_and_forwards_live_frame() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::ToolCallReady {
        id: "call-1".into(),
        name: "search".into(),
        arguments: json!({"q":"awaken"}),
    })
    .await;

    assert_eq!(stager.drafts.lock().len(), 1);
    assert_eq!(stager.drafts.lock()[0].event_kind.as_str(), "ToolCallReady");
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn normalizer_maps_suspended_tool_done_to_tool_call_suspended() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::ToolCallDone {
        id: "call-1".into(),
        message_id: "msg-1".into(),
        result: ToolResult::suspended("review", "approval required"),
        outcome: ToolCallOutcome::Suspended,
    })
    .await;

    assert_eq!(stager.drafts.lock().len(), 1);
    assert_eq!(
        stager.drafts.lock()[0].event_kind.as_str(),
        "ToolCallSuspended"
    );
    assert_eq!(stager.drafts.lock()[0].payload["outcome"], "suspended");
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn normalizer_records_permission_request_companion_event() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );
    let ticket = SuspendTicket::new(
        Suspension {
            id: "approval-1".into(),
            action: "approve_tool".into(),
            message: "Allow tool?".into(),
            parameters: json!({"tool": "review"}),
            response_schema: None,
        },
        PendingToolCall::new("call-1", "review", json!({"path": "/tmp/a"})),
        ToolCallResumeMode::ReplayToolCall,
    );

    sink.emit(AgentEvent::ToolCallDone {
        id: "call-1".into(),
        message_id: "msg-1".into(),
        result: ToolResult::suspended_with("review", "approval required", ticket),
        outcome: ToolCallOutcome::Suspended,
    })
    .await;

    let kinds = stager
        .drafts
        .lock()
        .iter()
        .map(|event| event.event_kind.as_str().to_string())
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["ToolCallSuspended", "ToolPermissionRequested"]);
    assert_eq!(
        stager.drafts.lock()[1].payload["result"]["suspension"]["suspension"]["id"],
        "approval-1"
    );
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn normalizer_classifies_lifecycle_facts_as_domain_events() {
    // ADR-0034 D11 lists ToolCallSuspended/Rejected/TimedOut/Cancelled
    // as DomainEvent, even though the source AgentEvent::ToolCallDone
    // and ::ToolCallCancel are CommittedRuntimeEvent.
    let normalizer = ScopedAgentEventNormalizer::new(run_context());

    let suspended = AgentEvent::ToolCallDone {
        id: "call-1".into(),
        message_id: "msg-1".into(),
        result: ToolResult::suspended("review", "approval required"),
        outcome: ToolCallOutcome::Suspended,
    };
    let rejected = AgentEvent::ToolCallDone {
        id: "call-2".into(),
        message_id: "msg-1".into(),
        result: ToolResult::error("shell", "permission denied").with_metadata("rejected", true),
        outcome: ToolCallOutcome::Failed,
    };
    let timed_out = AgentEvent::ToolCallDone {
        id: "call-3".into(),
        message_id: "msg-1".into(),
        result: ToolResult::error("mcp_search", "Timeout: 30s").with_metadata("timed_out", true),
        outcome: ToolCallOutcome::Failed,
    };
    let cancelled = AgentEvent::ToolCallCancel {
        id: "call-4".into(),
        name: "search".into(),
        reason: "user".into(),
    };
    let plain_done = AgentEvent::ToolCallDone {
        id: "call-5".into(),
        message_id: "msg-1".into(),
        result: ToolResult::success("search", json!({"ok": true})),
        outcome: ToolCallOutcome::Succeeded,
    };

    for (event, expected_kind, expected_fidelity) in [
        (suspended, "ToolCallSuspended", FidelityClass::DomainEvent),
        (rejected, "ToolCallRejected", FidelityClass::DomainEvent),
        (timed_out, "ToolCallTimedOut", FidelityClass::DomainEvent),
        (cancelled, "ToolCallCancelled", FidelityClass::DomainEvent),
        (
            plain_done,
            "ToolCallDone",
            FidelityClass::CommittedRuntimeEvent,
        ),
    ] {
        let normalized = normalizer.normalize(&event).unwrap().unwrap();
        assert_eq!(normalized.draft.event_kind.as_str(), expected_kind);
        assert_eq!(normalized.fidelity, expected_fidelity, "{expected_kind}");
    }
}

#[tokio::test]
async fn normalizer_maps_intercept_block_failures_to_tool_call_rejected() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::ToolCallDone {
        id: "call-1".into(),
        message_id: "msg-1".into(),
        result: ToolResult::error("shell", "permission denied").with_metadata("rejected", true),
        outcome: ToolCallOutcome::Failed,
    })
    .await;

    assert_eq!(stager.drafts.lock().len(), 1);
    assert_eq!(
        stager.drafts.lock()[0].event_kind.as_str(),
        "ToolCallRejected"
    );
    assert_eq!(
        stager.drafts.lock()[0].payload["result"]["metadata"]["rejected"],
        true
    );
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn normalizer_maps_timeout_failures_to_tool_call_timed_out() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::ToolCallDone {
        id: "call-1".into(),
        message_id: "msg-1".into(),
        result: ToolResult::error("mcp_search", "Timeout: 30s").with_metadata("timed_out", true),
        outcome: ToolCallOutcome::Failed,
    })
    .await;

    assert_eq!(stager.drafts.lock().len(), 1);
    assert_eq!(
        stager.drafts.lock()[0].event_kind.as_str(),
        "ToolCallTimedOut"
    );
    assert_eq!(
        stager.drafts.lock()[0].payload["result"]["metadata"]["timed_out"],
        true
    );
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn compacted_mode_skips_observed_event_but_forwards_live_frame() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::TextDelta { delta: "hi".into() })
        .await;

    assert!(stager.drafts.lock().is_empty());
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn full_fidelity_persists_observed_event() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::FullFidelity,
    );

    sink.emit(AgentEvent::TextDelta { delta: "hi".into() })
        .await;

    assert_eq!(stager.drafts.lock().len(), 1);
    assert_eq!(
        stager.drafts.lock()[0].event_kind.as_str(),
        "TextDeltaObserved"
    );
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn compacted_mode_persists_context_compaction_started_from_state_snapshot() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "revision": 7,
            "extensions": {
                "__context_compaction": {
                    "boundaries": [],
                    "total_compactions": 0,
                    "in_flight": {
                        "task_id": "bg_1",
                        "boundary_message_id": "msg_1",
                        "started_at_ms": 1234
                    }
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionStarted");
    assert_eq!(drafts[0].visibility, EventVisibility::Internal);
    assert_eq!(drafts[0].payload["task_id"], "bg_1");
    assert_eq!(drafts[0].payload["boundary_message_id"], "msg_1");
    assert!(drafts[0].scopes.contains(&EventScope::thread("t1")));
    assert!(!drafts[0].scopes.contains(&EventScope::run("r1")));
    assert_eq!(inner.events().len(), 1);
    drop(drafts);
}

#[tokio::test]
async fn compacted_mode_persists_context_compaction_completed_without_summary_payload() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );
    let snapshot = json!({
        "revision": 8,
        "extensions": {
            "__context_compaction": {
                "boundaries": [{
                    "summary": "sensitive full summary",
                    "task_id": "bg_1",
                    "boundary_message_id": "msg_1",
                    "pre_tokens": 9000,
                    "post_tokens": 300,
                    "timestamp_ms": 5678
                }],
                "total_compactions": 1
            }
        }
    });

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: snapshot.clone(),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot { snapshot }).await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 1, "normalizer must not duplicate snapshots");
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionCompleted");
    assert_eq!(drafts[0].visibility, EventVisibility::Internal);
    assert_eq!(drafts[0].payload["pre_tokens"], 9000);
    assert_eq!(drafts[0].payload["post_tokens"], 300);
    assert_eq!(drafts[0].payload["timestamp_ms"], 5678);
    assert_eq!(drafts[0].payload["compaction_ordinal"], 1);
    assert_eq!(drafts[0].payload["task_id"], "bg_1");
    assert_eq!(drafts[0].payload["boundary_message_id"], "msg_1");
    let hash = drafts[0]
        .payload
        .get("summary_hash")
        .and_then(serde_json::Value::as_str)
        .expect("summary_hash is present");
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert!(drafts[0].payload.get("summary").is_none());
    assert_eq!(inner.events().len(), 2);
}

#[tokio::test]
async fn compacted_mode_summary_hash_keeps_distinct_completed_boundaries() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [
                        {
                            "summary": "first summary",
                            "task_id": "bg_1",
                            "boundary_message_id": "msg_1",
                            "pre_tokens": 100,
                            "post_tokens": 10,
                            "timestamp_ms": 42
                        },
                        {
                            "summary": "second summary",
                            "task_id": "bg_2",
                            "boundary_message_id": "msg_2",
                            "pre_tokens": 100,
                            "post_tokens": 10,
                            "timestamp_ms": 42
                        }
                    ],
                    "total_compactions": 2
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 2);
    assert_ne!(
        drafts[0].payload["summary_hash"],
        drafts[1].payload["summary_hash"]
    );
    assert!(drafts[0].payload.get("summary").is_none());
    assert!(drafts[1].payload.get("summary").is_none());
}

#[tokio::test]
async fn compacted_mode_persists_context_compaction_cancelled_when_in_flight_clears() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [],
                    "total_compactions": 0,
                    "in_flight": {
                        "task_id": "bg_cancel",
                        "boundary_message_id": "msg_cancel",
                        "started_at_ms": 10
                    }
                }
            }
        }),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [],
                    "total_compactions": 0,
                    "timestamp_ms": 20
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionStarted");
    assert_eq!(drafts[1].event_kind.as_str(), "ContextCompactionCancelled");
    assert_eq!(drafts[1].payload["task_id"], "bg_cancel");
    assert_eq!(drafts[1].payload["boundary_message_id"], "msg_cancel");
    assert_eq!(drafts[1].payload["cancelled_at_ms"], 20);
}

#[tokio::test]
async fn compacted_mode_does_not_cancel_when_previous_in_flight_completes() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [],
                    "total_compactions": 0,
                    "in_flight": {
                        "task_id": "bg_done",
                        "boundary_message_id": "msg_done"
                    }
                }
            }
        }),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [{
                        "summary": "finished",
                        "task_id": "bg_done",
                        "boundary_message_id": "msg_done",
                        "timestamp_ms": 30
                    }],
                    "total_compactions": 1
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionStarted");
    assert_eq!(drafts[1].event_kind.as_str(), "ContextCompactionCompleted");
    assert!(
        drafts
            .iter()
            .all(|draft| draft.event_kind.as_str() != "ContextCompactionCancelled")
    );
}

#[tokio::test]
async fn compacted_mode_does_not_treat_same_boundary_different_task_as_terminal() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [],
                    "total_compactions": 0,
                    "in_flight": {
                        "task_id": "old_task",
                        "boundary_message_id": "shared_boundary"
                    }
                }
            }
        }),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [{
                        "summary": "different task completed",
                        "task_id": "new_task",
                        "boundary_message_id": "shared_boundary",
                        "timestamp_ms": 30
                    }],
                    "total_compactions": 1,
                    "timestamp_ms": 40
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 3);
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionStarted");
    assert_eq!(drafts[1].event_kind.as_str(), "ContextCompactionCompleted");
    assert_eq!(drafts[2].event_kind.as_str(), "ContextCompactionCancelled");
    assert_eq!(drafts[2].payload["task_id"], "old_task");
    assert_eq!(drafts[2].payload["boundary_message_id"], "shared_boundary");
}

#[tokio::test]
async fn compacted_mode_reset_allows_same_task_to_start_again() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    let in_flight = json!({
        "extensions": {
            "__context_compaction": {
                "boundaries": [],
                "total_compactions": 2,
                "in_flight": {
                    "task_id": "bg_reset",
                    "boundary_message_id": "msg_reset"
                }
            }
        }
    });
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: in_flight.clone(),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "boundaries": [],
                    "total_compactions": 0
                }
            }
        }),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: in_flight,
    })
    .await;

    let drafts = stager.drafts.lock();
    let started = drafts
        .iter()
        .filter(|draft| draft.event_kind.as_str() == "ContextCompactionStarted")
        .count();
    assert_eq!(started, 2);
}

#[tokio::test]
async fn compacted_mode_persists_context_compaction_failed_from_state_snapshot() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "revision": 9,
            "extensions": {
                "__context_compaction": {
                    "boundaries": [],
                    "failures": [{
                        "task_id": "bg_failed",
                        "boundary_message_id": "msg_failed",
                        "error": "summarizer failed",
                        "timestamp_ms": 6789
                    }],
                    "total_compactions": 0
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionFailed");
    assert_eq!(drafts[0].visibility, EventVisibility::Internal);
    assert_eq!(drafts[0].payload["task_id"], "bg_failed");
    assert_eq!(drafts[0].payload["boundary_message_id"], "msg_failed");
    assert_eq!(drafts[0].payload["error"], "summarizer failed");
    assert_eq!(drafts[0].payload["timestamp_ms"], 6789);
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn compacted_mode_persists_context_compaction_skipped_from_state_snapshot() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "revision": 10,
            "extensions": {
                "__context_compaction": {
                    "skipped": [{
                        "task_id": "bg_skipped",
                        "boundary_message_id": "msg_skipped",
                        "reason": "min_savings_ratio",
                        "pre_tokens": 4000,
                        "post_tokens": 3900,
                        "savings_ratio_ppm": 25000,
                        "min_savings_ratio_ppm": 300000,
                        "timestamp_ms": 7890
                    }],
                    "total_compactions": 0
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionSkipped");
    assert_eq!(drafts[0].visibility, EventVisibility::Internal);
    assert_eq!(drafts[0].payload["task_id"], "bg_skipped");
    assert_eq!(drafts[0].payload["boundary_message_id"], "msg_skipped");
    assert_eq!(drafts[0].payload["reason"], "min_savings_ratio");
    assert_eq!(drafts[0].payload["savings_ratio_ppm"], 25000);
    assert_eq!(drafts[0].payload["min_savings_ratio_ppm"], 300000);
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn compacted_mode_dedupes_skipped_by_stable_identity_not_metrics_or_index() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "skipped": [{
                        "task_id": "bg_skipped",
                        "boundary_message_id": "msg_skipped",
                        "reason": "min_savings_ratio",
                        "pre_tokens": 4000
                    }]
                }
            }
        }),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "skipped": [
                        {
                            "task_id": "other",
                            "boundary_message_id": "other_msg",
                            "reason": "cooldown"
                        },
                        {
                            "task_id": "bg_skipped",
                            "boundary_message_id": "msg_skipped",
                            "reason": "min_savings_ratio",
                            "pre_tokens": 4100,
                            "post_tokens": 3900
                        }
                    ]
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    let skipped = drafts
        .iter()
        .filter(|draft| draft.event_kind.as_str() == "ContextCompactionSkipped")
        .collect::<Vec<_>>();
    assert_eq!(skipped.len(), 2);
    assert_eq!(skipped[0].payload["task_id"], "bg_skipped");
    assert_eq!(skipped[1].payload["task_id"], "other");
}

#[tokio::test]
async fn compacted_mode_omits_absent_optional_skipped_fields() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "skipped": [{ "skip_id": "skip-1", "reason": "cooldown" }]
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].payload["skip_id"], "skip-1");
    assert!(drafts[0].payload.get("boundary_message_id").is_none());
    assert!(drafts[0].payload.get("timestamp_ms").is_none());
}

#[tokio::test]
async fn compacted_mode_does_not_cancel_when_previous_in_flight_is_skipped() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "total_compactions": 0,
                    "in_flight": {
                        "task_id": "bg_skip",
                        "boundary_message_id": "msg_skip"
                    }
                }
            }
        }),
    })
    .await;
    sink.emit(AgentEvent::StateSnapshot {
        snapshot: json!({
            "extensions": {
                "__context_compaction": {
                    "skipped": [{
                        "task_id": "bg_skip",
                        "boundary_message_id": "msg_skip",
                        "reason": "min_savings_ratio",
                        "timestamp_ms": 22
                    }],
                    "total_compactions": 0,
                    "timestamp_ms": 23
                }
            }
        }),
    })
    .await;

    let drafts = stager.drafts.lock();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].event_kind.as_str(), "ContextCompactionStarted");
    assert_eq!(drafts[1].event_kind.as_str(), "ContextCompactionSkipped");
    assert!(
        drafts
            .iter()
            .all(|draft| draft.event_kind.as_str() != "ContextCompactionCancelled")
    );
}

#[tokio::test]
async fn disabled_mode_only_forwards_inner_sink() {
    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = durable_sink(
        stager.clone(),
        inner.clone(),
        RuntimeEventDurability::Disabled,
    );

    sink.emit(AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
        identity: None,
    })
    .await;

    assert!(stager.drafts.lock().is_empty());
    assert_eq!(inner.events().len(), 1);
}

#[tokio::test]
async fn normalizer_failure_forwards_live_without_staging() {
    struct FailingNormalizer;

    impl AgentEventNormalizer for FailingNormalizer {
        fn normalize(
            &self,
            _event: &AgentEvent,
        ) -> Result<Option<NormalizedCanonicalEvent>, EventStoreError> {
            Err(EventStoreError::Validation("forced".to_string()))
        }
    }

    let stager = Arc::new(RecordingStager::default());
    let inner = Arc::new(VecEventSink::new());
    let sink = DurableEventSink::new(
        inner.clone(),
        stager.clone(),
        Arc::new(FailingNormalizer),
        RuntimeEventDurability::Compacted,
    );

    sink.emit(AgentEvent::ToolCallReady {
        id: "call-1".into(),
        name: "search".into(),
        arguments: json!({}),
    })
    .await;

    assert!(stager.drafts.lock().is_empty());
    assert_eq!(inner.events().len(), 1);
}

#[test]
fn normalizer_maps_run_lifecycle_without_duplicate_untyped_facts() {
    let normalizer = ScopedAgentEventNormalizer::new(run_context());
    let first = normalizer
        .normalize(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
            identity: None,
        })
        .unwrap()
        .unwrap();
    let resumed = normalizer
        .normalize(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
            identity: None,
        })
        .unwrap()
        .unwrap();
    let finished = normalizer
        .normalize(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            identity: None,
            result: None,
            termination: TerminationReason::NaturalEnd,
        })
        .unwrap()
        .unwrap();
    let duplicate_terminal = normalizer
        .normalize(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            identity: None,
            result: None,
            termination: TerminationReason::NaturalEnd,
        })
        .unwrap();

    assert_eq!(first.draft.event_kind.as_str(), "RunStarted");
    assert_eq!(resumed.draft.event_kind.as_str(), "RunResumed");
    assert_eq!(finished.draft.event_kind.as_str(), "RunFinished");
    assert!(duplicate_terminal.is_none());
}

#[test]
fn resumed_normalizer_maps_first_run_start_to_run_resumed() {
    let normalizer = ScopedAgentEventNormalizer::new_resumed(run_context());
    let first = normalizer
        .normalize(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
            identity: None,
        })
        .unwrap()
        .unwrap();

    assert_eq!(first.draft.event_kind.as_str(), "RunResumed");
}

#[test]
fn normalizer_indexes_context_events_into_thread_and_run_scopes() {
    let normalizer = ScopedAgentEventNormalizer::new(run_context());
    let normalized = normalizer
        .normalize(&AgentEvent::Error {
            message: "boom".into(),
            code: Some("E_TEST".into()),
        })
        .unwrap()
        .unwrap();

    assert_eq!(normalized.draft.event_kind.as_str(), "ErrorRecorded");
    assert!(normalized.draft.scopes.contains(&EventScope::thread("t1")));
    assert!(normalized.draft.scopes.contains(&EventScope::run("r1")));
    assert_eq!(normalized.draft.correlation_id.as_deref(), Some("trace-1"));
}

#[test]
fn normalizer_does_not_create_aggregate_scope_from_parent_thread() {
    let context = AgentEventNormalizationContext::new("child-thread", "run-child", "server")
        .unwrap()
        .with_correlation_id("trace-child");
    let normalizer = ScopedAgentEventNormalizer::new(context);
    let identity = RunIdentity::new(
        "child-thread".to_string(),
        Some(" parent-thread ".to_string()),
        "run-child".to_string(),
        None,
        "agent-a".to_string(),
        RunOrigin::Subagent,
    );
    let started = normalizer
        .normalize(&AgentEvent::RunStart {
            thread_id: "child-thread".into(),
            run_id: "run-child".into(),
            parent_run_id: None,
            identity: Some(identity),
        })
        .unwrap()
        .unwrap();

    assert_eq!(started.draft.scopes.len(), 2);
    assert!(
        started
            .draft
            .scopes
            .contains(&EventScope::thread("child-thread"))
    );
    assert!(started.draft.scopes.contains(&EventScope::run("run-child")));

    let ready = normalizer
        .normalize(&AgentEvent::ToolCallReady {
            id: "call-1".into(),
            name: "lookup".into(),
            arguments: json!({}),
        })
        .unwrap()
        .unwrap();
    assert_eq!(ready.draft.scopes.len(), 2);
    assert!(
        ready
            .draft
            .scopes
            .contains(&EventScope::thread("child-thread"))
    );
    assert!(ready.draft.scopes.contains(&EventScope::run("run-child")));
}
