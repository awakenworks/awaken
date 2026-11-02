//! T11-C01: after the awaited Run ends, a late tool result is refused by the
//! ordinary reply rule, the exact retained receipt still replays, and neither
//! path drives a Run, a tool reply or a root fact.

use super::*;
use awaken_session_types::RunErrorKind;

/// Every durable fact and every Runtime drive a terminal reply must not add.
#[derive(Debug, PartialEq)]
struct Drives {
    root: serde_json::Value,
    transcript: std::collections::HashMap<String, Vec<Message>>,
    tool_replies: usize,
    delegated_runs: usize,
}

async fn drives(state: &ManagedState, runtime: &RehydrateFake, session_id: &str) -> Drives {
    let root = state
        .roles
        .roots
        .session(&SessionId::from(session_id))
        .await
        .expect("root is readable");
    Drives {
        root: serde_json::to_value(&root).unwrap(),
        transcript: runtime.committed_by_thread.lock().clone(),
        tool_replies: runtime.thread_tool_replies.lock().len(),
        delegated_runs: runtime.delegated.lock().len(),
    }
}

/// The replay oracle: an exact retry returns exactly the retained receipt.
fn replays_original(
    outcome: &Result<SendEventsResponse, StateError>,
    expected: &SendEventsResponse,
) -> Result<(), String> {
    match outcome {
        Ok(response) => {
            let (actual, expected) = (
                serde_json::to_value(response).unwrap(),
                serde_json::to_value(expected).unwrap(),
            );
            if actual == expected {
                Ok(())
            } else {
                Err(format!("replayed another receipt: {actual} != {expected}"))
            }
        }
        Err(error) => Err(format!("legal exact replay was refused: {error}")),
    }
}

/// | Rule | Run | key / body | Effect |
/// |---|---|---|---|
/// | T1 | Ended | new key | refused by the reply rule (BadRequest, no match); no drive |
/// | T2 | Ended | retained key, changed body | IdempotencyMismatch; no drive |
/// | T3 | Ended | retained key, exact body | the original receipt; no drive, no recovery read |
/// | NC | Ended | a "refuse every terminal reply" implementation | the replay oracle fails |
#[tokio::test]
async fn ended_run_refuses_new_replies_and_replays_only_the_exact_receipt() {
    for cause in [EndCause::NaturalEnd, EndCause::Cancelled] {
        let (runtime, state, session_id, child_id, public_event_id) =
            pending_child_reply_fixture("terminal-receipt", true, false).await;
        let request = SendEventsRequest {
            events: vec![InboundEvent::UserToolResult {
                tool_use_id: public_event_id.clone(),
                content: Some(vec![ContentBlock::text("accepted result")]),
                is_error: false,
            }],
        };
        let validated = state
            .validate_event_batch(&session_id, &request.events)
            .await
            .expect("setup: the exact Awaiting reply is admissible");
        let fingerprint = awaken_agent_contract::stable_fingerprint(&(&request, &None::<String>));
        let (access, _) = crate::state::sessions::owned_session_access(&state, &session_id)
            .await
            .expect("setup: the Workspace owns the Session");
        let retained = state
            .roles
            .events
            .append_session_event_batch_idempotent(
                &access,
                validated.inputs,
                None,
                None,
                Some(("original".into(), fingerprint)),
            )
            .await
            .expect("setup: retain the accepted receipt")
            .into_batch()
            .expect("setup: an input batch is admitted, not a root no-op");
        let expected = SendEventsResponse {
            data: accepted_inbound_receipts(&session_id, &retained)
                .into_iter()
                .map(|projection| projection.event)
                .collect(),
        };
        let kind = if matches!(cause, EndCause::Cancelled) {
            RunLifecycleEventKind::Cancelled
        } else {
            RunLifecycleEventKind::Completed
        };
        runtime.lifecycle.lock().push(lifecycle(
            3,
            &child_id,
            &RunId("run-route-terminal-receipt".into()),
            kind,
            RunState::Ended(cause.clone()),
        ));
        runtime.pending_by_thread.lock().remove(&child_id);
        let before = drives(&state, &runtime, &session_id).await;

        let refused = state
            .send_events_attributed(&session_id, request.clone(), None, Some("late-new".into()))
            .await
            .expect_err("T1 a new late result is refused");
        assert!(
            matches!(&refused, StateError::Run(run) if run.kind == RunErrorKind::BadRequest),
            "T1 {cause:?}: {refused:?}"
        );
        assert_eq!(
            refused.to_string(),
            ManagedState::invalid_tool_reply().to_string(),
            "T1 {cause:?} the ordinary reply rule refuses, not an unrelated failure"
        );
        assert_eq!(drives(&state, &runtime, &session_id).await, before, "T1");

        let mut changed = request.clone();
        changed.events = vec![InboundEvent::UserToolResult {
            tool_use_id: public_event_id,
            content: Some(vec![ContentBlock::text("a different result")]),
            is_error: false,
        }];
        let mismatch = state
            .send_events_attributed(&session_id, changed, None, Some("original".into()))
            .await
            .expect_err("T2 a changed body under the retained key conflicts");
        assert!(
            matches!(mismatch, StateError::IdempotencyMismatch),
            "T2 {cause:?}: {mismatch:?}"
        );
        assert_eq!(drives(&state, &runtime, &session_id).await, before, "T2");

        let reads = runtime.reads.count("recovery");
        let replay = state
            .send_events_attributed(&session_id, request.clone(), None, Some("original".into()))
            .await;
        replays_original(&replay, &expected).expect("T3 exact receipt");
        assert_eq!(
            runtime.reads.count("recovery"),
            reads,
            "T3 no recovery read"
        );
        assert_eq!(drives(&state, &runtime, &session_id).await, before, "T3");

        // NC: an implementation that refuses every terminal reply ignores the
        // retained key; production's own fresh-key path is exactly that.
        let mutant = state
            .send_events_attributed(
                &session_id,
                request,
                None,
                Some("mutant-ignores-key".into()),
            )
            .await;
        let violation = replays_original(&mutant, &expected)
            .expect_err("NC refusing every terminal reply breaks legal replay");
        assert!(violation.contains("refused"), "NC {violation}");
        assert_eq!(drives(&state, &runtime, &session_id).await, before, "NC");
    }
}
