// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: managed parent and child transcript projection. Dynamic path: committed
// @formal reason: thread-scoped content is mapped to the correct transcript while delegation boundaries
// @formal reason: remain intact. Evidence: session.multiagent.thread_authority and managed transcript
// @formal reason: projection tests; this is a product-requirement classification, not a direct proof
// @formal reason: claim.
//! Committed child transcript classification and coordination cross-post projection.

use super::*;
use awaken_managed_session_events::{FactKey, FactProvenance, FactRole};

impl ManagedState {
    pub(super) fn append_child_transcript_projection(
        &self,
        record: &mut SessionRecord,
        projection: ChildTranscriptProjection<'_>,
    ) -> Result<(), StateError> {
        let ChildTranscriptProjection {
            thread_id,
            agent_name,
            advisor_model,
            messages,
            lifecycle_events,
            latest_run_id,
            latest_run_state,
            pending,
            pending_source_run_id,
            historical_pending,
        } = projection;
        let run_ids = lifecycle_events
            .iter()
            .map(|event| event.run_id.clone())
            .chain(latest_run_id.cloned())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let response_coordinates = Self::assistant_response_coordinates(messages, &run_ids);
        let ordinary_response_steps = messages
            .iter()
            .filter(|message| {
                message.role == awaken_agent_contract::agent::message::Role::Assistant
                    && message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
            })
            .filter_map(|message| {
                response_coordinates
                    .get(&message.id.0)
                    .map(|(run_id, step, _)| (run_id.clone(), *step))
            })
            .collect::<std::collections::HashSet<_>>();
        let new_messages = messages
            .iter()
            .filter(|message| !record.message_was_projected(thread_id, &message.id.0))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(advisor_model) = advisor_model {
            // The recovery snapshot is the committed terminal authority even
            // after its lifecycle cursor was consumed by an earlier refresh.
            // Running/Awaiting therefore retain every unprojected seed/output
            // Message. A failed/cancelled consultation consumes them without a
            // public receive, while a successful terminal selects only the
            // latest Run's canonical report response. This excludes the copied
            // parent transcript in the advisor seed and folds provider response
            // chunks into the protocol's one consultation result.
            match latest_run_state {
                Some(awaken_agent_contract::agent::run::RunState::Ended(
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd
                    | awaken_agent_contract::agent::run::EndCause::MaxSteps,
                )) => {}
                Some(awaken_agent_contract::agent::run::RunState::Ended(_)) => {
                    for message in &new_messages {
                        record.consume_message(thread_id, &message.id.0);
                    }
                    return Ok(());
                }
                Some(
                    awaken_agent_contract::agent::run::RunState::Running
                    | awaken_agent_contract::agent::run::RunState::Awaiting,
                )
                | None => return Ok(()),
            }
            let latest_run_id = latest_run_id.ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "terminal advisor recovery snapshot has no latest Run id",
                ))
            })?;
            let report_messages = session_agent_report_messages(messages, latest_run_id);
            let has_unprojected_report = report_messages
                .iter()
                .any(|report| new_messages.iter().any(|message| message.id == report.id));
            if has_unprojected_report {
                let report_content = report_messages
                    .iter()
                    .flat_map(|message| message.content.iter().cloned())
                    .collect::<Vec<_>>();
                let content = crate::types::project_advisor_thread_message_content(
                    advisor_model,
                    &report_content,
                );
                if !content.is_empty() {
                    let key = FactKey::new(
                        thread_id,
                        FactRole::AdvisorMessageReceived,
                        FactProvenance::AgentReport {
                            run_id: latest_run_id.0.clone(),
                        },
                    );
                    let id = record.fact_id(&key);
                    record
                        .event_thread_owners
                        .insert(id, record.session.id.clone());
                    record.push_fact(
                        key,
                        OutboundKind::AgentThreadMessageReceived {
                            from_session_thread_id: thread_id.to_string(),
                            from_agent_name: Some(agent_name.to_string()),
                            content,
                        },
                    );
                }
            }
            for message in &new_messages {
                record.consume_message(thread_id, &message.id.0);
            }
            return Ok(());
        }

        // The Session coordination contract is the sole report classifier used
        // by settlement and this wire projector. A normally completed child Run
        // contributes one cross-Thread report only when its classifier-selected
        // text is non-empty; a tool-only terminal Message must instead fall
        // through to occurrence projection so every classified call precedes
        // its result. Failed and Cancelled Runs settle directly and therefore
        // never own a report that could disguise a terminal failure as a normal
        // response.
        let mut report_run_by_message = std::collections::HashMap::<String, String>::new();
        let mut report_text_by_run = std::collections::HashMap::<String, String>::new();
        for terminal in lifecycle_events
            .iter()
            .filter(|event| event.kind == RunLifecycleEventKind::Completed)
        {
            let report_text = session_agent_report_text(messages, &terminal.run_id);
            if report_text.is_empty() {
                continue;
            }
            let report_messages = session_agent_report_messages(messages, &terminal.run_id);
            if report_messages.is_empty() {
                continue;
            }
            for message in report_messages {
                report_run_by_message.insert(message.id.0.clone(), terminal.run_id.0.clone());
            }
            report_text_by_run.insert(terminal.run_id.0.clone(), report_text);
        }

        // Project one durable Message at a time. An ordinal is meaningful only
        // inside that Message; deriving it from a refresh batch would make the
        // public id and report ordering depend on warm/cold grouping. Exact
        // occurrence classification is rebuilt once from the full committed
        // prefix, while stable Event ids make a partially classified source
        // Message safely revisitable without another cursor or registry.
        let transcript_evidence = project::committed_transcript_evidence(messages, pending);
        let mut pending_is_in_new_message = false;
        let mut projected_reports = std::collections::HashSet::new();
        for message in &new_messages {
            if let Some(report_run_id) = report_run_by_message.get(&message.id.0) {
                if projected_reports.insert(report_run_id.clone())
                    && let Some(text) = report_text_by_run
                        .get(report_run_id)
                        .filter(|text| !text.is_empty())
                {
                    record.push_fact(
                        FactKey::new(
                            thread_id,
                            FactRole::ChildMessageReceived,
                            FactProvenance::AgentReport {
                                run_id: report_run_id.clone(),
                            },
                        ),
                        OutboundKind::AgentThreadMessageReceived {
                            from_session_thread_id: thread_id.to_string(),
                            from_agent_name: Some(agent_name.to_string()),
                            content: vec![ContentBlock::text(text.clone())],
                        },
                    );
                }
                record.consume_message(thread_id, &message.id.0);
                continue;
            }
            if message.role == awaken_agent_contract::agent::message::Role::Assistant {
                let ordinary_is_proven = message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
                    || response_coordinates
                        .get(&message.id.0)
                        .is_some_and(|(run_id, step, _)| {
                            ordinary_response_steps.contains(&(run_id.clone(), *step))
                        });
                if !ordinary_is_proven {
                    // A complete text-only response whose lifecycle boundary is
                    // not yet in this accepted snapshot may still be the report.
                    // Leave it unconsumed until Completed classifies it instead
                    // of publishing an irreversible `agent.message` too early.
                    continue;
                }
            }
            let historical_boundaries = awaken_managed_session_events::historical_message_boundaries(
                message,
                historical_pending,
                lifecycle_events,
                thread_id,
                response_coordinates
                    .get(&message.id.0)
                    .map(|(run, _, _)| run.as_str()),
            );
            let message_source_run_id = pending_source_run_id
                .or_else(|| historical_boundaries.first().map(|(_, run)| *run));
            let tool_index = record.projected_tool_index_for(Some(thread_id));
            let pending_history = historical_boundaries
                .iter()
                .map(|(pending, _)| pending)
                .collect::<Vec<_>>();
            let selected = transcript_evidence.project_message(
                message,
                &tool_index.sources,
                message_source_run_id.map(|run_id| run_id.0.as_str()),
                &pending_history,
            );
            pending_is_in_new_message |= selected.pending_occurrence;
            // The exact current ticket wins over any earlier call in this
            // source message. Historical permissions are applied per call by
            // the shared encoder, never chosen by HashMap iteration order.
            let message_pending = selected
                .retained_pending_occurrence
                .then_some(pending)
                .flatten();
            let mut projected = project_messages_with_mcp_ids(
                std::slice::from_ref(&selected.message),
                message_pending,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
                &pending_history,
            );
            self.qualify_tool_projection(
                record,
                Some(thread_id),
                std::slice::from_ref(&selected.message),
                message_source_run_id,
                &mut projected,
            )?;
            for (ordinal, projected) in projected.into_iter().enumerate() {
                let kind = projected.kind;
                let key = match projected.id {
                    Some(_) => None,
                    None => Some(committed_projection::transcript_fact_key(
                        thread_id,
                        &message.id.0,
                        ordinal,
                        &kind,
                        response_coordinates.get(&message.id.0),
                    )?),
                };
                let id = match (&projected.id, &key) {
                    (Some(id), _) => id.clone(),
                    (None, Some(key)) => record.fact_id(key),
                    (None, None) => unreachable!("an unqualified fold has a fact key"),
                };
                if !record.has_event_id(&id) {
                    record
                        .event_thread_owners
                        .insert(id.clone(), thread_id.to_string());
                    match key {
                        Some(key) => {
                            record.append_fact_once(key, kind);
                        }
                        None => {
                            record.append_event_once(Event {
                                id,
                                kind,
                                processed_at: None,
                            });
                        }
                    }
                }
            }

            if selected.fully_classified {
                record.consume_message(thread_id, &message.id.0);
            }
        }

        // When Awaiting has no committed ToolUse message in this delta, retain
        // the existing synthetic-pending path. It has no minted transcript
        // event: the reversible tool id is sourced by the durable Run id.
        if pending.is_some() && !pending_is_in_new_message {
            let tool_index = record.projected_tool_index_for(Some(thread_id));
            let mut projected = project_messages_with_mcp_ids(
                &[],
                pending,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
                &[],
            );
            self.qualify_tool_projection(
                record,
                Some(thread_id),
                &[],
                pending_source_run_id,
                &mut projected,
            )?;
            for projected in projected {
                let id = projected.id.ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "synthetic child pending projection has no qualified tool id",
                    ))
                })?;
                record
                    .event_thread_owners
                    .insert(id.clone(), thread_id.to_string());
                record.events.push(Event {
                    id,
                    kind: projected.kind,
                    processed_at: None,
                });
            }
        }
        Ok(())
    }

    pub(super) fn coordinated_thread_agent(
        record: &SessionRecord,
        link: &CoordinatedThreadLink,
    ) -> Result<crate::types::SessionThreadAgentValue, StateError> {
        if link.session_id != record.session.id
            || link.thread_id.0 == record.session.id
            || link.thread_id.0 == public_thread_id(&record.session.id, &record.session.id)
        {
            return Err(StateError::Run(RunError::internal(
                "Runtime returned a coordinated Thread outside its Session",
            )));
        }
        let roster = record
            .session
            .agent
            .multiagent
            .as_ref()
            .map(|coordinator| coordinator.agents.as_slice())
            .unwrap_or_default();
        match &link.target {
            CoordinatedThreadTarget::Agent { agent_id } => roster
                .iter()
                .filter_map(crate::types::SessionMultiagentRosterEntry::as_agent)
                .find(|agent| agent.id == *agent_id)
                .cloned()
                .map(Into::into)
                .ok_or_else(|| {
                    StateError::Run(RunError::internal(format!(
                        "coordinated Thread `{}` names Agent `{agent_id}` outside the frozen roster",
                        link.thread_id.0
                    )))
                }),
            CoordinatedThreadTarget::Advisor { model } => {
                let advisors = roster
                    .iter()
                    .filter_map(crate::types::SessionMultiagentRosterEntry::as_advisor)
                    .collect::<Vec<_>>();
                let advisor = if model.trim().is_empty() {
                    // A cold Host may rebuild the stable advisor identity while
                    // its mutable model catalog is unavailable. The Session's
                    // frozen profile remains the presentation authority, but an
                    // ambiguous roster must still fail closed.
                    match advisors.as_slice() {
                        [advisor] => Some(*advisor),
                        _ => None,
                    }
                } else {
                    advisors
                        .into_iter()
                        .find(|advisor| advisor.model == *model)
                };
                advisor.cloned().map(Into::into).ok_or_else(|| {
                    StateError::Run(RunError::internal(format!(
                        "coordinated Thread `{}` cannot resolve advisor model `{model}` from the frozen roster",
                        link.thread_id.0
                    )))
                })
            }
        }
    }

    /// Lower accepted parent `send_message` calls into one cross-Thread message.
    /// Spawn calls are bound by the operation id that created the durable link;
    /// follow-ups are bound by the existing Thread id. A failed tool result never
    /// creates a message merely because its target link already exists.
    pub(super) fn append_coordination_cross_posts(
        &self,
        record: &mut SessionRecord,
        links: &[CoordinatedThreadLink],
    ) {
        // No link can accept a cross-post yet; its unchanged committed call
        // remains eligible when a later refresh observes that relationship.
        if links.is_empty() {
            return;
        }
        let calls = {
            let wanted = record.events.iter().filter(|event| !record.event_thread_owners.contains_key(&event.id))
                .filter(|event| matches!(&event.kind, OutboundKind::AgentToolUse { name, .. } if name == SEND_MESSAGE))
                .map(|event|event.id.as_str()).collect::<std::collections::HashSet<_>>();
            let receipts = super::coordination_receipts::CoordinationReceipts::for_calls(
                &record.events,
                &record.event_thread_owners,
                &wanted,
            );
            let mut links_by_thread =
                std::collections::HashMap::<&str, Vec<&CoordinatedThreadLink>>::new();
            for link in links {
                links_by_thread
                    .entry(&link.thread_id.0)
                    .or_default()
                    .push(link);
            }
            record
                .events
                .iter()
                .filter(|event| !record.event_thread_owners.contains_key(&event.id))
                .filter_map(|event| {
                    let OutboundKind::AgentToolUse { name, input, .. } = &event.kind else {
                        return None;
                    };
                    if name != SEND_MESSAGE {
                        return None;
                    }
                    let receipt_thread = receipts.get(&event.id)?;
                    let runtime_call_id = decode_managed_tool_event_id(&event.id)
                        .map_or(event.id.as_str(), |identity| identity.call_id);
                    let link =
                        links_by_thread
                            .get(receipt_thread)?
                            .iter()
                            .copied()
                            .find(|link| {
                                if link.thread_id.0 != receipt_thread {
                                    return false;
                                }
                                match (
                                    input
                                        .get("agent_id")
                                        .and_then(serde_json::Value::as_str)
                                        .filter(|value| !value.trim().is_empty()),
                                    input
                                        .get("session_thread_id")
                                        .and_then(serde_json::Value::as_str)
                                        .filter(|value| !value.trim().is_empty()),
                                ) {
                                    (Some(agent_id), None) => {
                                        link.target.agent_id() == Some(agent_id)
                                            && link
                                                .created_by_operation_id
                                                .strip_suffix(runtime_call_id)
                                                .is_some_and(|prefix| prefix.ends_with(':'))
                                    }
                                    (None, Some(thread_id)) => link.thread_id.0 == thread_id,
                                    _ => false,
                                }
                            })?;
                    let message = input
                        .get("message")
                        .and_then(serde_json::Value::as_str)?
                        .to_owned();
                    Some((event.id.clone(), link.thread_id.0.clone(), message))
                })
                .collect::<Vec<_>>()
        };

        if calls.is_empty() {
            return;
        }
        // These indices exist only for this reduction and are rebuilt from the
        // same record; they do not become independent cross-refresh receipts.
        let mut child_positions = std::collections::HashMap::new();
        for (position, child) in record.child_threads.iter().enumerate() {
            child_positions.entry(child.id.clone()).or_insert(position);
        }
        for (call_id, thread_id, message) in calls {
            let at = |role| {
                FactKey::new(
                    thread_id.as_str(),
                    role,
                    FactProvenance::CoordinationCall {
                        event_id: call_id.clone(),
                    },
                )
            };
            let sent = at(FactRole::CoordinationMessageSent);
            let sent_event_id = record.fact_id(&sent);
            // The deterministic public event is the sole projection receipt.
            // Re-reading the same committed call must not require a parallel
            // process-local call-id cursor, while a call whose link was not yet
            // visible remains eligible on a later refresh.
            if record.has_event_id(&sent_event_id) {
                continue;
            }
            let Some(&position) = child_positions.get(&thread_id) else {
                continue;
            };
            let child = &mut record.child_threads[position];
            if child.status == SessionThreadStatus::Terminated {
                continue;
            }
            let agent_name = child.agent.display_name().to_string();
            let opened = child.status != SessionThreadStatus::Running;
            if opened {
                child.status = SessionThreadStatus::Running;
                child.updated_at = LEGACY_UNRECORDED_AT.to_string();
                record.append_fact_once(
                    at(FactRole::CoordinationStatusRunning),
                    OutboundKind::SessionThreadStatusRunning {
                        session_thread_id: thread_id.clone(),
                        agent_name: agent_name.clone(),
                    },
                );
            }
            record.append_fact_once(
                sent,
                OutboundKind::AgentThreadMessageSent {
                    to_session_thread_id: thread_id.clone(),
                    to_agent_name: Some(agent_name),
                    content: vec![ContentBlock::text(message)],
                },
            );
        }
    }
}
