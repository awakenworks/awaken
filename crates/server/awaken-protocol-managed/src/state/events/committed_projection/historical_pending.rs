// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
//! Answerable events for historical Awaiting targets recovered from the Thread
//! audit prefix (see [`awaken_managed_session_events::historical_pending_by_lifecycle`]).

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

impl ManagedState {
    /// Render the one synthetic event of each accepted Awaiting target the
    /// timeline domain selects (no exact ToolUse Message belonged to that
    /// committed prefix yet), unless an earlier refresh already issued it.
    pub(super) fn append_synthetic_historical_pending_projections(
        &self,
        record: &mut SessionRecord,
        session_id: &str,
        index: &OrderIndex<'_>,
    ) -> Result<(), StateError> {
        for tool in index.synthetic_awaiting_tools().map_err(projection_error)? {
            let owner_thread_id = (tool.thread_id != session_id).then_some(tool.thread_id.as_str());
            let qualified_id = project::managed_tool_event_id(
                &Self::public_tool_thread_id(session_id, owner_thread_id),
                &tool.run_id.0,
                &tool.pending.tool_use_id,
            );
            if record.has_event_id(&qualified_id) {
                continue;
            }
            let mut projected = project_messages_with_mcp_ids(
                &[],
                Some(&tool.pending),
                &std::collections::HashSet::new(),
                Default::default(),
                &Default::default(),
                &[&tool.pending],
            );
            Self::qualify_tool_projection(
                record,
                owner_thread_id,
                Some(&tool.run_id.0),
                &[],
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
