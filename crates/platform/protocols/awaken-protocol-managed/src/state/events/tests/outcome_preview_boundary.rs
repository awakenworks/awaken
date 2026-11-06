//! T31-C02: a live assistant preview never becomes an Outcome fact. Outcome
//! spans and the Session summary come only from the committed Outcome
//! projection, and their appearance extends the issued prefix.

use super::*;
use crate::preview::ThreadPreviewProjector;
use crate::types::PreviewFrame;
use awaken_agent_contract::event::{AgentEvent, Delta};

fn replica(
    runtime: &RehydrateFake,
    repo: Arc<dyn awaken_session_application::SessionStore>,
) -> ManagedState {
    let session_fixture_clock = awaken_time::system_clock();
    crate::state::test_support::managed_state_from_parts(
        std::sync::Arc::new(runtime.clone()),
        None,
        repo,
        awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
            .execution,
        session_fixture_clock,
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
}

/// The Outcome-fact oracle while no terminal Outcome is committed: no
/// observable surface may carry an evaluation span or a Session summary.
fn no_outcome_fact(event_types: &[String], evaluations: usize) -> Result<(), String> {
    if let Some(span) = event_types
        .iter()
        .find(|event_type| event_type.starts_with("span.outcome_evaluation_"))
    {
        return Err(format!("uncommitted Outcome surfaced as {span}"));
    }
    if evaluations > 0 {
        return Err(format!("{evaluations} uncommitted Outcome summaries"));
    }
    Ok(())
}

fn preview_types(frames: &[PreviewFrame]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| match frame {
            PreviewFrame::EventStart { event } => event.event_type.clone(),
            PreviewFrame::EventDelta { .. } => frame.type_str().to_owned(),
        })
        .collect()
}

fn listed(state: &ManagedState, session_id: &str) -> Vec<(String, String)> {
    state
        .list_events(session_id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .map(|event| (event.id.clone(), event.type_str().to_owned()))
        .collect()
}

/// | Rule | committed Outcome | live root preview | Effect |
/// |---|---|---|---|
/// | V1 | none | "satisfied" text deltas | only `agent.message` preview frames; no span, no summary |
/// | V2 | Completed (2 iterations) | none | six spans, one latest summary; the V1 prefix is retained |
/// | V3 | Completed | cold replica that never saw a preview | identical spans and summary |
/// | NC | none | a projector that derives an Outcome from preview text | the V1 oracle fails |
#[tokio::test]
async fn live_preview_text_never_becomes_an_outcome_fact() {
    let session_id = "session-outcome-preview";
    let repository: Arc<dyn awaken_session_application::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    let runtime = RehydrateFake::default();
    let (persisted, outcome_id) = retained_outcome_fixture(session_id, "outcome-preview-batch", 2);
    crate::state::test_support::create_session_fixture(
        repository.as_ref(),
        awaken_tenancy::DEFAULT_WORKSPACE_ID,
        persisted,
    )
    .await;
    let warm = replica(&runtime, repository.clone());
    warm.refresh_committed_events(session_id).await.unwrap();

    let mut projector = ThreadPreviewProjector::new(session_id.into(), session_id.into());
    let mut frames = Vec::new();
    for chunk in ["{\"result\":\"satisfied\",", "\"explanation\":\"FINAL\"}"] {
        frames.extend(projector.project(
            awaken_agent_contract::stream::event::Observation::assistant_delta(
                RunId("run-outcome-preview".into()),
                ThreadId(session_id.into()),
                0,
                0,
                AgentEvent::Delta(Delta::TextDelta {
                    delta: chunk.into(),
                }),
            ),
        ));
    }
    let previewed = preview_types(&frames);
    assert!(
        !previewed.is_empty()
            && frames.iter().all(|frame| match frame {
                PreviewFrame::EventStart { event } => event.event_type == "agent.message",
                PreviewFrame::EventDelta { .. } => true,
            }),
        "V1 a preview is only an agent.message preview: {previewed:?}"
    );
    warm.refresh_committed_events(session_id).await.unwrap();
    let prefix = listed(&warm, session_id);
    let mut observed = previewed.clone();
    observed.extend(prefix.iter().map(|(_, event_type)| event_type.clone()));
    no_outcome_fact(
        &observed,
        warm.get_session(session_id)
            .unwrap()
            .outcome_evaluations
            .len(),
    )
    .expect("V1 no Outcome fact before its committed terminal");

    // NC: a projector that turns a "satisfied" preview into a terminal Outcome.
    let preview_text = frames
        .iter()
        .filter_map(|frame| match frame {
            PreviewFrame::EventDelta { delta, .. } => serde_json::to_string(delta).ok(),
            PreviewFrame::EventStart { .. } => None,
        })
        .collect::<String>();
    let mut mutant = observed.clone();
    if preview_text.contains("satisfied") {
        mutant.push("span.outcome_evaluation_end".into());
    }
    let violation =
        no_outcome_fact(&mutant, 0).expect_err("NC preview-derived Outcome is rejected");
    assert!(
        violation.contains("span.outcome_evaluation_end"),
        "NC {violation}"
    );

    let final_message = Message::text(
        MessageId("outcome-preview-final".into()),
        Role::Assistant,
        "FINAL",
    );
    *runtime.committed.lock() = Some(vec![final_message.clone()]);
    runtime.outcome_projections.lock().insert(
        (session_id.into(), outcome_id.clone()),
        CommittedOutcomeProjection::Completed(OutcomeReport {
            iterations: vec![
                OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: outcome_id.clone(),
                    description: "ship".into(),
                    iteration: 0,
                    result: "needs_revision".into(),
                    explanation: "add FINAL".into(),
                    usage: None,
                },
                OutcomeIteration {
                    messages: vec![final_message],
                    outcome_id: outcome_id.clone(),
                    description: "ship".into(),
                    iteration: 1,
                    result: "satisfied".into(),
                    explanation: "done".into(),
                    usage: None,
                },
            ],
        }),
    );
    runtime
        .state_cursors_by_thread
        .lock()
        .insert(session_id.into(), vec![10, 20, 20, 20]);
    runtime
        .message_cursors_by_thread
        .lock()
        .insert(session_id.into(), vec![20]);
    *runtime.root_store_cursor_override.lock() = Some(30);
    warm.refresh_committed_events(session_id).await.unwrap();

    let completed = listed(&warm, session_id);
    assert_eq!(
        completed.get(..prefix.len()),
        Some(prefix.as_slice()),
        "V2 the committed Outcome extends the issued prefix"
    );
    let spans = |events: &[(String, String)]| {
        events
            .iter()
            .filter(|(_, event_type)| event_type.starts_with("span.outcome_evaluation_"))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(spans(&completed).len(), 6, "V2 one trio per iteration");
    let evaluations = warm.get_session(session_id).unwrap().outcome_evaluations;
    assert_eq!(evaluations.len(), 1, "V2 one latest summary");
    assert_eq!(evaluations[0].iteration, 1, "V2 latest");
    assert_eq!(
        evaluations[0].explanation.as_deref(),
        Some("done"),
        "V2 the summary is the committed report, not the previewed text"
    );

    let cold = replica(&runtime, repository);
    cold.refresh_committed_events(session_id).await.unwrap();
    assert_eq!(spans(&listed(&cold, session_id)), spans(&completed), "V3");
    assert_eq!(
        cold.get_session(session_id).unwrap().outcome_evaluations,
        evaluations,
        "V3"
    );
}
