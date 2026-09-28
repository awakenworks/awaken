//! Native collection qualification and the proved empty terminal boundary.
use super::*;

fn command(kind: u8, second: bool) -> SessionEventCommand {
    let operation_id = if second { "b" } else { "a" }.to_owned();
    match kind {
        0 => SessionEventCommand::UserMessage {
            operation_id,
            run_id: RunId("r".into()),
            content: vec![ContentBlock::text("p")],
            data_subject_id: Some("d".into()),
        },
        1 => SessionEventCommand::SystemMessage {
            operation_id,
            content: vec![ContentBlock::text("s")],
        },
        2 => SessionEventCommand::DefineOutcome {
            operation_id,
            outcome_id: "o".into(),
            description: "d".into(),
            rubric: SessionOutcomeRubric::Text {
                content: "r".into(),
            },
            max_iterations: Some(2),
        },
        3 => SessionEventCommand::ToolReply {
            operation_id,
            reply: SessionEventToolReply {
                tool_request_event_id: "e".into(),
                target: SessionThreadTarget::Primary,
                expected_run_id: RunId("r".into()),
                expected_correlation_id: "c".into(),
                expected_thread_version: Some(3),
                answered_pending_commit_cursor: Some(4),
                runtime_tool_use_id: "t".into(),
                reply: SessionEventToolReplyKind::Confirmation {
                    allow: false,
                    deny_message: Some("n".into()),
                },
            },
        },
        4 => SessionEventCommand::Interrupt {
            operation_id,
            interrupt: SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            },
        },
        _ => unreachable!("the finite fixture contains the five command variants"),
    }
}

fn batch(events: Vec<SessionEventEntry>) -> SessionEventBatch {
    SessionEventBatch {
        batch_id: "b".into(),
        events,
        admitted_revision: crate::SessionRevision(1),
        admission_projection_anchor: Some(SessionEventProjectionAnchor {
            source_commit_cursor: 2,
        }),
        idempotency_key: Some("k".into()),
        request_fingerprint: Some("f".into()),
        admission_closed: false,
        traceparent: Some("t".into()),
        wake_activity_epoch: Some(3),
    }
}

/// Cause/effect table, shared by native and symbolic calls to the real method:
/// R1 processed => preserve all entry fields, including legacy unknown time;
/// R2 pending + existing anchor => complete at that anchor and supplied time;
/// R3 pending + absent anchor => complete at optional terminal anchor/time;
/// R4 all rows => preserve complete commands, order and every batch header;
/// R5 count == original pending cardinality, then every entry is complete;
/// R6 a second call with independent anchor/time => zero and full-object stutter.
/// Empty batches are R5/R6 witnesses. No admission-validity assumption is used:
/// the method must also retain unrelated header fields on legacy input states.
fn check_resolution(
    mut value: SessionEventBatch,
    terminal_anchor: Option<SessionEventProjectionAnchor>,
    now: u64,
    replay_anchor: Option<SessionEventProjectionAnchor>,
    replay_time: u64,
) {
    let before = value.clone();
    let count = value.resolve_terminally(terminal_anchor, now);
    let mut expected_count = 0;
    assert_eq!(
        value.events.len(),
        before.events.len(),
        "B4 event cardinality"
    );
    for (actual, original) in value.events.iter().zip(&before.events) {
        assert!(
            actual.event == original.event,
            "B4 complete command and order"
        );
        assert_eq!(
            actual.dispatch_settlement, original.dispatch_settlement,
            "B7 retained dispatch receipt"
        );
        if original.processed {
            assert!(actual == original, "B1 completed entry is immutable");
        } else {
            expected_count += 1;
            assert!(actual.processed, "B2 pending entry completes");
            assert_eq!(actual.processed_at_unix_ms, Some(now), "B2 first time");
            let expected_anchor = match original.projection_anchor {
                Some(anchor) => Some(anchor),
                None => terminal_anchor,
            };
            assert_eq!(
                actual.projection_anchor, expected_anchor,
                "B3 anchor precedence"
            );
        }
    }
    assert_eq!(count, expected_count, "B5 exact pending count");
    assert!(value.is_complete(), "B5 complete batch");
    // Restore only the three permitted per-entry changes in an independent
    // comparison image; derived full-object equality includes every header,
    // command variant and nested payload, including future added fields.
    let mut frame = value.clone();
    for (entry, original) in frame.events.iter_mut().zip(&before.events) {
        entry.processed = original.processed;
        entry.processed_at_unix_ms = original.processed_at_unix_ms;
        entry.projection_anchor = original.projection_anchor;
    }
    assert!(frame == before, "B4 complete immutable frame");
    let settled = value.clone();
    assert_eq!(
        value.resolve_terminally(replay_anchor, replay_time),
        0,
        "B6 replay count"
    );
    assert!(value == settled, "B6 complete replay frame");
}

#[cfg(kani)]
/// @obligation session_time.processed_event_retains_first_time_and_rejects_atomically
#[kani::proof]
#[kani::unwind(4)]
fn terminal_batch_resolution_empty() {
    // This proved partition has exactly zero real entries. Full u64 numeric
    // header metadata, optional header/call anchors, both clocks and the closed-
    // admission flag remain symbolic; all immutable fields and replay are checked.
    // Real String/Vec/Clone/Eq/Drop and default checks remain. Nonempty full-
    // object probes did not complete: their 30-partition domain is not proved
    // by this empty boundary. The native table below exercises all five command
    // variants and every occupied-slot marker-presence combination separately.
    let mut value = batch(Vec::new());
    value.admitted_revision = crate::SessionRevision(kani::any());
    value.admission_projection_anchor = kani::any::<bool>().then(|| SessionEventProjectionAnchor {
        source_commit_cursor: kani::any(),
    });
    value.admission_closed = kani::any();
    value.wake_activity_epoch = kani::any::<bool>().then(kani::any::<u64>);
    let anchor = kani::any::<bool>().then(|| SessionEventProjectionAnchor {
        source_commit_cursor: kani::any(),
    });
    let replay_anchor = kani::any::<bool>().then(|| SessionEventProjectionAnchor {
        source_commit_cursor: kani::any(),
    });
    check_resolution(value, anchor, kani::any(), replay_anchor, kani::any());
}

#[cfg(test)]
#[test]
fn terminal_batch_collection_decision_table_preserves_frame_and_replay() {
    // Exhaust length x command x processed/time/anchor/settlement-presence per
    // occupied slot, with no choices for nonexistent slots: 1+80+6400 cases.
    // Cross each with absent/equal/different terminal anchor and MIN/MAX clock:
    // 38886 rows. The second call always supplies MAX time and a distinct anchor.
    // Nonempty symbolic metadata is not claimed proved by the empty harness.
    let entry = |code: u8, second: bool| SessionEventEntry {
        event: command(code / 16, second),
        processed: code & 1 != 0,
        processed_at_unix_ms: (code & 2 != 0).then_some(7),
        projection_anchor: (code & 4 != 0).then_some(SessionEventProjectionAnchor {
            source_commit_cursor: 7,
        }),
        dispatch_settlement: (code & 8 != 0).then_some(SessionEventDispatchSettlementReceipt {
            session_activity_epoch: 11,
            commit_observation_anchor: SessionEventProjectionAnchor {
                source_commit_cursor: 13,
            },
        }),
    };
    for length in 0..=2 {
        for first in 0..if length > 0 { 80 } else { 1 } {
            for second in 0..if length > 1 { 80 } else { 1 } {
                let mut events = Vec::new();
                if length > 0 {
                    events.push(entry(first, false));
                }
                if length > 1 {
                    events.push(entry(second, true));
                }
                for cursor in [None, Some(7), Some(u64::MAX)] {
                    for now in [u64::MIN, u64::MAX] {
                        check_resolution(
                            batch(events.clone()),
                            cursor.map(|source_commit_cursor| SessionEventProjectionAnchor {
                                source_commit_cursor,
                            }),
                            now,
                            Some(SessionEventProjectionAnchor {
                                source_commit_cursor: 9,
                            }),
                            u64::MAX,
                        );
                    }
                }
            }
        }
    }
}
