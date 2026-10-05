//! Committed child transcript classification and coordination cross-post projection.

use super::*;
use awaken_managed_session_events::{
    ChildTranscriptPlan, ChildTranscriptStep, DerivedSessionEvent, EventIdentity, FactProvenance,
    ManagedEventType, SessionEventKind,
};

/// The display name of one coordinated child record.
fn child_agent_name(record: &SessionRecord, thread_id: &str) -> Result<String, StateError> {
    record
        .child_threads
        .iter()
        .find(|child| child.id == thread_id)
        .map(|child| child.agent.display_name().to_string())
        .ok_or_else(|| {
            StateError::Run(RunError::internal(format!(
                "coordinated Thread `{thread_id}` has no projected record"
            )))
        })
}

impl ManagedState {
    pub(super) fn append_child_transcript_projection(
        &self,
        record: &mut SessionRecord,
        projection: ChildTranscriptProjection<'_>,
    ) -> Result<(), StateError> {
        let ChildTranscriptProjection {
            thread_id,
            advisor_model,
            messages,
            lifecycle_events,
            latest_run_id,
            latest_run_state,
            pending,
            pending_source_run_id,
            historical_pending,
        } = projection;
        let plan = awaken_managed_session_events::plan_child_transcript(
            &awaken_managed_session_events::ChildTranscript {
                thread_id,
                advisor: advisor_model.is_some(),
                messages,
                lifecycle: lifecycle_events,
                latest_run_id,
                latest_run_state,
                pending_source_run_id,
                historical_pending,
            },
            |message_id| record.message_was_projected(thread_id, message_id),
        )
        .map_err(projection_error)?;
        let (steps, response_coordinates) = match plan {
            ChildTranscriptPlan::Advisor { consume, report } => {
                // The recovery snapshot is the committed terminal authority
                // even after its lifecycle cursor was consumed by an earlier
                // refresh; one consultation folds into one result.
                if let (Some((run_id, report_content)), Some(advisor_model)) =
                    (report, advisor_model)
                {
                    let content = crate::types::project_advisor_thread_message_content(
                        advisor_model,
                        &report_content,
                    );
                    if !content.is_empty() {
                        let key = EventIdentity::new(
                            thread_id,
                            ManagedEventType::AdvisorMessageReceived,
                            FactProvenance::AgentReport { run_id: run_id.0 },
                        );
                        let id = record.fact_id(&key);
                        let agent_name = child_agent_name(record, thread_id)?;
                        record
                            .event_thread_owners
                            .insert(id, record.session.id.clone());
                        record.push_fact(
                            key,
                            OutboundKind::AgentThreadMessageReceived {
                                from_session_thread_id: thread_id.to_string(),
                                from_agent_name: Some(agent_name),
                                content,
                            },
                        );
                    }
                }
                for message in consume {
                    record.consume_message(thread_id, &message.id.0);
                }
                return Ok(());
            }
            ChildTranscriptPlan::Ordinary { steps, coordinates } => (steps, coordinates),
        };

        // Project one durable Message at a time. An ordinal is meaningful only
        // inside that Message; deriving it from a refresh batch would make the
        // public id and report ordering depend on warm/cold grouping. Exact
        // occurrence classification is rebuilt once from the full committed
        // prefix, while stable Event ids make a partially classified source
        // Message safely revisitable without another cursor or registry.
        let transcript_evidence = project::committed_transcript_evidence(messages, pending);
        for step in steps {
            let fold = match step {
                ChildTranscriptStep::Report {
                    message,
                    run_id,
                    text,
                } => {
                    if let Some(text) = text {
                        record.publish_fact(
                            &DerivedSessionEvent::new(
                                EventIdentity::new(
                                    thread_id,
                                    ManagedEventType::ChildMessageReceived,
                                    FactProvenance::AgentReport { run_id },
                                ),
                                SessionEventKind::ReportReceived { text },
                            ),
                            None,
                        )?;
                    }
                    record.consume_message(thread_id, &message.id.0);
                    continue;
                }
                ChildTranscriptStep::Fold(fold) => fold,
            };
            let message = fold.message;
            let pending_history = fold.pending_history();
            let tool_index = record.projected_tool_index_for(Some(thread_id));
            let selected = transcript_evidence.project_message(
                message,
                &tool_index.sources,
                fold.source_run_id.map(|run_id| run_id.0.as_str()),
                &pending_history,
            );
            // Only this Message's committed Awaiting boundaries classify its
            // calls. The latest ticket must not synthesize a call in another
            // historical Message merely because that Message had an old wait.
            let mut projected = project_messages_with_mcp_ids(
                std::slice::from_ref(&selected.message),
                None,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
                &pending_history,
            );
            self.qualify_tool_projection(
                record,
                Some(thread_id),
                std::slice::from_ref(&selected.message),
                fold.source_run_id,
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
}
