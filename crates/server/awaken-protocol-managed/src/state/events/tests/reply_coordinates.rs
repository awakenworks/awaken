//! T11-C02 reply coordinates and authorization: only the exact projected
//! tool-use Event id of the current Awaiting owner is admitted, exactly once,
//! without widening its target; every forged, stale, cross-Session,
//! cross-Thread, wrong-family or ambiguous coordinate is refused before any
//! root fact is appended or any Runtime delivery is driven.

use super::*;
use crate::project::{decode_managed_tool_event_id, managed_tool_event_id};
use awaken_session_application::SessionEventBatchIdempotency;
use awaken_session_types::RunErrorKind;

/// Every durable or driven effect a rejected batch must leave untouched.
#[derive(Debug, PartialEq)]
struct Effects {
    root: serde_json::Value,
    batches: usize,
    transcript: std::collections::HashMap<String, Vec<Message>>,
    deliveries: usize,
}

async fn effects(state: &ManagedState, runtime: &RehydrateFake, session_id: &str) -> Effects {
    let root = state
        .roles
        .roots
        .session(&SessionId::from(session_id))
        .await
        .expect("root is readable");
    Effects {
        batches: root.event_batches.len(),
        root: serde_json::to_value(&root).unwrap(),
        transcript: runtime.committed_by_thread.lock().unwrap().clone(),
        deliveries: runtime.thread_tool_replies.lock().unwrap().len(),
    }
}

async fn idempotency(
    state: &ManagedState,
    session_id: &str,
    key: &str,
    request: &SendEventsRequest,
) -> SessionEventBatchIdempotency {
    let fingerprint = awaken_agent_contract::stable_fingerprint(&(request, &None::<String>));
    state
        .roles
        .events
        .session_event_batch_idempotency(&SessionId::from(session_id), key, &fingerprint)
        .await
        .expect("idempotency index is readable")
}

/// The negative-control model: a checker that only verifies the public id
/// spelling (prefix plus well-formed length header). Production must be
/// stricter than this for every forged id below.
fn prefix_only_check_accepts(id: &str, expected_call_id: &str) -> bool {
    id.starts_with("mtool_v1_")
        && decode_managed_tool_event_id(id).is_some_and(|identity| {
            identity.call_id == expected_call_id && !identity.thread_id.is_empty()
        })
}

fn tool_result(id: &str) -> SendEventsRequest {
    SendEventsRequest {
        events: vec![InboundEvent::UserToolResult {
            tool_use_id: id.to_owned(),
            content: Some(vec![ContentBlock::text("forged result")]),
            is_error: false,
        }],
    }
}

/// Send `request` to `target_session` under a fresh key and prove the
/// rejection had zero facts and zero drive on every observed Session.
async fn assert_rejected_without_effect(
    state: &Arc<ManagedState>,
    runtime: &RehydrateFake,
    target_session: &str,
    observed_sessions: &[&str],
    request: SendEventsRequest,
    row: &str,
) {
    let mut before = Vec::new();
    for session in observed_sessions {
        before.push(effects(state, runtime, session).await);
    }
    let key = format!("forged-{row}");
    let error = state
        .send_events_attributed(target_session, request.clone(), None, Some(key.clone()))
        .await
        .expect_err(&format!("{row}: forged coordinate must be rejected"));
    assert!(
        matches!(&error, StateError::Run(error) if error.kind == RunErrorKind::BadRequest),
        "{row}: stable bad-request class, got {error:?}"
    );
    for (session, before) in observed_sessions.iter().zip(before) {
        assert_eq!(
            effects(state, runtime, session).await,
            before,
            "{row}: zero facts and zero drive on {session}"
        );
    }
    assert!(
        matches!(
            idempotency(state, target_session, &key, &request).await,
            SessionEventBatchIdempotency::Absent
        ),
        "{row}: the rejected key is not retained"
    );
}

#[tokio::test]
async fn forged_reply_coordinates_are_rejected_with_zero_facts_and_zero_drive() {
    // Cause/effect table over one child Awaiting a client-executed Agent tool
    // (generic `user.tool_result`). C = the reply's coordinate; every row
    // except P1 is refused (E0: bad request, root JSON/batches/transcript/
    // deliveries unchanged, idempotency key absent).
    // | Row | coordinate                                          | effect |
    // | F1  | exact id, sent to another Session (separate test)   | E0     |
    // | F2  | re-encoded onto the root Thread (cross-Thread)      | E0     |
    // | F3  | re-encoded onto another child Thread                | E0     |
    // | F4  | right prefix/thread/call, forged source (cross-Run) | E0     |
    // | F5  | stale projection id (the other legal source spelling) | E0   |
    // | F6  | exact id, wrong family (custom result / confirmation) | E0   |
    // NC: F2-F5 decode with the public-id decoder and pass a prefix-only
    // check, yet production rejects them.
    let (runtime, state, session_id, child_id, public_event_id) =
        Box::pin(pending_child_reply_fixture("coordinates", true, false)).await;
    let identity = decode_managed_tool_event_id(&public_event_id).expect("exact id decodes");
    assert_eq!(identity.thread_id, child_id);
    assert_eq!(identity.call_id, "call-route-coordinates");
    let source = identity.source_id.to_owned();
    let call = identity.call_id.to_owned();
    let stale_source = if source == "run-route-coordinates" {
        "message-route-coordinates".to_owned()
    } else {
        "run-route-coordinates".to_owned()
    };

    let observed = [session_id.as_str()];
    let forged = [
        ("F2", managed_tool_event_id(&session_id, &source, &call)),
        (
            "F3",
            managed_tool_event_id("sthr-route-other", &source, &call),
        ),
        ("F4", managed_tool_event_id(&child_id, "run-forged", &call)),
        ("F5", managed_tool_event_id(&child_id, &stale_source, &call)),
    ];
    for (row, id) in &forged {
        assert_ne!(id, &public_event_id, "{row}: forged id differs");
        assert!(
            prefix_only_check_accepts(id, &call),
            "{row} NC: a prefix-only check would accept {id}"
        );
        Box::pin(assert_rejected_without_effect(
            &state,
            &runtime,
            &session_id,
            &observed,
            tool_result(id),
            row,
        ))
        .await;
    }
    // NC sanity: the prefix-only model cannot tell the forged ids apart from
    // the exact one, so it is not the admission rule.
    assert!(prefix_only_check_accepts(&public_event_id, &call));

    for (row, event) in [
        (
            "F6-custom",
            InboundEvent::UserCustomToolResult {
                custom_tool_use_id: public_event_id.clone(),
                content: Some(vec![ContentBlock::text("wrong family")]),
                is_error: false,
            },
        ),
        (
            "F6-confirmation",
            InboundEvent::UserToolConfirmation {
                tool_use_id: public_event_id.clone(),
                result: ConfirmResult::Allow,
                deny_message: None,
            },
        ),
    ] {
        Box::pin(assert_rejected_without_effect(
            &state,
            &runtime,
            &session_id,
            &observed,
            SendEventsRequest {
                events: vec![event],
            },
            row,
        ))
        .await;
    }

    // P1: the exact coordinate is accepted once, for exactly its owner.
    let before = effects(&state, &runtime, &session_id).await;
    let exact = tool_result(&public_event_id);
    state
        .send_events_attributed(&session_id, exact.clone(), None, Some("exact".into()))
        .await
        .expect("P1 exact coordinate is admitted");
    let after = effects(&state, &runtime, &session_id).await;
    assert_eq!(after.batches, before.batches + 1, "P1 one batch");
    {
        let replies = runtime.thread_tool_replies.lock().unwrap();
        assert_eq!(replies.len(), 1, "P1 one delivery");
        assert_eq!(
            replies[0].target,
            SessionThreadTarget::Child(ThreadId(child_id.clone())),
            "P1 delivered to exactly the projected owner"
        );
        assert_eq!(replies[0].tool_use_id, call, "P1 exact Runtime call");
    }
    let root = state
        .roles
        .roots
        .session(&SessionId::from(&session_id))
        .await
        .unwrap();
    let retained = root
        .event_batches
        .last()
        .expect("P1 retained batch")
        .events
        .iter()
        .filter_map(|entry| match &entry.event {
            awaken_session_event_contract::SessionEventCommand::ToolReply { reply, .. } => {
                Some(reply.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 1, "P1 one retained reply");
    assert_eq!(
        retained[0].target,
        SessionThreadTarget::Child(ThreadId(child_id.clone())),
        "P1 no expanded target"
    );
    assert_eq!(
        retained[0].expected_run_id,
        RunId("run-route-coordinates".into()),
        "P1 exact Run"
    );
    assert_eq!(retained[0].tool_request_event_id, public_event_id);
    assert!(
        retained[0].expected_thread_version.is_some()
            && retained[0].answered_pending_commit_cursor.is_some(),
        "P1 new admission carries the full version fence"
    );
    assert!(matches!(
        idempotency(&state, &session_id, "exact", &exact).await,
        SessionEventBatchIdempotency::Exact(_)
    ));

    // P2: once is once - the same exact id under a new key is refused.
    Box::pin(assert_rejected_without_effect(
        &state,
        &runtime,
        &session_id,
        &observed,
        exact,
        "P2",
    ))
    .await;
}

#[tokio::test]
async fn reply_id_of_one_session_is_rejected_by_another_session() {
    // F1 cross-Session: Session B awaits its own child tool; the exact,
    // currently valid public id of Session A's child is sent to B. Each
    // Session has its own fixture Runtime because the fixture coordination
    // feed is not Session-scoped. NC: the foreign id decodes and passes a
    // prefix-only check, and B's own exact id is admitted afterwards.
    let (_runtime_a, _state_a, _session_a, child_a, foreign_id) =
        Box::pin(pending_child_reply_fixture("session-a", true, false)).await;
    let (runtime, state, session_b, child_b, own_id) =
        Box::pin(pending_child_reply_fixture("session-b", true, false)).await;
    let identity = decode_managed_tool_event_id(&foreign_id).expect("F1 foreign id decodes");
    assert_eq!(identity.thread_id, child_a);
    assert!(
        prefix_only_check_accepts(&foreign_id, "call-route-session-a"),
        "F1 NC: a prefix-only check would accept the foreign id"
    );
    Box::pin(assert_rejected_without_effect(
        &state,
        &runtime,
        &session_b,
        &[session_b.as_str()],
        tool_result(&foreign_id),
        "F1",
    ))
    .await;
    state
        .send_events(&session_b, tool_result(&own_id))
        .await
        .expect("F1 positive control: B's own exact id is admitted");
    let replies = runtime.thread_tool_replies.lock().unwrap();
    assert_eq!(replies.len(), 1, "F1 one delivery");
    assert_eq!(
        replies[0].target,
        SessionThreadTarget::Child(ThreadId(child_b)),
        "F1 delivered only to B's own child"
    );
}

#[tokio::test]
async fn ambiguous_legacy_reply_id_is_rejected_with_zero_facts_and_zero_drive() {
    // F7: a pre-qualification id (raw Runtime call id) while the root and the
    // child both await the same call/family has no unique owner.
    // NC: the raw id carries no qualified prefix at all; only the owner
    // uniqueness rule can refuse it.
    let (runtime, state, session_id, _child_id, _) =
        Box::pin(pending_child_reply_fixture("ambiguous", true, true)).await;
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: "call-route-ambiguous".into(),
        name: "client_lookup".into(),
        input: serde_json::json!({"owner":"root"}),
        client_executed: true,
    });
    runtime
        .committed_by_thread
        .lock()
        .unwrap()
        .get_mut(&session_id)
        .expect("root transcript")
        .push(Message::new(
            MessageId("message-route-ambiguous-root".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "call-route-ambiguous",
                "client_lookup",
                serde_json::json!({"owner":"root"}),
            )],
        ));
    runtime
        .message_cursors_by_thread
        .lock()
        .unwrap()
        .get_mut(&session_id)
        .expect("root transcript coordinates")
        .push(4);
    let root_run = RunId("run-route-ambiguous-root".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            3,
            &session_id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            4,
            &session_id,
            &root_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);
    state.refresh_committed_events(&session_id).await.unwrap();
    assert!(
        decode_managed_tool_event_id("call-route-ambiguous").is_none(),
        "F7 the legacy id is not a qualified id"
    );
    Box::pin(assert_rejected_without_effect(
        &state,
        &runtime,
        &session_id,
        &[session_id.as_str()],
        SendEventsRequest {
            events: vec![InboundEvent::UserCustomToolResult {
                custom_tool_use_id: "call-route-ambiguous".into(),
                content: Some(vec![ContentBlock::text("ambiguous")]),
                is_error: false,
            }],
        },
        "F7",
    ))
    .await;
}

/// The exact admitted reply input, then forged at the application boundary.
async fn forged_application_reply(
    mutate: impl FnOnce(&mut awaken_session_event_contract::SessionEventToolReply),
) -> (
    RehydrateFake,
    Arc<ManagedState>,
    String,
    Effects,
    Result<(), RunError>,
) {
    let (runtime, state, session_id, _child_id, public_event_id) =
        Box::pin(pending_child_reply_fixture("application", true, false)).await;
    let mut validated = state
        .validate_event_batch(&session_id, &tool_result(&public_event_id).events)
        .await
        .expect("the exact reply is admissible");
    let Some(awaken_session_event_contract::SessionEventInput::ToolReply(reply)) =
        validated.inputs.first_mut()
    else {
        panic!("the exact reply lowers to one ToolReply input");
    };
    mutate(reply);
    let before = effects(&state, &runtime, &session_id).await;
    let outcome = state
        .application
        .event_batches()
        .append_owned_session_event_batch(
            &awaken_session_application::SessionRootReads::authorize(
                &*state.application.queries(),
                &state.resolve_owner(&session_id).await.unwrap().unwrap(),
                &SessionId::from(&session_id),
            )
            .await
            .unwrap(),
            validated.inputs,
            None,
            None,
            Some(("forged-application".into(), "fingerprint".into())),
        )
        .await
        .map(|_| ());
    (runtime, state, session_id, before, outcome)
}

#[tokio::test]
async fn application_append_rejects_forged_reply_coordinates() {
    let (runtime, state, session_id, before, outcome) =
        Box::pin(forged_application_reply(|reply| {
            reply.expected_run_id = RunId("run-forged".into());
            reply.target = SessionThreadTarget::Child(ThreadId("sthr-route-other".into()));
        }))
        .await;
    assert!(outcome.is_err(), "forged coordinates must be refused");
    assert_eq!(
        effects(&state, &runtime, &session_id).await,
        before,
        "zero facts"
    );
}

#[tokio::test]
async fn application_append_rejects_version_less_new_reply() {
    let (runtime, state, session_id, before, outcome) =
        Box::pin(forged_application_reply(|reply| {
            reply.expected_thread_version = None;
            reply.answered_pending_commit_cursor = None;
        }))
        .await;
    assert!(
        outcome.is_err(),
        "a version-less coordinate is reserved for legacy rows, never a new admission"
    );
    assert_eq!(
        effects(&state, &runtime, &session_id).await,
        before,
        "zero facts"
    );
}
