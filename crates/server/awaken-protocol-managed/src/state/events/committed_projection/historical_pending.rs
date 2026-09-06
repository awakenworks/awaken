//! Historical Awaiting targets recovered from the Thread audit prefix.

use super::*;

pub(super) fn historical_pending_by_lifecycle<'a>(
    lifecycle_events: &[RunLifecycleEvent],
    snapshots: impl Fn(
        &str,
    )
        -> Option<&'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
) -> Result<std::collections::HashMap<awaken_agent_contract::RunLifecycleCursor, Pending>, StateError>
{
    let mut pending = std::collections::HashMap::new();
    let mut awaiting_ordinal = std::collections::HashMap::<(String, String), usize>::new();
    for lifecycle in lifecycle_events
        .iter()
        .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
    {
        let key = (lifecycle.thread_id.0.clone(), lifecycle.run_id.0.clone());
        let ordinal = awaiting_ordinal.entry(key).or_default();
        let historical = snapshots(&lifecycle.thread_id.0)
            .into_iter()
            .flat_map(|snapshot| snapshot.events.iter())
            .filter(|audit| {
                audit.run_id == lifecycle.run_id
                    && audit.kind == awaken_agent_contract::audit::kind::Kind::RunStateChanged
                    && serde_json::from_value::<awaken_agent_contract::agent::run::RunState>(
                        audit.payload["state"].clone(),
                    )
                    .is_ok_and(|state| {
                        state == awaken_agent_contract::agent::run::RunState::Awaiting
                    })
            })
            .nth(*ordinal)
            .and_then(|audit| audit.payload.get("await_target"))
            .map(|value| {
                serde_json::from_value::<awaken_agent_contract::agent::awaiting::AwaitTarget>(
                    value.clone(),
                )
            })
            .transpose()
            .map_err(|error| {
                StateError::Run(RunError::internal(format!(
                    "decode committed Awaiting target: {error}"
                )))
            })?
            .as_ref()
            .and_then(Pending::from_await_target);
        if let Some(historical) = historical {
            pending.insert(lifecycle.cursor, historical);
        }
        *ordinal += 1;
    }
    Ok(pending)
}

impl ManagedState {
    /// Materialize answerable events for Awaiting intervals that closed before
    /// this projector first observed them and had no committed ToolUse message.
    /// The historical RunStateChanged audit is already the ticket source; this
    /// method only applies the ordinary Managed tool projection to that source.
    pub(super) fn append_synthetic_historical_pending_projections(
        &self,
        record: &mut SessionRecord,
        session_id: &str,
        lifecycle_events: &[RunLifecycleEvent],
        historical_pending: &std::collections::HashMap<RunLifecycleCursor, Pending>,
        root_messages: &[awaken_agent_contract::agent::message::Message],
        child_messages: &std::collections::HashMap<
            String,
            Vec<awaken_agent_contract::agent::message::Message>,
        >,
    ) -> Result<(), StateError> {
        let mut cursors = historical_pending.keys().copied().collect::<Vec<_>>();
        cursors.sort_unstable();
        for cursor in cursors {
            let pending = &historical_pending[&cursor];
            let Some(lifecycle) = lifecycle_events.iter().find(|event| event.cursor == cursor)
            else {
                continue;
            };
            let is_root = lifecycle.thread_id.0 == session_id;
            let owner_thread_id = (!is_root).then_some(lifecycle.thread_id.0.as_str());
            let messages = if is_root {
                root_messages
            } else {
                child_messages
                    .get(&lifecycle.thread_id.0)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            };
            let has_committed_call = messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolUse { id, .. }
                            if id.as_str() == pending.tool_use_id.as_str()
                    )
                })
            });
            let tool_index = record.projected_tool_index_for(owner_thread_id);
            if has_committed_call || tool_index.latest.contains_key(&pending.tool_use_id) {
                continue;
            }
            let advisor_blocks = project::advisor_consultation_blocks(messages);
            let mut projected = project_messages_with_mcp_ids(
                &[],
                Some(pending),
                &tool_index.all,
                tool_index.mcp,
                &advisor_blocks,
            );
            self.qualify_tool_projection(
                record,
                owner_thread_id,
                &[],
                Some(&lifecycle.run_id),
                &mut projected,
            )?;
            for projected in projected {
                let id = projected.id.ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "synthetic historical pending projection has no qualified tool id",
                    ))
                })?;
                if let Some(owner) = owner_thread_id {
                    record
                        .event_thread_owners
                        .insert(id.clone(), owner.to_string());
                }
                if !record.events.iter().any(|event| event.id == id) {
                    record.events.push(Event {
                        id,
                        kind: projected.kind,
                        processed_at: None,
                    });
                }
            }
        }
        Ok(())
    }
}
