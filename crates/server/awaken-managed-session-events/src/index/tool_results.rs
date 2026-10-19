// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Read-only result attribution reuses the canonical occurrence fold and historical Awaiting predicate.
// @formal reason: One source-qualified result is derived from the same prefix; ambiguous or missing sources refuse projection.
//! Exact result sources, shared by root and child wire projection.

use super::{OrderIndex, ThreadIndex};
use crate::{ProjectionError, ToolResultSource};
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::{RunLifecycleEventKind, agent::content::ContentBlock};

impl OrderIndex<'_> {
    /// Legacy Messages carry no Run coordinate. Borrow the original matching
    /// law only over pauses after this Message committed and no later than
    /// this owning Awaiting; future Runs cannot alter an old source proof.
    fn validate_legacy_pending_source_scope(
        &self,
        message: &Message,
        message_cursor: u64,
        awaiting: &awaken_agent_contract::RunLifecycleEvent,
    ) -> Result<(), ProjectionError> {
        let as_of = self
            .evidence
            .lifecycle
            .iter()
            .filter(|event| {
                message_cursor <= event.source_commit_cursor
                    && event.source_commit_cursor <= awaiting.source_commit_cursor
            })
            .cloned()
            .collect::<Vec<_>>();
        let boundaries = crate::historical_message_boundaries(
            message,
            self.evidence.historical_pending,
            &as_of,
            &awaiting.thread_id.0,
            None,
        );
        if boundaries.iter().any(|(_, run)| *run != &awaiting.run_id) {
            return Err(ProjectionError::corrupt(
                "pending ToolUse has ambiguous historical Run sources",
            ));
        }
        Ok(())
    }

    /// The same occurrence law used by the transcript and synthetic renderer,
    /// cut at this exact accepted Awaiting commit, not the latest live ticket.
    pub fn pending_tool_source(
        &self,
        awaiting: &awaken_agent_contract::RunLifecycleEvent,
        pending: &awaken_session_application_contract::Pending,
    ) -> Result<ToolResultSource, ProjectionError> {
        if awaiting.kind != RunLifecycleEventKind::Awaiting
            || self.lifecycle_by_cursor.get(&awaiting.cursor).copied() != Some(awaiting)
            || self.evidence.historical_pending.get(&awaiting.cursor) != Some(pending)
        {
            return Err(ProjectionError::corrupt(
                "pending source is not its exact accepted Awaiting target",
            ));
        }
        let thread = self
            .threads
            .get(awaiting.thread_id.0.as_str())
            .ok_or_else(|| {
                ProjectionError::corrupt(
                    "accepted Awaiting target has no committed Thread snapshot",
                )
            })?;
        if !thread.aligned || thread.snapshot.thread_id != awaiting.thread_id {
            return Err(ProjectionError::corrupt(
                "pending source has misaligned committed Thread coordinates",
            ));
        }
        if awaiting.source_commit_cursor == 0
            || awaiting.source_commit_cursor > thread.snapshot.store_cursor
        {
            return Err(ProjectionError::corrupt(
                "pending source has no exact committed Awaiting coordinate",
            ));
        }
        let synthetic =
            crate::historical::needs_synthetic_awaiting_tool(thread.snapshot, awaiting, pending);
        // Reuse the canonical call index and FIFO result attribution. Later
        // results cannot consume an occurrence at this earlier Awaiting cut.
        let mut selected: Option<(ToolResultSource, bool)> = None;
        for &position in thread.transcript.call_message_indices(&pending.tool_use_id) {
            let message = &thread.snapshot.messages[position];
            let cursor = thread.snapshot.message_commit_cursors[position];
            if cursor == 0 || cursor > thread.snapshot.store_cursor {
                return Err(ProjectionError::corrupt(
                    "pending ToolUse has no exact committed Message coordinate",
                ));
            }
            if cursor > awaiting.source_commit_cursor {
                continue;
            }
            let source_run = message.id.assistant_coordinates().map(|(run, _, _)| run);
            let Some(name) = crate::historical_message_await_name(
                message,
                pending,
                &awaiting.run_id,
                source_run,
            ) else {
                continue;
            };
            let mut blocks = message
                .content
                .iter()
                .enumerate()
                .filter_map(|(ordinal, block)| {
                    matches!(block, ContentBlock::ToolUse { id, .. } if id == &pending.tool_use_id)
                        .then_some(ordinal)
                });
            let ordinal = blocks.next().ok_or_else(|| {
                ProjectionError::corrupt("indexed pending source has no matching ToolUse")
            })?;
            if blocks.next().is_some() {
                return Err(ProjectionError::corrupt(
                    "pending ToolUse has ambiguous public source coordinates",
                ));
            }
            if let Some(result) =
                thread
                    .transcript
                    .result_message_index(&message.id.0, ordinal, &pending.tool_use_id)
            {
                let result_cursor = thread.snapshot.message_commit_cursors[result];
                if result_cursor == 0 || result_cursor > thread.snapshot.store_cursor {
                    return Err(ProjectionError::corrupt(
                        "pending ToolResult has no exact committed coordinate",
                    ));
                }
                if result_cursor <= awaiting.source_commit_cursor {
                    continue;
                }
            }
            let unqualified = source_run.is_none();
            if unqualified {
                self.validate_legacy_pending_source_scope(message, cursor, awaiting)?;
            }
            if let Some((_, previous_unqualified)) = &selected {
                if unqualified || *previous_unqualified {
                    return Err(ProjectionError::corrupt(
                        "pending ToolUse has ambiguous historical Run sources",
                    ));
                }
                // Same-Run, ordered unmatched calls follow the existing FIFO
                // rule: the earliest occurrence owns the ticket. Do not pick
                // a latest Message merely because the prefix later grew.
                continue;
            }
            selected = Some((
                ToolResultSource {
                    source_id: message.id.0.clone(),
                    call_id: pending.tool_use_id.clone(),
                    name: name.to_string(),
                },
                unqualified,
            ));
        }
        if synthetic {
            return Ok(ToolResultSource {
                source_id: awaiting.run_id.0.clone(),
                call_id: pending.tool_use_id.clone(),
                name: pending.name.clone(),
            });
        }
        selected.map(|(source, _)| source).ok_or_else(|| {
            ProjectionError::corrupt("accepted Awaiting target has no unmatched ToolUse source")
        })
    }

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
use awaken_agent_contract::agent::run::{Record, RunState};
#[cfg(test)]
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
#[cfg(test)]
use awaken_agent_contract::{RunId, ThreadId};

#[cfg(test)]
pub(crate) fn snapshot(messages: Vec<Message>, cursors: Vec<u64>) -> RunRecoverySnapshot {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OrderEvidence, ParentTerminalEvidence};
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_agent_contract::agent::run::RunState;
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

    fn with_index<T>(
        snapshot: &RunRecoverySnapshot,
        lifecycle: &[RunLifecycleEvent],
        pending: &HashMap<RunLifecycleCursor, Pending>,
        inspect: impl FnOnce(&OrderIndex<'_>) -> T,
    ) -> T {
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
        inspect(&index)
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
            let selected = with_index(&evidence, &[], &HashMap::new(), |index| {
                index.tool_result_sources("thread", message)
            })
            .unwrap();
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
            with_index(&evidence, std::slice::from_ref(&pause), &history, |index| {
                index.tool_result_sources("thread", &r)
            })
            .unwrap()[0]
                .source_id,
            "run",
            "R1"
        );
        evidence.messages.insert(0, call(1, "read"));
        evidence.message_commit_cursors.insert(0, 20);
        assert_eq!(
            with_index(&evidence, std::slice::from_ref(&pause), &history, |index| {
                index.tool_result_sources("thread", &r)
            })
            .unwrap()[0]
                .source_id,
            "run",
            "R2"
        );
        let other = awaiting(20, "other-run");
        history.insert(other.cursor, pending);
        let orphan = snapshot(vec![r.clone()], vec![30]);
        assert!(
            with_index(&orphan, &[pause, other], &history, |index| index
                .tool_result_sources("thread", &r))
            .is_err(),
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
                with_index(&damaged, &[], &HashMap::new(), |index| index
                    .tool_result_sources("thread", &r))
                .is_err(),
                "R1 C1"
            );
        }
        assert!(
            with_index(
                &snapshot(vec![r.clone()], vec![2]),
                &[],
                &HashMap::new(),
                |index| index.tool_result_sources("thread", &r)
            )
            .is_err(),
            "R1 C2"
        );
        let mut changed = r;
        changed.content = vec![ContentBlock::text("different")];
        assert!(
            with_index(&base, &[], &HashMap::new(), |index| index
                .tool_result_sources("thread", &changed))
            .is_err(),
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
    #[test]
    fn pending_sources_use_their_own_awaiting_cut_and_existing_fifo() {
        // C1 same-id Message occurrences in one Run; C2 a result before or
        // after the selected Awaiting; C3 a later Run reuses the raw call.
        // R1 C1+unconsumed => earliest indexed FIFO source, never latest;
        // R2 result<=Awaiting consumes that exact source, selecting the next;
        // R3 result>Awaiting cannot consume its historical source; R4 C3
        // scopes the choice to the exact accepted Run. Every result lookup is
        // the existing transcript index; no current pending map or new fold.
        let pending = Pending {
            tool_use_id: "same".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let first = call(1, "read");
        let second = call(2, "read");
        let done = result("done");
        for (rule, cut, cursors, expected) in [
            ("R1 earliest unmatched", 10, vec![1, 2], &first),
            ("R2 consumed before pause", 10, vec![1, 2, 3], &second),
            (
                "R3 later result cannot rewrite pause",
                2,
                vec![1, 4, 5],
                &first,
            ),
        ] {
            let messages = if cursors.len() == 2 {
                vec![first.clone(), second.clone()]
            } else {
                vec![first.clone(), second.clone(), done.clone()]
            };
            let prefix = snapshot(messages, cursors);
            let pause = awaiting(cut, "run");
            let history = HashMap::from([(pause.cursor, pending.clone())]);
            let source = with_index(&prefix, std::slice::from_ref(&pause), &history, |index| {
                index.pending_tool_source(&pause, &pending)
            })
            .unwrap();
            assert_eq!(source.source_id, expected.id.0, "{rule}");
            assert_eq!(source.call_id, "same", "{rule} call");
            assert_eq!(source.name, "read", "{rule} family");
        }
        let mut other = second;
        other.id = MessageId::assistant(&RunId("other".into()), 1);
        let prefix = snapshot(vec![first.clone(), other.clone()], vec![1, 4]);
        let pauses = [awaiting(2, "run"), awaiting(5, "other")];
        let history = pauses
            .iter()
            .map(|pause| (pause.cursor, pending.clone()))
            .collect();
        for (pause, expected) in pauses.iter().zip([&first, &other]) {
            let source = with_index(&prefix, &pauses, &history, |index| {
                index.pending_tool_source(pause, &pending)
            })
            .unwrap();
            assert_eq!(source.source_id, expected.id.0, "R4 exact Run");
        }
    }

    #[test]
    fn pending_synthetic_source_survives_later_message_and_rejects_unknown_coordinates() {
        // S1 no matching Message as of an audited pause => original Run
        // source; S2 the matching Message commits later => same source;
        // S3 missing Thread, misaligned/zero/beyond-prefix coordinates,
        // conflicting target or ambiguous same-public-source => typed refusal.
        // The as-of synthetic predicate is the production renderer's original
        // one; corrupted evidence never falls back to latest/raw call identity.
        let pending = Pending {
            tool_use_id: "same".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let pause = awaiting(10, "run");
        let history = HashMap::from([(pause.cursor, pending.clone())]);
        for prefix in [
            snapshot(vec![], vec![]),
            snapshot(vec![call(1, "read")], vec![20]),
        ] {
            let source = with_index(&prefix, std::slice::from_ref(&pause), &history, |index| {
                index.pending_tool_source(&pause, &pending)
            })
            .unwrap();
            assert_eq!(source.source_id, "run", "S1/S2 original synthetic identity");
            assert_eq!(source.call_id, "same", "S1/S2 exact call");
        }
        for cursors in [vec![], vec![0], vec![31]] {
            let prefix = snapshot(vec![call(1, "read")], cursors);
            assert!(
                with_index(&prefix, std::slice::from_ref(&pause), &history, |index| {
                    index.pending_tool_source(&pause, &pending)
                })
                .is_err(),
                "S3 committed coordinates"
            );
        }
        let mut duplicate = call(1, "read");
        duplicate.content.push(ContentBlock::tool_use(
            "same",
            "read",
            serde_json::json!({}),
        ));
        let prefix = snapshot(vec![duplicate], vec![1]);
        assert!(
            with_index(&prefix, std::slice::from_ref(&pause), &history, |index| {
                index.pending_tool_source(&pause, &pending)
            })
            .is_err(),
            "S3 one Message cannot encode two identical public tool coordinates"
        );
        let mut legacy = [call(1, "read"), call(2, "read")];
        legacy[0].id = MessageId("legacy-first".into());
        legacy[1].id = MessageId("legacy-second".into());
        let prefix = snapshot(legacy.to_vec(), vec![1, 2]);
        assert!(
            with_index(&prefix, std::slice::from_ref(&pause), &history, |index| {
                index.pending_tool_source(&pause, &pending)
            })
            .is_err(),
            "S3 unprovable historical Run ownership"
        );
        let prefix = snapshot(vec![], vec![]);
        let mut missing = pause.clone();
        missing.thread_id = ThreadId("missing".into());
        assert!(
            with_index(&prefix, std::slice::from_ref(&missing), &history, |index| {
                index.pending_tool_source(&missing, &pending)
            })
            .is_err(),
            "S3 missing Thread"
        );
        let mut conflicting = pending;
        conflicting.input = serde_json::json!({"changed":true});
        assert!(
            with_index(&prefix, std::slice::from_ref(&pause), &history, |index| {
                index.pending_tool_source(&pause, &conflicting)
            })
            .is_err(),
            "S3 exact recorded target cannot change"
        );
    }
    #[test]
    fn legacy_pending_source_ownership_is_cut_at_each_own_awaiting() {
        // C1 one unqualified legacy Message commits at4 and Run1 awaits5;
        // C2 Run2 awaits10 with the same raw call/name/input; C3 a reader sees
        // the full prefix at once. R1=C1+C2+C3, query Await5 => the same original
        // Message (future Run2 cannot rebind or invalidate Run1's source).
        // R2=same prefix, query Await10 => typed ambiguity: the single legacy
        // Message cannot prove which Run owns it. No new id, synthetic source,
        // heuristic or compatibility state may fill that missing proof.
        let mut message = call(1, "read");
        message.id = MessageId("legacy-tool".into());
        let mut prefix = snapshot(vec![message.clone()], vec![4]);
        prefix.runs[0].state = RunState::Awaiting;
        prefix.runs.push(awaken_agent_contract::agent::run::Record {
            id: RunId("later-run".into()),
            thread_id: ThreadId("thread".into()),
            state: RunState::Awaiting,
        });
        prefix.claimed_run_id = RunId("later-run".into());
        prefix.latest_run_id = Some(RunId("later-run".into()));
        let pending = Pending {
            tool_use_id: "same".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let pauses = [awaiting(5, "run"), awaiting(10, "later-run")];
        let history = pauses
            .iter()
            .map(|pause| (pause.cursor, pending.clone()))
            .collect();
        let first = with_index(&prefix, &pauses[..1], &history, |index| {
            index.pending_tool_source(&pauses[0], &pending)
        })
        .unwrap();
        assert_eq!(first.source_id, message.id.0, "R1 original Message");
        let first_in_full = with_index(&prefix, &pauses, &history, |index| {
            index.pending_tool_source(&pauses[0], &pending)
        })
        .unwrap();
        assert_eq!(
            first_in_full, first,
            "R1 future Run cannot change old Awaiting"
        );
        let error = with_index(&prefix, &pauses, &history, |index| {
            index.pending_tool_source(&pauses[1], &pending)
        })
        .expect_err("R2 a single legacy Message has two possible Run owners");
        assert_eq!(
            error,
            ProjectionError::corrupt("pending ToolUse has ambiguous historical Run sources"),
            "R2 typed refusal rather than an arbitrary owner"
        );
    }
    #[test]
    fn legacy_source_cannot_borrow_a_pause_before_its_message_committed() {
        // L1 the first legacy call commits4, Awaiting5, result6; L2 the same
        // raw call has another legacy Message9 and Awaiting10. R3=L1+L2 =>
        // the current source is Message9: old Await5 cannot own a Message
        // that had not committed. Its own old source remains Message4 even
        // when the full prefix now includes its result and the later Run.
        // This pins the lower as well as upper bound of the one shared
        // historical_message_boundaries view; no new legacy selection law.
        let mut first = call(1, "read");
        first.id = MessageId("first-legacy-tool".into());
        let mut second = call(2, "read");
        second.id = MessageId("second-legacy-tool".into());
        let prefix = snapshot(
            vec![first.clone(), result("first-done"), second.clone()],
            vec![4, 6, 9],
        );
        let pending = Pending {
            tool_use_id: "same".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let pauses = [awaiting(5, "run"), awaiting(10, "later-run")];
        let history = pauses
            .iter()
            .map(|pause| (pause.cursor, pending.clone()))
            .collect();
        for (pause, message) in pauses.iter().zip([&first, &second]) {
            let source = with_index(&prefix, &pauses, &history, |index| {
                index.pending_tool_source(pause, &pending)
            })
            .unwrap();
            assert_eq!(source.source_id, message.id.0, "R3 own committed source");
        }
    }
}
