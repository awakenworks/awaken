// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Read-only result attribution reuses the canonical occurrence fold and historical Awaiting predicate.
// @formal reason: One source-qualified result is derived from the same prefix; ambiguous or missing sources refuse projection.
//! Exact result sources, shared by root and child wire projection.

use super::{OrderIndex, ThreadIndex};
use crate::{ProjectionError, ToolResultSource};
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::{RunLifecycleEventKind, agent::content::ContentBlock};

impl OrderIndex<'_> {
    pub fn tool_result_sources(
        &self,
        thread_id: &str,
        message: &Message,
    ) -> Result<Vec<ToolResultSource>, ProjectionError> {
        if message.role != Role::Tool {
            return Ok(Vec::new());
        }
        let thread = self.threads.get(thread_id).ok_or_else(|| {
            ProjectionError::corrupt("tool result has no committed Thread prefix")
        })?;
        if !thread.aligned {
            return Err(ProjectionError::corrupt(
                "tool result prefix has misaligned Message coordinates",
            ));
        }
        let committed = thread
            .messages_by_id
            .get(message.id.0.as_str())
            .and_then(|positions| positions.first())
            .map(|index| &thread.snapshot.messages[*index]);
        if committed != Some(message) {
            return Err(ProjectionError::corrupt(
                "tool result Message differs from its committed prefix",
            ));
        }
        message
            .content
            .iter()
            .enumerate()
            .filter_map(|(ordinal, block)| {
                let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                    return None;
                };
                if thread
                    .transcript
                    .advisor_blocks()
                    .is_result(&message.id.0, ordinal)
                {
                    return None;
                }
                Some(self.tool_result_source(thread, &message.id.0, ordinal, tool_use_id))
            })
            .collect()
    }

    fn tool_result_source(
        &self,
        thread: &ThreadIndex<'_>,
        result_message_id: &str,
        ordinal: usize,
        call_id: &str,
    ) -> Result<ToolResultSource, ProjectionError> {
        let position = *thread
            .messages_by_id
            .get(result_message_id)
            .and_then(|positions| positions.first())
            .ok_or_else(|| {
                ProjectionError::corrupt("tool result Message is absent from its prefix")
            })?;
        let cursor = thread
            .snapshot
            .message_commit_cursors
            .get(position)
            .copied()
            .filter(|cursor| *cursor > 0 && *cursor <= thread.snapshot.store_cursor)
            .ok_or_else(|| {
                ProjectionError::corrupt("tool result has no exact committed coordinate")
            })?;
        let ordinary = thread.transcript.result_source(result_message_id, ordinal);
        if ordinary.is_some_and(|source| source.call_id != call_id) {
            return Err(ProjectionError::corrupt(
                "tool result call differs from its committed occurrence",
            ));
        }
        let source_message = ordinary
            .and_then(|source| thread.messages_by_id.get(source.source_id.as_str()))
            .and_then(|positions| positions.first())
            .map(|index| &thread.snapshot.messages[*index]);
        let mut synthetic = None;
        for (await_cursor, pending) in self.evidence.historical_pending {
            if pending.tool_use_id != call_id {
                continue;
            }
            let Some(awaiting) = self.lifecycle_by_cursor.get(await_cursor).filter(|event| {
                event.thread_id == thread.snapshot.thread_id
                    && event.kind == RunLifecycleEventKind::Awaiting
                    && event.source_commit_cursor <= cursor
                    && self
                        .next_run_boundary(event)
                        .is_none_or(|next| cursor <= next.source_commit_cursor)
            }) else {
                continue;
            };
            if !crate::historical::needs_synthetic_awaiting_tool(thread.snapshot, awaiting, pending)
            {
                continue;
            }
            if source_message.is_some_and(|message| {
                crate::historical_message_await_name(
                    message,
                    pending,
                    &awaiting.run_id,
                    message.id.assistant_coordinates().map(|(run, _, _)| run),
                )
                .is_none()
            }) {
                continue;
            }
            let source = ToolResultSource {
                source_id: awaiting.run_id.0.clone(),
                call_id: call_id.to_string(),
                name: pending.name.clone(),
            };
            if synthetic
                .as_ref()
                .is_some_and(|previous| previous != &source)
            {
                return Err(ProjectionError::corrupt(
                    "tool result matches multiple synthetic Awaiting sources",
                ));
            }
            synthetic = Some(source);
        }
        // The call id keeps the refusal actionable: it names the result whose
        // ToolUse (or synthetic Awaiting source) is not in this prefix yet.
        synthetic.or_else(|| ordinary.cloned()).ok_or_else(|| {
            ProjectionError::corrupt(format!(
                "tool result `{call_id}` has no preceding tool-use identity"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OrderEvidence, ParentTerminalEvidence};
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_agent_contract::agent::run::{Record, RunState};
    use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
    use awaken_agent_contract::{
        RunId, RunLifecycleCursor, RunLifecycleEvent, ThreadId, encode_run_lifecycle_cursor,
    };
    use awaken_session_application_contract::Pending;
    use std::collections::{HashMap, HashSet};

    fn call(step: usize, name: &str) -> Message {
        Message::new(
            MessageId::assistant(&RunId("run".into()), step),
            Role::Assistant,
            vec![ContentBlock::tool_use("same", name, serde_json::json!({}))],
        )
    }
    fn result(id: &str) -> Message {
        Message::new(
            MessageId(id.into()),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "same",
                vec![ContentBlock::text("done")],
            )],
        )
    }
    fn snapshot(messages: Vec<Message>, cursors: Vec<u64>) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            thread_id: ThreadId("thread".into()),
            claimed_run_id: RunId("run".into()),
            runs: vec![Record {
                id: RunId("run".into()),
                thread_id: ThreadId("thread".into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(RunId("run".into())),
            messages,
            message_commit_cursors: cursors,
            commit_timestamps: Default::default(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 30,
            next_commit_ordinal: 0,
        }
    }
    fn sources(
        snapshot: &RunRecoverySnapshot,
        lifecycle: &[RunLifecycleEvent],
        pending: &HashMap<RunLifecycleCursor, Pending>,
        message: &Message,
    ) -> Result<Vec<ToolResultSource>, ProjectionError> {
        let children = HashMap::new();
        let index = OrderIndex::new(OrderEvidence {
            updates: None,
            session_id: "thread",
            batches: &[],
            closed_intervals: &[],
            budget_reach: &[],
            parent_terminal: ParentTerminalEvidence::default(),
            lifecycle,
            root: Some(snapshot),
            children: &children,
            links: &[],
            outcomes: &[],
            historical_pending: pending,
        });
        index.tool_result_sources("thread", message)
    }
    fn awaiting(cursor: u64, run: &str) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: encode_run_lifecycle_cursor(cursor, 0).unwrap(),
            source_commit_cursor: cursor,
            thread_id: ThreadId("thread".into()),
            run_id: RunId(run.into()),
            kind: RunLifecycleEventKind::Awaiting,
            state: RunState::Awaiting,
            await_reason: None,
        }
    }

    #[test]
    fn exact_tool_result_sources_are_fifo_and_replay_stable() {
        // C1 two same-id ToolUses precede both results; C2 builtin/MCP families;
        // C3 byte-equal source/result replay. R1 C1+C2 => first result owns first
        // Message, second owns second; R2 C3 => neither binding is rekeyed.
        let first = call(1, "read");
        let second = call(2, "mcp__srv__echo");
        let r1 = result("r1");
        let r2 = result("r2");
        let evidence = snapshot(
            vec![
                first.clone(),
                second.clone(),
                r1.clone(),
                r2.clone(),
                first.clone(),
                r1.clone(),
            ],
            vec![1, 2, 3, 4, 5, 6],
        );
        for (message, source) in [(&r1, &first), (&r2, &second)] {
            let selected = sources(&evidence, &[], &HashMap::new(), message).unwrap();
            assert_eq!(selected[0].source_id, source.id.0, "R1-R2");
            assert_eq!(selected[0].call_id, "same", "R1");
            let ContentBlock::ToolUse { name, .. } = &source.content[0] else {
                unreachable!()
            };
            assert_eq!(&selected[0].name, name, "R1 family source");
        }
    }
    #[test]
    fn synthetic_awaiting_result_keeps_its_run_source_or_refuses_ambiguity() {
        // R1 exact Awaiting without a ToolUse => Run source; R2 a later
        // ToolUse Message appears => retain that same earlier Run source;
        // R3 two Runs can own the orphan result => refusal, no iteration winner.
        let r = result("r");
        let pause = awaiting(10, "run");
        let pending = Pending {
            tool_use_id: "same".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let mut history = HashMap::from([(pause.cursor, pending.clone())]);
        let mut evidence = snapshot(vec![r.clone()], vec![30]);
        assert_eq!(
            sources(&evidence, std::slice::from_ref(&pause), &history, &r).unwrap()[0].source_id,
            "run",
            "R1"
        );
        evidence.messages.insert(0, call(1, "read"));
        evidence.message_commit_cursors.insert(0, 20);
        assert_eq!(
            sources(&evidence, std::slice::from_ref(&pause), &history, &r).unwrap()[0].source_id,
            "run",
            "R2"
        );
        let other = awaiting(20, "other-run");
        history.insert(other.cursor, pending);
        let orphan = snapshot(vec![r.clone()], vec![30]);
        assert!(
            sources(&orphan, &[pause, other], &history, &r).is_err(),
            "R3"
        );
    }
    #[test]
    fn unprovable_result_sources_refuse_the_projection() {
        // C1 misaligned, zero, or beyond-prefix result cursor; C2 orphan result;
        // C3 same Message id with a different payload. R1 each C1/C2/C3 =>
        // Corrupt instead of inventing a source or using a cached latest call.
        let r = result("r");
        let base = snapshot(vec![call(1, "read"), r.clone()], vec![1, 2]);
        for cursors in [vec![1], vec![1, 0], vec![1, 31]] {
            let mut damaged = base.clone();
            damaged.message_commit_cursors = cursors;
            assert!(
                sources(&damaged, &[], &HashMap::new(), &r).is_err(),
                "R1 C1"
            );
        }
        assert!(
            sources(
                &snapshot(vec![r.clone()], vec![2]),
                &[],
                &HashMap::new(),
                &r
            )
            .is_err(),
            "R1 C2"
        );
        let mut changed = r;
        changed.content = vec![ContentBlock::text("different")];
        assert!(
            sources(&base, &[], &HashMap::new(), &changed).is_err(),
            "R1 C3"
        );
    }
    #[test]
    fn current_ticket_overlay_reuses_the_first_unmatched_occurrence() {
        // C1 two unmatched same-id calls; C2 builtin/client current ticket.
        // R1 all rows => the shared no-ticket index with one current overlay
        // equals the existing ticket-aware fold; only its first call is visible.
        let messages = [call(1, "read"), call(2, "read")];
        for client_executed in [false, true] {
            let pending = Pending {
                tool_use_id: "same".into(),
                name: "read".into(),
                input: serde_json::json!({}),
                client_executed,
            };
            let shared = crate::committed_transcript_evidence(&messages, None);
            let prior = crate::committed_transcript_evidence(&messages, Some(&pending));
            for (index, message) in messages.iter().enumerate() {
                let overlay =
                    shared.project_message(message, &HashSet::new(), None, &[], Some(&pending));
                let existing = prior.project_message(message, &HashSet::new(), None, &[], None);
                assert_eq!(overlay.message, existing.message, "R1");
                assert_eq!(
                    overlay.fully_classified,
                    index == 0,
                    "R1 earliest unmatched"
                );
            }
        }
    }
}
