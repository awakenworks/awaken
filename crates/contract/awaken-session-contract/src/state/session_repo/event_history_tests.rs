use super::*;
use crate::state::SessionEventHistory;

fn batch(id: &crate::state::SessionId, batch_id: &str) -> crate::state::SessionEventBatch {
    crate::state::SessionEventBatch::compile(
        id,
        batch_id,
        vec![crate::state::SessionEventInput::UserMessage {
            content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "input",
            )],
        }],
    )
    .unwrap()
}

#[test]
fn history_cursor_covers_all_retained_batches_without_mutating_dto() {
    let mut session = mutation_tests::session("history", SessionRevision(1));
    assert_eq!(SessionEventHistory::from(&session).projection_cursor(), 0);
    for (name, cursor) in [("first", 9), ("second", 4), ("last", 17)] {
        let mut batch = batch(&session.session_id, name);
        batch.events[0].projection_anchor = Some(crate::state::SessionEventProjectionAnchor {
            source_commit_cursor: cursor,
        });
        session.event_batches.push(batch);
    }
    let before = serde_json::to_vec(&session).unwrap();
    assert_eq!(SessionEventHistory::from(&session).projection_cursor(), 17);
    assert_eq!(serde_json::to_vec(&session).unwrap(), before);
}

#[test]
fn matching_history_does_not_hide_malformed_later_provenance() {
    let mut session = mutation_tests::session("history-corruption", SessionRevision(1));
    let first = batch(&session.session_id, "first");
    let crate::state::SessionEventCommand::UserMessage { run_id, .. } = &first.events[0].event
    else {
        unreachable!();
    };
    let run_id = run_id.clone();
    session.event_batches.push(first.clone());
    assert!(
        SessionEventHistory::from(&session)
            .retains_user_run(&run_id)
            .unwrap()
    );
    assert!(
        !SessionEventHistory::from(&session)
            .retains_user_run(&awaken_agent_contract::agent::run::Id("absent".into()))
            .unwrap()
    );
    let mut malformed = first;
    malformed.events.push(malformed.events[0].clone());
    session.event_batches.push(malformed);
    assert_eq!(
        SessionEventHistory::from(&session).retains_user_run(&run_id),
        Err(crate::state::SessionEventBatchError::ProgressMismatch)
    );
}
