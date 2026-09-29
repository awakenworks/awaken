// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
//! Answerable events for historical Awaiting targets recovered from the Thread
//! audit prefix (see [`awaken_managed_session_events::historical_pending_by_lifecycle`]).

use super::*;

impl ManagedState {
    /// Materialize the one synthetic event for an accepted Awaiting target
    /// when no exact ToolUse Message belonged to that committed prefix yet.
    /// A Message committed later cannot rekey an already-issued Run-qualified
    /// tool; the audit and Message cursors decide this on warm and cold reads.
    pub(super) fn append_synthetic_historical_pending_projections(
        &self,
        record: &mut SessionRecord,
        session_id: &str,
        lifecycle_events: &[RunLifecycleEvent],
        historical_pending: &std::collections::HashMap<RunLifecycleCursor, Pending>,
        root_snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        child_snapshots: &std::collections::HashMap<
            String,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
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
            let snapshot = if is_root {
                root_snapshot
            } else {
                child_snapshots.get(&lifecycle.thread_id.0)
            }
            .ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "accepted Awaiting target has no committed Thread snapshot",
                ))
            })?;
            if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
                return Err(StateError::Run(RunError::internal(
                    "committed ToolUse Message cursors are not aligned",
                )));
            }
            let has_as_of_message = snapshot
                .messages
                .iter()
                .zip(&snapshot.message_commit_cursors)
                .any(|(message, source)| {
                    *source <= lifecycle.source_commit_cursor
                        && awaken_managed_session_events::historical_message_await_name(
                            message,
                            pending,
                            &lifecycle.run_id,
                            message.id.assistant_coordinates().map(|(run, _, _)| run),
                        )
                        .is_some()
                });
            if has_as_of_message {
                continue;
            }
            let qualified_id = project::managed_tool_event_id(
                &Self::public_tool_thread_id(session_id, owner_thread_id),
                &lifecycle.run_id.0,
                &pending.tool_use_id,
            );
            if record.has_event_id(&qualified_id) {
                continue;
            }
            let tool_index = record.projected_tool_index_for(owner_thread_id);
            let mut projected = project_messages_with_mcp_ids(
                &[],
                Some(pending),
                &std::collections::HashSet::new(),
                tool_index.mcp,
                &Default::default(),
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
