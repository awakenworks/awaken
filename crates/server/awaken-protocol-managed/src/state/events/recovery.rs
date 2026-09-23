//! Committed recovery snapshots, pending-tool identity resolution, and projection qualification.

use super::*;

struct LifecycleMergeEntry {
    event: RunLifecycleEvent,
    stream_index: usize,
}

impl PartialEq for LifecycleMergeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.event.cursor == other.event.cursor && self.stream_index == other.stream_index
    }
}

impl Eq for LifecycleMergeEntry {}

impl PartialOrd for LifecycleMergeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LifecycleMergeEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; reverse cursor/source comparison to pop the
        // smallest cursor first. Source order preserves sort_by_key's prior
        // stable tie behavior for equal cursors.
        other
            .event
            .cursor
            .cmp(&self.event.cursor)
            .then_with(|| other.stream_index.cmp(&self.stream_index))
    }
}

fn merge_ordered_lifecycle_streams(streams: Vec<Vec<RunLifecycleEvent>>) -> Vec<RunLifecycleEvent> {
    debug_assert!(streams.iter().all(|events| {
        events
            .windows(2)
            .all(|pair| pair[0].cursor <= pair[1].cursor)
    }));
    let capacity = streams.iter().map(Vec::len).sum();
    let mut streams = streams.into_iter().map(Vec::into_iter).collect::<Vec<_>>();
    let mut heap = std::collections::BinaryHeap::with_capacity(streams.len());
    for (stream_index, stream) in streams.iter_mut().enumerate() {
        if let Some(event) = stream.next() {
            heap.push(LifecycleMergeEntry {
                event,
                stream_index,
            });
        }
    }
    let mut merged = Vec::with_capacity(capacity);
    while let Some(entry) = heap.pop() {
        let stream_index = entry.stream_index;
        merged.push(entry.event);
        if let Some(event) = streams[stream_index].next() {
            heap.push(LifecycleMergeEntry {
                event,
                stream_index,
            });
        }
    }
    merged
}

#[cfg(test)]
mod lifecycle_merge_tests {
    use super::*;

    fn event(cursor: u64, stream: usize) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: awaken_agent_contract::RunLifecycleCursor(cursor),
            source_commit_cursor: cursor,
            thread_id: awaken_agent_contract::ThreadId(format!("thread-{stream}")),
            run_id: awaken_agent_contract::RunId(format!("run-{stream}-{cursor}")),
            kind: awaken_agent_contract::RunLifecycleEventKind::Running,
            state: awaken_agent_contract::agent::run::RunState::Running,
            await_reason: None,
        }
    }

    #[test]
    fn ordered_lifecycle_merge_matches_stable_full_sort() {
        // Causes: C1 zero/one/many source streams; C2 each stream is empty or
        // carries an ordered subset; C3 cursors may tie across streams. Effects:
        // E1 every event is retained once; E2 cursors are globally ordered; E3
        // equal cursors retain input-stream order. Exhaust every assignment of
        // six distinct cursors to three streams, then cover equal cursors.
        assert!(merge_ordered_lifecycle_streams(Vec::new()).is_empty(), "M1");
        for assignment in 0_u32..3_u32.pow(6) {
            let mut value = assignment;
            let mut streams = vec![Vec::new(), Vec::new(), Vec::new()];
            for cursor in 1..=6 {
                let stream = (value % 3) as usize;
                value /= 3;
                streams[stream].push(event(cursor, stream));
            }
            let mut expected = streams.iter().flatten().cloned().collect::<Vec<_>>();
            expected.sort_by_key(|item| item.cursor);
            let merged = merge_ordered_lifecycle_streams(streams);
            assert_eq!(
                merged.iter().map(|item| item.cursor.0).collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|item| item.cursor.0)
                    .collect::<Vec<_>>(),
                "M2 assignment {assignment}",
            );
        }
        let tied = merge_ordered_lifecycle_streams(vec![
            vec![event(1, 0), event(3, 0)],
            vec![event(1, 1), event(2, 1)],
        ]);
        assert_eq!(
            tied.iter()
                .map(|item| (item.cursor.0, item.thread_id.0.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "thread-0"),
                (1, "thread-1"),
                (2, "thread-1"),
                (3, "thread-0")
            ],
            "M3 stable tie order",
        );
    }

    #[test]
    fn lifecycle_merge_scaling_matrix_preserves_every_ordered_prefix() {
        // Scaling matrix: H=100/1k/10k lifecycle facts x S=1/8/25 ordered
        // Thread streams. Effects: every fact appears once, global cursor order
        // is monotonic, and elapsed observations remain diagnostic rather than a
        // machine-speed correctness threshold. K3D owns the public p95 gate.
        for total in [100_usize, 1_000, 10_000] {
            for source_count in [1_usize, 8, 25] {
                let mut streams = vec![Vec::new(); source_count];
                for cursor in 1..=total {
                    let source = cursor % source_count;
                    streams[source].push(event(cursor as u64, source));
                }
                let started = std::time::Instant::now();
                let merged = merge_ordered_lifecycle_streams(streams);
                let elapsed = started.elapsed();
                assert_eq!(merged.len(), total, "H={total} S={source_count}");
                assert!(
                    merged
                        .windows(2)
                        .all(|pair| pair[0].cursor <= pair[1].cursor),
                    "H={total} S={source_count}"
                );
                eprintln!(
                    "LIFECYCLE_MERGE_BENCH {{\"history\":{total},\"sources\":{source_count},\"elapsed_us\":{}}}",
                    elapsed.as_micros(),
                );
            }
        }
    }
}

#[cfg(any(test, kani))]
mod selection_tests;

/// Facts selected from the same committed snapshot, before materializing any
/// public Pending payload. Cardinality belongs to the outer snapshot lookup.
#[derive(Clone, Copy)]
enum PendingTicketObservation<'a> {
    Absent,
    Multiple,
    One(&'a awaken_agent_contract::agent::awaiting::ResumeTicket),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingTicketAdmission {
    Inactive,
    Missing { positive_awaiting: bool },
    Multiple,
    Inconsistent,
    Admit,
}

/// The one strict ticket-admission decision, consumed by the snapshot entry
/// below. Run truth dominates residual, duplicate and corrupt ticket rows.
/// Unknown legacy lifecycle preserves a unique exact ticket. Identity remains
/// owned by the neutral recovery contract; this adapter does not reimplement it.
fn pending_ticket_admission(
    current_state: Option<&awaken_agent_contract::agent::run::RunState>,
    current_run_id: &awaken_agent_contract::RunId,
    thread_id: &awaken_agent_contract::ThreadId,
    observation: PendingTicketObservation<'_>,
) -> PendingTicketAdmission {
    use awaken_agent_contract::agent::run::RunState;
    if matches!(current_state, Some(RunState::Running | RunState::Ended(_))) {
        return PendingTicketAdmission::Inactive;
    }
    match observation {
        PendingTicketObservation::Absent => PendingTicketAdmission::Missing {
            positive_awaiting: matches!(current_state, Some(RunState::Awaiting)),
        },
        PendingTicketObservation::Multiple => PendingTicketAdmission::Multiple,
        PendingTicketObservation::One(ticket) => {
            if awaken_agent_contract::thread::read::recovery::validate_resume_ticket_owner(
                ticket,
                current_run_id,
                thread_id,
            )
            .is_err()
            {
                PendingTicketAdmission::Inconsistent
            } else {
                PendingTicketAdmission::Admit
            }
        }
    }
}

/// Fold one lifecycle fact into the exact Awaiting commit selected for a
/// reply. This is the heap-free production owner for the fenced maximum; the
/// surrounding method supplies real Thread/Run/kind equality observations.
const fn advance_answered_pending_commit(
    selected: Option<u64>,
    same_thread: bool,
    same_run: bool,
    awaiting: bool,
    source_commit_cursor: u64,
    snapshot_store_cursor: u64,
) -> Option<u64> {
    if same_thread
        && same_run
        && awaiting
        && source_commit_cursor != 0
        && source_commit_cursor <= snapshot_store_cursor
        && match selected {
            Some(current) => source_commit_cursor > current,
            None => true,
        }
    {
        Some(source_commit_cursor)
    } else {
        selected
    }
}

impl ManagedState {
    /// Resolve the one current Run coordinate carried by a recovery prefix.
    /// Strict admission, lenient projection, and interruption must not each
    /// invent a different latest-versus-claimed fallback or state lookup.
    pub(super) fn current_recovery_run(
        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> (
        &awaken_agent_contract::agent::run::Id,
        Option<&awaken_agent_contract::agent::run::RunState>,
    ) {
        let run_id = snapshot
            .latest_run_id
            .as_ref()
            .unwrap_or(&snapshot.claimed_run_id);
        let state = snapshot
            .runs
            .iter()
            .find(|run| &run.id == run_id)
            .map(|run| &run.state);
        (run_id, state)
    }

    /// Return the latest well-formed state-change audit for one Run. Both the
    /// strict reply classifier and the read-only fallback use this single
    /// historical projection rule; malformed payloads confer no authority.
    fn latest_recovery_run_state_change<'a>(
        snapshot: &'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Option<(
        &'a awaken_agent_contract::audit::record::Record,
        awaken_agent_contract::agent::run::RunState,
    )> {
        let event = snapshot.events.iter().rev().find(|event| {
            &event.run_id == run_id
                && event.kind == awaken_agent_contract::audit::kind::Kind::RunStateChanged
        })?;
        let state = event
            .payload
            .get("state")
            .cloned()
            .and_then(|state| serde_json::from_value(state).ok())?;
        Some((event, state))
    }

    /// Apply the canonical coordinated-Thread terminal policy to one committed
    /// disposition and latest Run state. This is a pure classifier; link and
    /// recovery-snapshot lookup remain the caller's responsibility.
    pub(super) fn coordinated_thread_is_interruptible(
        target: &CoordinatedThreadTarget,
        disposition: awaken_agent_contract::ThreadDisposition,
        latest_state: Option<&awaken_agent_contract::agent::run::RunState>,
    ) -> bool {
        if disposition == awaken_agent_contract::ThreadDisposition::Archived {
            return false;
        }
        latest_state.is_none_or(|state| {
            !coordinated_child_run_is_terminal(
                matches!(target, CoordinatedThreadTarget::Advisor { .. }),
                state,
            )
        })
    }

    /// Derive the complete lifecycle from already observed atomic prefixes.
    /// Admission and projection use this same helper so neither
    /// can silently truncate or reinterpret Awaiting commit coordinates.
    pub(super) fn committed_lifecycle_prefix<'a>(
        snapshots: impl IntoIterator<
            Item = &'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        >,
    ) -> Result<Vec<RunLifecycleEvent>, StateError> {
        let mut lifecycle_streams = Vec::new();
        for snapshot in snapshots {
            let page = snapshot
                .lifecycle_events_after(
                    awaken_agent_contract::RunLifecycleCursor::default(),
                    usize::MAX,
                )
                .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
            lifecycle_streams.push(page.events);
        }
        Ok(merge_ordered_lifecycle_streams(lifecycle_streams))
    }

    /// Classify one coordinated child from the same Runtime prefix already read
    /// for Event admission. Link membership is the topology authority; committed
    /// Thread disposition and latest Run state are its terminal authorities.
    fn coordinated_thread_is_terminal(
        link: &CoordinatedThreadLink,
        snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<bool, StateError> {
        let Some(snapshot) = snapshot else {
            // A link may commit before the child has a first recovery snapshot.
            // It is still a valid cancellable dispatch target, not an unknown id.
            return Ok(false);
        };
        let disposition =
            awaken_agent_contract::thread_disposition_from_committed_state(&snapshot.state)
                .map_err(|error| {
                    StateError::Run(RunError::internal(format!(
                        "recover coordinated Thread disposition: {error}"
                    )))
                })?;
        let latest_run_id = snapshot.latest_run_id.as_ref().ok_or_else(|| {
            StateError::Run(RunError::internal(
                "coordinated Thread recovery omitted its latest committed Run",
            ))
        })?;
        let latest_state = snapshot
            .runs
            .iter()
            .find(|run| &run.id == latest_run_id)
            .map(|run| &run.state)
            .ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "coordinated Thread recovery omitted its latest Run state",
                ))
            })?;
        Ok(!Self::coordinated_thread_is_interruptible(
            &link.target,
            disposition,
            Some(latest_state),
        ))
    }

    /// Resolve the public optional Thread selector onto the Runtime's canonical
    /// Thread keys. The caller supplies the one link/snapshot prefix it already
    /// read for this Event batch; the disposable Managed Thread DTO cache is not
    /// a second membership or terminal authority.
    pub(super) fn interrupt_targets(
        session_id: &str,
        requested_thread_id: Option<&str>,
        links: &[CoordinatedThreadLink],
        child_snapshots: &std::collections::HashMap<
            String,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        >,
    ) -> Result<Vec<SessionThreadTarget>, StateError> {
        if let Some(thread_id) = requested_thread_id {
            let target = session_thread_target_from_public(session_id, thread_id);
            let SessionThreadTarget::Child(child_thread_id) = &target else {
                return Ok(vec![target]);
            };
            let link = links
                .iter()
                .find(|link| link.session_id == session_id && link.thread_id == *child_thread_id)
                .ok_or_else(|| {
                    StateError::Run(RunError::bad_request(
                        "session_thread_id does not name a thread in this session",
                    ))
                })?;
            if Self::coordinated_thread_is_terminal(link, child_snapshots.get(&child_thread_id.0))?
            {
                return Err(StateError::Run(RunError::bad_request(
                    "an archived or terminated session thread cannot be interrupted",
                )));
            }
            return Ok(vec![target]);
        }

        let mut targets = vec![SessionThreadTarget::Primary];
        for link in links.iter().filter(|link| link.session_id == session_id) {
            if !Self::coordinated_thread_is_terminal(link, child_snapshots.get(&link.thread_id.0))?
            {
                targets.push(SessionThreadTarget::Child(link.thread_id.clone()));
            }
        }
        Ok(targets)
    }

    /// Read one logical Thread through the Runtime's existing recovery snapshot
    /// boundary. Transcript, resume ticket, disposition and commit watermark are
    /// one prefix; Managed never assembles those facts from independent reads.
    pub(super) async fn recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        StateError,
    > {
        let snapshot = self
            .application
            .session_thread_recovery_snapshot(
                &awaken_session_types::SessionId::from(session_id),
                thread_id,
            )
            .await
            .map_err(StateError::Run)?;
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.thread_id.0 != thread_id)
        {
            return Err(StateError::Run(RunError::internal(
                "Runtime returned a recovery snapshot for a different Session Thread",
            )));
        }
        Ok(snapshot)
    }

    /// Read each coordinated Thread through that same consistency boundary.
    pub(super) async fn coordinated_recovery_snapshots(
        &self,
        session_id: &str,
        links: &[CoordinatedThreadLink],
    ) -> Result<
        std::collections::HashMap<
            String,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        >,
        StateError,
    > {
        let mut children = std::collections::HashMap::new();
        for link in links {
            let Some(snapshot) = self
                .recovery_snapshot(session_id, &link.thread_id.0)
                .await?
            else {
                continue;
            };
            children.insert(link.thread_id.0.clone(), snapshot);
        }
        Ok(children)
    }

    /// Collect one deterministic coordinated-Thread prefix. Callers compare two
    /// collections around projection reads so independent child commits cannot
    /// form a mixed public prefix.
    pub(super) async fn coordinated_projection_prefix(
        &self,
        session_id: &str,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        (
            Vec<CoordinatedThreadLink>,
            std::collections::HashMap<
                String,
                awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
            >,
        ),
        StateError,
    > {
        let mut links = self
            .application
            .coordinated_threads(
                &awaken_session_types::SessionId::from(session_id),
                observed_root,
            )
            .await
            .map_err(StateError::Run)?;
        let snapshots = self
            .coordinated_recovery_snapshots(session_id, &links)
            .await?;
        for link in &mut links {
            if let Some(snapshot) = snapshots.get(&link.thread_id.0) {
                link.latest_run_id = snapshot.latest_run_id.clone();
            }
        }
        links.sort_by(|left, right| left.thread_id.0.cmp(&right.thread_id.0));
        Ok((links, snapshots))
    }

    pub(super) fn pending_ticket_from_recovery_snapshot(
        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<Option<(awaken_agent_contract::agent::run::Id, String, Pending)>, StateError> {
        let (run_id, current_state) = Self::current_recovery_run(snapshot);
        // The Runtime resumes one committed Awaiting ticket at a time. An
        // ActiveToolBatch may retain later Requested calls, but they are not
        // externally answerable until the current ticket is consumed and the
        // Runtime advances the batch. Never pick arbitrarily if a malformed or
        // future producer publishes two tickets for the same current Run.
        let mut tickets = snapshot
            .resume_tickets
            .iter()
            .filter(|ticket| &ticket.run_id == run_id);
        let ticket = tickets.next();
        let observation = match ticket {
            None => PendingTicketObservation::Absent,
            Some(_) if tickets.next().is_some() => PendingTicketObservation::Multiple,
            Some(ticket) => PendingTicketObservation::One(&ticket.ticket),
        };
        let admission =
            pending_ticket_admission(current_state, run_id, &snapshot.thread_id, observation);
        match admission {
            PendingTicketAdmission::Inactive => return Ok(None),
            PendingTicketAdmission::Multiple => {
                return Err(StateError::projection_recovery_required(
                    "Runtime recovery exposed multiple answerable tickets for one Run; interrupt the Session to settle it",
                ));
            }
            PendingTicketAdmission::Inconsistent => {
                return Err(StateError::projection_recovery_required(
                    "Runtime recovery exposed an inconsistent pending-tool identity; interrupt the Session to settle it",
                ));
            }
            PendingTicketAdmission::Missing { positive_awaiting } => {
                if snapshot
                    .resume_tickets
                    .iter()
                    .any(|ticket| &ticket.ticket.run_id == run_id)
                {
                    return Err(StateError::projection_recovery_required(
                        "Runtime recovery exposed an inconsistent pending-tool identity; interrupt the Session to settle it",
                    ));
                }
                let audit_state = Self::latest_recovery_run_state_change(snapshot, run_id)
                    .map(|(_, state)| state);
                if positive_awaiting
                    || matches!(
                        audit_state,
                        Some(awaken_agent_contract::agent::run::RunState::Awaiting)
                    )
                {
                    return Err(StateError::projection_recovery_required(
                        "Runtime recovery exposed an Awaiting Run without its answerable ticket; interrupt the Session to settle it",
                    ));
                }
                return Ok(None);
            }
            PendingTicketAdmission::Admit => {}
        }
        let ticket = ticket.expect("admission requires the selected unique ticket");
        let pending = Pending::from_resume_ticket(&ticket.ticket);
        Ok(pending.map(|pending| {
            (
                ticket.run_id.clone(),
                ticket.ticket.correlation_id.clone(),
                pending,
            )
        }))
    }

    pub(super) fn pending_from_recovery_snapshot(
        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<Option<Pending>, StateError> {
        let ticket_error = match Self::pending_ticket_from_recovery_snapshot(snapshot) {
            Ok(Some((_, _, pending))) => return Ok(Some(pending)),
            Ok(None) => None,
            Err(error) => {
                tracing::error!(
                    awaken.thread.id = %snapshot.thread_id.0,
                    %error,
                    "projecting a damaged pending-tool ticket from its committed Awaiting audit"
                );
                Some(error)
            }
        };

        let (run_id, current_state) = Self::current_recovery_run(snapshot);
        if !matches!(
            current_state,
            Some(awaken_agent_contract::agent::run::RunState::Awaiting)
        ) {
            return ticket_error.map_or(Ok(None), Err);
        }
        let Some((event, awaken_agent_contract::agent::run::RunState::Awaiting)) =
            Self::latest_recovery_run_state_change(snapshot, run_id)
        else {
            return ticket_error.map_or(Ok(None), Err);
        };
        let pending = event
            .payload
            .get("await_target")
            .cloned()
            .and_then(|target| {
                serde_json::from_value::<awaken_agent_contract::agent::awaiting::AwaitTarget>(
                    target,
                )
                .ok()
            })
            .and_then(|target| Pending::from_await_target(&target));
        match (pending, ticket_error) {
            (Some(pending), _) => Ok(Some(pending)),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(None),
        }
    }

    pub(super) fn public_tool_thread_id(session_id: &str, owner_thread_id: Option<&str>) -> String {
        public_thread_id(session_id, owner_thread_id.unwrap_or(session_id))
    }

    pub(super) fn tool_call_sources(
        messages: &[awaken_agent_contract::agent::message::Message],
    ) -> std::collections::HashMap<String, std::collections::VecDeque<String>> {
        let mut sources =
            std::collections::HashMap::<String, std::collections::VecDeque<String>>::new();
        for message in messages.iter().filter(|message| {
            message.role == awaken_agent_contract::agent::message::Role::Assistant
        }) {
            for block in &message.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    sources
                        .entry(id.clone())
                        .or_default()
                        .push_back(message.id.0.clone());
                }
            }
        }
        sources
    }

    /// Qualify every tool-use identity at the sole event append boundary, and
    /// rewrite all references to that public identity. The Runtime call id stays
    /// inside the reversible encoding and is decoded only at command admission.
    pub(super) fn qualify_tool_projection(
        &self,
        record: &SessionRecord,
        owner_thread_id: Option<&str>,
        messages: &[awaken_agent_contract::agent::message::Message],
        synthetic_source_run_id: Option<&awaken_agent_contract::agent::run::Id>,
        events: &mut Vec<ProjectedEvent>,
    ) -> Result<(), StateError> {
        let public_thread_id = Self::public_tool_thread_id(&record.session.id, owner_thread_id);
        let declared_custom_tools = owner_thread_id
            .and_then(|thread_id| {
                record
                    .child_threads
                    .iter()
                    .find(|thread| thread.id == thread_id)
                    .and_then(|thread| thread.agent.as_agent())
                    .map(|agent| agent.tools.as_slice())
            })
            .unwrap_or(record.session.agent.tools.as_slice())
            .iter()
            .filter_map(|tool| match tool {
                awaken_session_configuration_contract::AgentTool::Custom { name, .. } => {
                    Some(name.as_str())
                }
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let mut sources = Self::tool_call_sources(messages);
        let tool_index = record.projected_tool_index_for(owner_thread_id);
        let mut latest = tool_index.latest;
        let client_settled_occurrences = tool_index.client_settled_occurrences;
        let mut qualified = Vec::with_capacity(events.len());
        for mut event in std::mem::take(events) {
            // Runtime's neutral pending state says only that the client must
            // supply the result. Managed distinguishes authored custom tools
            // from self-hosted Agent-toolset execution at this sole projection
            // boundary, using the frozen Session/child Agent definition. It runs
            // before the recorded decision is applied, so a converted call is an
            // Agent tool use like any other and the record reaches it; the `allow`
            // below is only what a session older than those records reports.
            if let OutboundKind::AgentCustomToolUse {
                name,
                input,
                session_thread_id,
            } = &event.kind
                && !declared_custom_tools.contains(name.as_str())
            {
                event.kind = OutboundKind::AgentToolUse {
                    name: name.clone(),
                    input: input.clone(),
                    evaluated_permission: Some(EvaluatedPermission::Allow),
                    evaluation: None,
                    session_thread_id: session_thread_id.clone(),
                };
            }
            // The recorded gate decision replaces whatever the transcript-only
            // fold inferred. A refused call and an executed call that errored
            // look identical in the transcript, so only this record can tell
            // them apart — and reporting a refusal as `allow` tells a client the
            // call it blocked went through.
            if let Some(recorded) = event
                .source_tool_call_id
                .as_deref()
                .and_then(|call_id| record.permission_decisions.get(call_id))
            {
                match &mut event.kind {
                    OutboundKind::AgentToolUse {
                        evaluated_permission,
                        evaluation,
                        ..
                    }
                    | OutboundKind::AgentMcpToolUse {
                        evaluated_permission,
                        evaluation,
                        ..
                    } => {
                        *evaluated_permission = Some(recorded.decision);
                        *evaluation = recorded.evaluation.clone();
                    }
                    _ => {}
                }
            }
            if ProjectedToolUseFamily::from_kind(&event.kind).is_some() {
                let runtime_call_id = event.id.as_deref().ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "Managed tool-use projection is missing its Runtime call id",
                    ))
                })?;
                let source_id = sources
                    .get_mut(runtime_call_id)
                    .and_then(std::collections::VecDeque::pop_front)
                    .or_else(|| synthetic_source_run_id.map(|run_id| run_id.0.clone()))
                    .ok_or_else(|| {
                        StateError::Run(RunError::internal(format!(
                            "Managed tool call `{runtime_call_id}` has no committed message or Run identity"
                        )))
                    })?;
                let public_id =
                    managed_tool_event_id(&public_thread_id, &source_id, runtime_call_id);
                latest.insert(runtime_call_id.to_string(), public_id.clone());
                event.id = Some(public_id);
            }
            match &mut event.kind {
                OutboundKind::AgentToolResult { tool_use_id, .. } => {
                    // Managed Agents publishes a client-owned result as the
                    // admitted `user.custom_tool_result`. Runtime must retain
                    // its neutral ToolResult for model continuation, but
                    // exposing that same fact again as `agent.tool_result`
                    // violates the public tool family and makes an exact SDK
                    // history internally contradictory.
                    let public_tool_use_id = latest.get(tool_use_id).cloned().ok_or_else(|| {
                        StateError::Run(RunError::internal(format!(
                            "Managed tool result `{tool_use_id}` has no preceding tool-use identity"
                        )))
                    })?;
                    if client_settled_occurrences.contains(&public_tool_use_id) {
                        continue;
                    }
                    *tool_use_id = public_tool_use_id;
                }
                OutboundKind::AgentMcpToolResult {
                    mcp_tool_use_id, ..
                } => {
                    *mcp_tool_use_id = latest.get(mcp_tool_use_id).cloned().ok_or_else(|| {
                        StateError::Run(RunError::internal(
                            "Managed MCP result has no preceding tool-use identity",
                        ))
                    })?;
                }
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::RequiresAction { event_ids },
                } => {
                    for event_id in event_ids {
                        *event_id = latest.get(event_id).cloned().ok_or_else(|| {
                            StateError::Run(RunError::internal(
                                "Managed requires_action has no answerable tool-use identity",
                            ))
                        })?;
                    }
                }
                _ => {}
            }
            qualified.push(event);
        }
        *events = qualified;
        Ok(())
    }

    pub(super) fn invalid_tool_reply() -> StateError {
        StateError::Run(RunError::bad_request(
            "tool result does not match the pending tool event",
        ))
    }

    /// Select the immutable Awaiting commit from the same fenced recovery
    /// prefix that exposed a reply candidate. Retaining this coordinate with
    /// the root command prevents a post-delivery read racing the next Awaiting.
    pub(super) fn answered_pending_commit_cursor(
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
        snapshot_store_cursor: u64,
        lifecycle_events: &[RunLifecycleEvent],
    ) -> Result<u64, StateError> {
        lifecycle_events
            .iter()
            .fold(None, |selected, event| {
                advance_answered_pending_commit(
                    selected,
                    &event.thread_id == thread_id,
                    &event.run_id == run_id,
                    event.kind == RunLifecycleEventKind::Awaiting,
                    event.source_commit_cursor,
                    snapshot_store_cursor,
                )
            })
            .ok_or_else(|| {
                StateError::Run(RunError::unavailable(
                    "committed pending lifecycle is not yet visible",
                ))
            })
    }

    /// Resolve a tool reply once at batch admission from committed pending
    /// snapshots. Anthropic Managed multiagent replies route by the qualified
    /// tool-use Event id. A pre-qualification id has no embedded owner, so it is
    /// safe only when exactly one pending call of the requested reply kind
    /// matches across the Session (upgrade recovery for a previously emitted
    /// raw call id).
    pub(super) fn resolve_tool_reply(
        session_id: &str,
        public_event_id: &str,
        family: ToolReplyFamily,
        candidates: &[PendingToolReplyCandidate],
    ) -> Result<ResolvedToolReply, StateError> {
        if let Some(identity) = decode_managed_tool_event_id(public_event_id) {
            let target = session_thread_target_from_public(session_id, identity.thread_id);
            let candidate = candidates
                .iter()
                .find(|candidate| {
                    candidate.key.target == target
                        && candidate.key.runtime_call_id == identity.call_id
                        && candidate.key.client_executed == family.requires_client_execution()
                        && candidate
                            .projected_family
                            .is_some_and(|kind| family.accepts(kind))
                        && candidate.projected_event_id.as_deref() == Some(public_event_id)
                })
                .ok_or_else(Self::invalid_tool_reply)?;
            return Ok(ResolvedToolReply {
                key: candidate.key.clone(),
                answered_pending_commit_cursor: candidate.answered_pending_commit_cursor,
            });
        }

        // An old event id carries only the Runtime-local call id. Prove the
        // call/type pair has exactly one pending owner across the Session.
        let mut matching = candidates.iter().filter(|candidate| {
            candidate.key.runtime_call_id == public_event_id
                && candidate.key.client_executed == family.requires_client_execution()
                && candidate
                    .projected_family
                    .is_some_and(|kind| family.accepts(kind))
        });
        let candidate = matching.next().ok_or_else(Self::invalid_tool_reply)?;
        if matching.next().is_some() {
            return Err(Self::invalid_tool_reply());
        }
        Ok(ResolvedToolReply {
            key: candidate.key.clone(),
            answered_pending_commit_cursor: candidate.answered_pending_commit_cursor,
        })
    }

    pub(super) fn consume_tool_reply_identity(
        unresolved: &mut std::collections::HashSet<PendingToolReplyKey>,
        resolved: &ResolvedToolReply,
    ) -> Result<(), StateError> {
        if unresolved.remove(&resolved.key) {
            Ok(())
        } else {
            Err(Self::invalid_tool_reply())
        }
    }

    /// Classify each current Runtime ticket against the exact retained
    /// Session-root reply. The root entry's processed marker distinguishes a
    /// reply still resolving from a processed reply; absence remains pending
    /// and therefore blocks follow-up. This disposable map creates no second
    /// pending/recovery registry.
    pub(super) fn tool_reply_projection_phases(
        session: &awaken_session_state_contract::PersistedSession,
        candidates: &[PendingToolReplyCandidate],
    ) -> std::collections::HashMap<PendingToolReplyKey, project::RuntimeToolProjectionPhase> {
        candidates
            .iter()
            .map(|candidate| {
                let retained = session
                    .event_batches
                    .iter()
                    .flat_map(|batch| &batch.events)
                    .find(|entry| match &entry.event {
                        awaken_session_event_contract::SessionEventCommand::ToolReply { reply, .. } => {
                            let client_executed = matches!(
                                reply.reply,
                                awaken_session_event_contract::SessionEventToolReplyKind::CustomToolResult { .. }
                                    | awaken_session_event_contract::SessionEventToolReplyKind::ToolResult { .. }
                            );
                            reply.target == candidate.key.target
                                && reply.expected_thread_version
                                    == Some(candidate.key.expected_thread_version)
                                && reply.expected_run_id == candidate.key.expected_run_id
                                && reply.expected_correlation_id
                                    == candidate.key.expected_correlation_id
                                && reply.answered_pending_commit_cursor
                                    == Some(candidate.answered_pending_commit_cursor)
                                && reply.runtime_tool_use_id == candidate.key.runtime_call_id
                                && client_executed == candidate.key.client_executed
                        }
                        _ => false,
                    });
                let phase = retained.map_or_else(
                    || {
                        project::RuntimeToolProjectionPhase::from_pending(
                            candidate.key.client_executed,
                        )
                    },
                    |entry| {
                        project::RuntimeToolProjectionPhase::from_retained_reply(entry.processed)
                    },
                );
                (candidate.key.clone(), phase)
            })
            .collect()
    }

    pub(super) fn unresolved_tool_replies_block_followup(
        event: &InboundEvent,
        unresolved: &std::collections::HashSet<PendingToolReplyKey>,
    ) -> bool {
        !unresolved.is_empty()
            && matches!(
                event,
                InboundEvent::SystemMessage { .. } | InboundEvent::UserMessage { .. }
            )
    }

    pub(super) fn assistant_response_coordinates(
        messages: &[awaken_agent_contract::agent::message::Message],
        run_ids: &[awaken_agent_contract::agent::run::Id],
    ) -> std::collections::HashMap<String, (String, usize, usize)> {
        let candidates: std::collections::HashSet<&str> =
            run_ids.iter().map(|run_id| run_id.0.as_str()).collect();
        let mut next_response = std::collections::HashMap::<(String, usize), usize>::new();
        let mut coordinates = std::collections::HashMap::new();
        for message in messages.iter().filter(|message| {
            message.role == awaken_agent_contract::agent::message::Role::Assistant
        }) {
            let Some((run_id, step, response)) = message.id.assistant_coordinates() else {
                continue;
            };
            if !candidates.contains(run_id) {
                continue;
            }
            let next = next_response.entry((run_id.to_owned(), step)).or_default();
            let response = match response {
                Some(response) => {
                    *next = (*next).max(response + 1);
                    response
                }
                None => *next,
            };
            coordinates.insert(message.id.0.clone(), (run_id.to_owned(), step, response));
        }
        coordinates
    }
}

#[cfg(test)]
mod assistant_coordinate_tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;

    #[test]
    fn indexed_assistant_coordinates_match_ordered_response_rules() {
        // Causes: C1 selected/foreign Run; C2 Assistant/other role; C3 full,
        // partial, malformed identity; C4 duplicate or reordered responses;
        // C5 Run names overlap/contain delimiters; C6 selected Run repeated.
        // Rules A1=!C1 or !C2 or malformed => omitted without advancing;
        // A2=selected Assistant partial => exact encoded ordinal and next=max;
        // A3=selected Assistant full => current next, initially zero;
        // A4=duplicate message => last coordinate wins in source order;
        // A5=overlap/duplicate Runs => exact owner only, never double advance.
        // Exhaust all streams of length 0..=4 over these ten cause partitions.
        // The oracle states the retained prefix-scan behavior; it is test-only,
        // not a second projection path or persisted source of coordinates.
        fn oracle(
            messages: &[Message],
            runs: &[RunId],
        ) -> std::collections::HashMap<String, (String, usize, usize)> {
            let mut runs = runs.to_vec();
            runs.sort_by_key(|run| std::cmp::Reverse(run.0.len()));
            let mut next = std::collections::HashMap::<(String, usize), usize>::new();
            let mut result = std::collections::HashMap::new();
            for message in messages
                .iter()
                .filter(|message| message.role == Role::Assistant)
            {
                for run in &runs {
                    if let Some((step, response)) = message.id.assistant_truncated_response_of(run)
                    {
                        let value = next.entry((run.0.clone(), step)).or_default();
                        *value = (*value).max(response + 1);
                        result.insert(message.id.0.clone(), (run.0.clone(), step, response));
                        break;
                    }
                    if let Some(step) = message.id.assistant_step_of(run) {
                        result.insert(
                            message.id.0.clone(),
                            (
                                run.0.clone(),
                                step,
                                *next.entry((run.0.clone(), step)).or_default(),
                            ),
                        );
                        break;
                    }
                }
            }
            result
        }
        let root = RunId("root".into());
        let nested = RunId("root-assistant-0".into());
        let foreign = RunId("foreign".into());
        let alphabet = [
            (MessageId::assistant(&root, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 2), Role::Assistant),
            (MessageId::assistant(&root, 1), Role::Assistant),
            (MessageId::assistant(&nested, 0), Role::Assistant),
            (
                MessageId::assistant_truncated(&nested, 0, 1),
                Role::Assistant,
            ),
            (MessageId::assistant(&foreign, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 9), Role::User),
            (MessageId::assistant(&root, 0), Role::Tool),
            (MessageId("root-assistant-bad".into()), Role::Assistant),
        ]
        .map(|(id, role)| Message::text(id, role, "coordinate fixture"));
        let runs = [root.clone(), nested.clone(), root.clone()];
        for len in 0..=4_u32 {
            for code in 0..alphabet.len().pow(len) {
                let mut code = code;
                let messages: Vec<_> = (0..len)
                    .map(|_| {
                        let message = alphabet[code % alphabet.len()].clone();
                        code /= alphabet.len();
                        message
                    })
                    .collect();
                assert_eq!(
                    ManagedState::assistant_response_coordinates(&messages, &runs),
                    oracle(&messages, &runs)
                );
            }
        }
        let messages = alphabet.to_vec();
        assert!(
            ManagedState::assistant_response_coordinates(&messages, &[]).is_empty(),
            "A1 no selected Runs"
        );
        let expected = oracle(&messages, &runs);
        for count in [0, 10, 10_000] {
            let mut expanded: Vec<_> = (0..count)
                .map(|i| RunId(format!("unrelated-{i}")))
                .collect();
            expanded.extend(runs.clone());
            assert_eq!(
                ManagedState::assistant_response_coordinates(&messages, &expanded),
                expected,
                "A5 unrelated Run cardinality {count}"
            );
        }
    }
}

#[cfg(test)]
mod interrupt_target_tests {
    use super::*;
    use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, Record, RunState};
    use awaken_agent_contract::agent::state::Action as StateAction;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_session_error_contract::RunErrorKind;

    fn link(thread_id: &str, target: CoordinatedThreadTarget) -> CoordinatedThreadLink {
        CoordinatedThreadLink {
            session_id: "session".into(),
            thread_id: ThreadId(thread_id.into()),
            target,
            created_by_operation_id: format!("operation-{thread_id}"),
            latest_run_id: Some(RunId(format!("run-{thread_id}"))),
        }
    }

    fn snapshot(
        thread_id: &str,
        state: RunState,
        archived: bool,
    ) -> awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
        let run_id = RunId(format!("run-{thread_id}"));
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: ThreadId(thread_id.into()),
            claimed_run_id: run_id.clone(),
            runs: vec![Record {
                id: run_id.clone(),
                thread_id: ThreadId(thread_id.into()),
                state,
            }],
            latest_run_id: Some(run_id),
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state: archived
                .then(awaken_agent_contract::archive_thread_command)
                .into_iter()
                .collect(),
            state_commit_cursors: archived.then_some(1).into_iter().collect(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 0,
        }
    }

    #[test]
    fn canonical_snapshot_classifies_interrupt_terminal_children() {
        // Causes: C1 target kind is ordinary Agent or Advisor; C2 recovery
        // snapshot is absent or present; C3 a present snapshot has a valid,
        // missing, or dangling latest Run; C4 disposition is Active, Archived,
        // or malformed; C5 latest Run is Running, naturally completed, or
        // failed. Effects: E1 only an absent snapshot is the cancellable
        // pre-first-snapshot state; E2 Running is cancellable; E3 completed
        // ordinary Agent remains reusable; E4 any ended Advisor, failed Agent,
        // or Archived Thread is terminal; E5 every malformed present snapshot
        // fails Internal instead of reviving a possibly terminal Thread.
        //
        // Decision table: T0=!C2=>E1; T1=Agent+Running+Active=>E2;
        // T2=Agent+Completed+Active=>E3; T3=Advisor+Ended+Active=>E4;
        // T4=Agent+Failed+Active=>E4; T5=C4 Archived=>E4;
        // T6=C2+missing latest=>E5; T7=C2+dangling latest=>E5;
        // T8=C2+malformed disposition=>E5. Runtime recovery remains the only
        // state/disposition authority; the Managed DTO cache is absent here.
        let agent = link(
            "agent",
            CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
        );
        let advisor = link(
            "advisor",
            CoordinatedThreadTarget::Advisor {
                model: "advisor-model".into(),
            },
        );
        assert!(
            !ManagedState::coordinated_thread_is_terminal(&agent, None).unwrap(),
            "T0"
        );
        assert!(
            !ManagedState::coordinated_thread_is_terminal(
                &agent,
                Some(&snapshot("agent", RunState::Running, false)),
            )
            .unwrap(),
            "T1"
        );
        assert!(
            !ManagedState::coordinated_thread_is_terminal(
                &agent,
                Some(&snapshot(
                    "agent",
                    RunState::Ended(EndCause::NaturalEnd),
                    false,
                )),
            )
            .unwrap(),
            "T2"
        );
        assert!(
            ManagedState::coordinated_thread_is_terminal(
                &advisor,
                Some(&snapshot(
                    "advisor",
                    RunState::Ended(EndCause::NaturalEnd),
                    false,
                )),
            )
            .unwrap(),
            "T3"
        );
        assert!(
            ManagedState::coordinated_thread_is_terminal(
                &agent,
                Some(&snapshot(
                    "agent",
                    RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
                    false,
                )),
            )
            .unwrap(),
            "T4"
        );
        assert!(
            ManagedState::coordinated_thread_is_terminal(
                &agent,
                Some(&snapshot("agent", RunState::Running, true)),
            )
            .unwrap(),
            "T5"
        );

        let mut missing_latest = snapshot("agent", RunState::Running, false);
        missing_latest.latest_run_id = None;
        let missing_latest =
            ManagedState::coordinated_thread_is_terminal(&agent, Some(&missing_latest));
        assert!(
            matches!(
                &missing_latest,
                Err(StateError::Run(RunError {
                    kind: RunErrorKind::Internal,
                    ..
                }))
            ),
            "T6: {missing_latest:?}"
        );

        let mut dangling_latest = snapshot("agent", RunState::Running, false);
        dangling_latest.latest_run_id = Some(RunId("missing-run".into()));
        let dangling_latest =
            ManagedState::coordinated_thread_is_terminal(&agent, Some(&dangling_latest));
        assert!(
            matches!(
                &dangling_latest,
                Err(StateError::Run(RunError {
                    kind: RunErrorKind::Internal,
                    ..
                }))
            ),
            "T7: {dangling_latest:?}"
        );

        let mut malformed_disposition = snapshot("agent", RunState::Running, true);
        malformed_disposition.state[0].action =
            StateAction::Set(serde_json::json!({"unknown": true}));
        let malformed_disposition =
            ManagedState::coordinated_thread_is_terminal(&agent, Some(&malformed_disposition));
        assert!(
            matches!(
                &malformed_disposition,
                Err(StateError::Run(RunError {
                    kind: RunErrorKind::Internal,
                    ..
                }))
            ),
            "T8: {malformed_disposition:?}"
        );
    }
}
