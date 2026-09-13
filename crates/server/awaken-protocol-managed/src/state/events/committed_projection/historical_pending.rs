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

    /// Root and child use the same ordered, source-scoped history selection.
    /// A provider-local call id alone cannot transfer an earlier permission to
    /// another Thread, Run or input. With committed Run coordinates the call id
    /// binds the internal ticket name to the model-facing alias; legacy messages
    /// without those coordinates still require an exact tool name. Keep every
    /// matching boundary:
    /// one assistant response can contain several separately approved calls.
    pub(in crate::state::events) fn historical_message_boundaries<'a>(
        message: &awaken_agent_contract::agent::message::Message,
        history: &'a std::collections::HashMap<RunLifecycleCursor, Pending>,
        lifecycle: &'a [RunLifecycleEvent],
        thread_id: &str,
        source_run_id: Option<&str>,
    ) -> Vec<(Pending, &'a awaken_agent_contract::agent::run::Id)> {
        lifecycle
            .iter()
            .filter(|event| {
                event.thread_id.0 == thread_id
                    && source_run_id.is_none_or(|run| event.run_id.0 == run)
            })
            .filter_map(|event| {
                let pending = history.get(&event.cursor)?;
                message.content.iter().find_map(|block| {
                    let ContentBlock::ToolUse { id, name, input } = block else {
                        return None;
                    };
                    (id == &pending.tool_use_id
                        && input == &pending.input
                        && (name == &pending.name
                            || source_run_id == Some(event.run_id.0.as_str())))
                    .then(|| {
                        let mut public_pending = pending.clone();
                        public_pending.name = name.clone();
                        (public_pending, &event.run_id)
                    })
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_history_does_not_cross_message_ownership() {
        // Causes: Thread, source Run, raw call id, public alias and input can each
        // differ. Effects: H1 all equal selects the committed boundary; H2 any
        // ownership/payload mismatch selects none. Proven same-Run aliases
        // retain the public name (H3); absent coordinates require name equality
        // (H4), including a negative legacy-alias row.
        // The table is shared for root/child consumers; a same-name call alone
        // is never permission evidence. No ordering comes from HashMap keys.
        let pending = Pending {
            tool_use_id: "call".into(),
            name: "write".into(),
            input: serde_json::json!({"path":"one"}),
            client_executed: false,
        };
        let history = std::collections::HashMap::from([(RunLifecycleCursor(1), pending.clone())]);
        let lifecycle = vec![RunLifecycleEvent {
            cursor: RunLifecycleCursor(1),
            source_commit_cursor: 1,
            thread_id: awaken_agent_contract::ThreadId("thread".into()),
            run_id: awaken_agent_contract::agent::run::Id("run".into()),
            kind: RunLifecycleEventKind::Awaiting,
            state: awaken_agent_contract::agent::run::RunState::Awaiting,
            await_reason: None,
        }];
        for changed in [
            None,
            Some("thread"),
            Some("run"),
            Some("id"),
            Some("name"),
            Some("input"),
            Some("no-coordinate"),
            Some("legacy-alias"),
        ] {
            let mut call = pending.clone();
            if changed == Some("id") {
                call.tool_use_id = "other".into();
            }
            if matches!(changed, Some("name" | "legacy-alias")) {
                call.name = "read".into();
            }
            if changed == Some("input") {
                call.input = serde_json::json!({"path":"other"});
            }
            let message = awaken_agent_contract::agent::message::Message::new(
                awaken_agent_contract::agent::message::Id("message".into()),
                awaken_agent_contract::agent::message::Role::Assistant,
                vec![ContentBlock::tool_use(
                    call.tool_use_id,
                    call.name,
                    call.input,
                )],
            );
            let thread = if changed == Some("thread") {
                "other"
            } else {
                "thread"
            };
            let run = match changed {
                Some("run") => Some("other"),
                Some("no-coordinate" | "legacy-alias") => None,
                _ => Some("run"),
            };
            let selected = ManagedState::historical_message_boundaries(
                &message, &history, &lifecycle, thread, run,
            );
            assert_eq!(
                selected.len(),
                usize::from(changed.is_none() || matches!(changed, Some("no-coordinate" | "name"))),
                "H1-H3 {changed:?}"
            );
        }
    }
}
