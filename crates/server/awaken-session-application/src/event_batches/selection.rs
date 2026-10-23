//! Pure selection of the next committed Session event an Event batch handles.

use awaken_session_event_contract::{SessionEventCommand, SessionUserRunSystemInput};
use awaken_session_state_contract::PersistedSession;

use super::{FailedToolReplyCoordinate, SelectedSessionEvent, SessionEventSelection};

pub(super) fn adjacent_system_input(
    session: &PersistedSession,
    batch_id: &str,
    operation_id: &str,
) -> Option<SessionUserRunSystemInput> {
    session
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == batch_id)
        .and_then(|batch| {
            batch
                .events
                .iter()
                .position(|entry| entry.event.operation_id() == operation_id)
                .and_then(|ordinal| batch.events.get(ordinal + 1))
        })
        .and_then(|entry| match &entry.event {
            SessionEventCommand::SystemMessage {
                operation_id,
                content,
            } => Some(SessionUserRunSystemInput {
                operation_id: operation_id.clone(),
                content: content.clone(),
            }),
            _ => None,
        })
}

pub(super) fn preceding_event_runtime_target(
    session: &PersistedSession,
    batch_id: &str,
    operation_id: &str,
) -> Option<(
    awaken_agent_contract::agent::thread::Id,
    awaken_agent_contract::agent::run::Id,
)> {
    let batch = session
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == batch_id)?;
    let ordinal = batch
        .events
        .iter()
        .position(|entry| entry.event.operation_id() == operation_id)?;
    batch.events[..ordinal]
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            SessionEventCommand::UserMessage { run_id, .. } => Some((
                awaken_agent_contract::agent::thread::Id(session.session_id.to_string()),
                run_id.clone(),
            )),
            SessionEventCommand::ToolReply { reply, .. } => Some((
                reply.target.thread_id(&session.session_id),
                reply.expected_run_id.clone(),
            )),
            SessionEventCommand::SystemMessage { .. }
            | SessionEventCommand::DefineOutcome { .. }
            | SessionEventCommand::Interrupt { .. } => None,
        })
}

/// Select the next command through the durable aggregate's ordinary priority
/// and FIFO rules. Request-local acceleration must compare against this exact
/// result rather than implementing another ordering rule.
fn select_normal_session_event(session: &PersistedSession) -> Option<SelectedSessionEvent> {
    let select = |predicate: fn(&SessionEventCommand) -> bool| {
        session.event_batches.iter().find_map(|batch| {
            batch.events.iter().find_map(|entry| {
                (!entry.processed && predicate(&entry.event)).then(|| SelectedSessionEvent {
                    batch_id: batch.batch_id.clone(),
                    event: entry.event.clone(),
                    traceparent: batch.traceparent.clone(),
                    processed_anchor: None,
                    superseded_tool_reply: None,
                })
            })
        })
    };
    select(|event| {
        matches!(
            event,
            SessionEventCommand::ToolReply { .. }
                | SessionEventCommand::DefineOutcome { .. }
                | SessionEventCommand::Interrupt { .. }
        )
    })
    .or_else(|| {
        session.event_batches.iter().find_map(|batch| {
            batch
                .events
                .windows(2)
                .find(|pair| {
                    pair[0].processed
                        && matches!(&pair[0].event, SessionEventCommand::ToolReply { .. })
                        && !pair[1].processed
                        && matches!(&pair[1].event, SessionEventCommand::SystemMessage { .. })
                })
                .map(|pair| SelectedSessionEvent {
                    batch_id: batch.batch_id.clone(),
                    event: pair[1].event.clone(),
                    traceparent: batch.traceparent.clone(),
                    processed_anchor: None,
                    superseded_tool_reply: None,
                })
        })
    })
    .or_else(|| {
        select(|event| {
            !matches!(
                event,
                SessionEventCommand::ToolReply { .. }
                    | SessionEventCommand::DefineOutcome { .. }
                    | SessionEventCommand::Interrupt { .. }
            )
        })
    })
}

/// One eligibility selector over retained provenance. Normal selection keeps
/// ToolReply, DefineOutcome, and Interrupt receipt commands in their original
/// relative order while letting them cross queued User entries. A request-local
/// preferred batch narrows that same authority to its ToolReply/Interrupt
/// receipt, or starts its User command only when the ordinary selector confirms
/// it is already the global FIFO head. Only after an exact ToolReply delivery has failed may recovery select
/// a later retained Interrupt that includes the failed reply's exact Thread
/// target; it must rediscover the same unprocessed failed operation in a fresh
/// root before crossing it. An already-processed Interrupt supplies its root
/// anchor without repeating the Runtime effect, closing the crash window between
/// Interrupt anchoring and reply supersession. A System
/// immediately following an already-processed ToolReply is otherwise eligible
/// only as a side-effect-free observation of its exact committed Message.
/// User-associated System and every other command remain FIFO.
pub(super) fn select_session_event(
    session: &PersistedSession,
    selection: SessionEventSelection<'_>,
) -> Option<SelectedSessionEvent> {
    if let SessionEventSelection::InterruptAfterFailedToolReply {
        batch_id,
        operation_id,
    } = selection
    {
        let mut failed_tool_reply = None;
        for batch in &session.event_batches {
            for entry in &batch.events {
                if failed_tool_reply.is_none() {
                    if batch.batch_id == batch_id
                        && entry.event.operation_id() == operation_id
                        && !entry.processed
                        && let SessionEventCommand::ToolReply { reply, .. } = &entry.event
                    {
                        let activity_operation_id = reply
                            .delivery_command(
                                &session.session_id,
                                adjacent_system_input(session, batch_id, operation_id),
                            )
                            .activity_operation_id();
                        failed_tool_reply = Some((
                            reply.target.clone(),
                            FailedToolReplyCoordinate {
                                batch_id: batch_id.to_string(),
                                operation_id: operation_id.to_string(),
                                activity_operation_id,
                            },
                        ));
                    }
                    continue;
                }
                let Some((failed_target, failed)) = failed_tool_reply.as_ref() else {
                    continue;
                };
                if let SessionEventCommand::Interrupt { interrupt, .. } = &entry.event
                    && interrupt.targets.contains(failed_target)
                {
                    let processed_anchor = if entry.processed {
                        let Some(anchor) = entry.projection_anchor else {
                            // Legacy processed rows can lack projection
                            // provenance. They cannot own supersession, but
                            // neither may they starve a later exact Interrupt.
                            continue;
                        };
                        Some(anchor)
                    } else {
                        None
                    };
                    return Some(SelectedSessionEvent {
                        batch_id: batch.batch_id.clone(),
                        event: entry.event.clone(),
                        traceparent: batch.traceparent.clone(),
                        processed_anchor,
                        superseded_tool_reply: Some(failed.clone()),
                    });
                }
            }
        }
        return None;
    }

    let SessionEventSelection::Normal { preferred_batch_id } = selection else {
        unreachable!("failed-reply selection returned above")
    };
    if let Some(batch_id) = preferred_batch_id {
        let batch = session
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == batch_id)?;
        if let Some(receipt) = batch.events.iter().find_map(|entry| {
            (!entry.processed
                && matches!(
                    &entry.event,
                    SessionEventCommand::ToolReply { .. } | SessionEventCommand::Interrupt { .. }
                ))
            .then(|| SelectedSessionEvent {
                batch_id: batch.batch_id.clone(),
                event: entry.event.clone(),
                traceparent: batch.traceparent.clone(),
                processed_anchor: None,
                superseded_tool_reply: None,
            })
        }) {
            return Some(receipt);
        }
        return select_normal_session_event(session).filter(|selected| {
            selected.batch_id == batch_id
                && matches!(&selected.event, SessionEventCommand::UserMessage { .. })
        });
    }
    select_normal_session_event(session)
}
