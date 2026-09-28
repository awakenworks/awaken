// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
//! Answerable events for historical Awaiting targets recovered from the Thread
//! audit prefix (see [`awaken_managed_session_events::historical_pending_by_lifecycle`]).

use super::*;

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
                &[pending],
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
                record.append_event_once(Event {
                    id,
                    kind: projected.kind,
                    processed_at: None,
                });
            }
        }
        Ok(())
    }
}
