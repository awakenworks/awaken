//! Committed recovery snapshots, pending-tool identity resolution, and projection qualification.

use super::*;

impl ManagedState {
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
        Ok(
            disposition == awaken_agent_contract::ThreadDisposition::Archived
                || coordinated_child_run_is_terminal(
                    matches!(&link.target, CoordinatedThreadTarget::Advisor { .. }),
                    latest_state,
                ),
        )
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
            .session_thread_recovery_snapshot(session_id, thread_id)
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
            .coordinated_threads(session_id)
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
        let run_id = snapshot
            .latest_run_id
            .as_ref()
            .unwrap_or(&snapshot.claimed_run_id);
        // The Runtime resumes one committed Awaiting ticket at a time. An
        // ActiveToolBatch may retain later Requested calls, but they are not
        // externally answerable until the current ticket is consumed and the
        // Runtime advances the batch. Never pick arbitrarily if a malformed or
        // future producer publishes two tickets for the same current Run.
        let mut tickets = snapshot
            .resume_tickets
            .iter()
            .filter(|ticket| &ticket.run_id == run_id);
        let Some(ticket) = tickets.next() else {
            return Ok(None);
        };
        if tickets.next().is_some() {
            return Err(StateError::Run(RunError::internal(
                "Runtime recovery exposed multiple answerable tickets for one Run",
            )));
        }
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
        Self::pending_ticket_from_recovery_snapshot(snapshot)
            .map(|pending| pending.map(|(_, _, pending)| pending))
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
        events: &mut [ProjectedEvent],
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
                awaken_session_contract::AgentTool::Custom { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let mut sources = Self::tool_call_sources(messages);
        let mut latest = record.projected_tool_index_for(owner_thread_id).latest;
        for event in events {
            // Runtime's neutral pending state says only that the client must
            // supply the result. Managed distinguishes authored custom tools
            // from self-hosted Agent-toolset execution at this sole projection
            // boundary, using the frozen Session/child Agent definition.
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
                    session_thread_id: session_thread_id.clone(),
                };
            }
            if matches!(
                event.kind,
                OutboundKind::AgentToolUse { .. }
                    | OutboundKind::AgentCustomToolUse { .. }
                    | OutboundKind::AgentMcpToolUse { .. }
            ) {
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
                    *tool_use_id = latest.get(tool_use_id).cloned().ok_or_else(|| {
                        StateError::Run(RunError::internal(
                            "Managed tool result has no preceding tool-use identity",
                        ))
                    })?;
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
        }
        Ok(())
    }

    pub(super) fn invalid_tool_reply() -> StateError {
        StateError::Run(RunError::bad_request(
            "tool result does not match the pending tool event",
        ))
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
        let mut candidates = run_ids.to_vec();
        candidates.sort_by_key(|run_id| std::cmp::Reverse(run_id.0.len()));
        let mut next_response = std::collections::HashMap::<(String, usize), usize>::new();
        let mut coordinates = std::collections::HashMap::new();
        for message in messages.iter().filter(|message| {
            message.role == awaken_agent_contract::agent::message::Role::Assistant
        }) {
            for run_id in &candidates {
                if let Some((step, response)) = message.id.assistant_truncated_response_of(run_id) {
                    next_response
                        .entry((run_id.0.clone(), step))
                        .and_modify(|next| *next = (*next).max(response + 1))
                        .or_insert(response + 1);
                    coordinates.insert(message.id.0.clone(), (run_id.0.clone(), step, response));
                    break;
                }
                if let Some(step) = message.id.assistant_step_of(run_id) {
                    let response = *next_response.entry((run_id.0.clone(), step)).or_default();
                    coordinates.insert(message.id.0.clone(), (run_id.0.clone(), step, response));
                    break;
                }
            }
        }
        coordinates
    }
}

#[cfg(test)]
mod interrupt_target_tests {
    use super::*;
    use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, Record, RunState};
    use awaken_agent_contract::agent::state::Action as StateAction;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_session_contract::RunErrorKind;

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
