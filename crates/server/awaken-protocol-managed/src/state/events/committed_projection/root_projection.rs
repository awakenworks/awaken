// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Root Thread reduction of one projection candidate: inbound receipts, the
//! aggregate opening, primary Thread transitions, run observations, the root
//! transcript and its answerable ticket, and Outcome facts.
//!
//! Consumer: the candidate builder, before the delegation projector and the
//! aggregate close.

use super::candidate_evidence::{BudgetReachProjection, CandidateEvidence};
use super::*;
use awaken_managed_session_events::{
    FactKey, FactKind, FactProvenance, FactRole, TimelineFact, TranscriptRole,
};

/// Primary-thread input not already owned by a retained Managed admission.
/// Other public adapters use the same committed transcript, not another journal.
fn transcript_user_input(
    message: &awaken_agent_contract::agent::message::Message,
    session_id: &str,
    batches: &[awaken_session_event_contract::SessionEventBatch],
    runs: &[awaken_agent_contract::agent::run::Record],
) -> Option<OutboundKind> {
    use awaken_agent_contract::agent::message::{Id, Role};
    if message.role != Role::User
        || message.id.is_agent_thread_report()
        || runs
            .iter()
            .any(|run| message.id.is_continuation_of(&run.id) || message.id.is_steer_of(&run.id))
        || batches.iter().flat_map(|batch| &batch.events).any(|entry| {
            Id::session_event_input(session_id, entry.event.operation_id()) == message.id
        })
    {
        return None;
    }
    Some(OutboundKind::UserMessage {
        content: message.content.clone(),
    })
}

/// What the root reduction decided that the aggregate close reuses.
pub(super) struct RootProjection {
    /// Budget-reach transitions not yet projected by an earlier refresh.
    pub(super) projected_budget_reach: Vec<BudgetReachProjection>,
}

/// The public id family of one transcript fold: an assistant response when the
/// Message carries Run coordinates, else the Message and fold ordinal.
pub(in crate::state::events) fn transcript_fact_key(
    thread_id: &str,
    message_id: &str,
    ordinal: usize,
    kind: &OutboundKind,
    coordinates: Option<&(String, usize, usize)>,
) -> Result<FactKey, StateError> {
    let role = TranscriptRole::from_wire_type(kind.type_str()).ok_or_else(|| {
        StateError::Run(RunError::internal(format!(
            "Managed transcript fold `{}` has no Message identity",
            kind.type_str()
        )))
    })?;
    let provenance = match (kind, coordinates) {
        (
            OutboundKind::AgentMessage { .. } | OutboundKind::AgentThinking {},
            Some((run_id, step, response)),
        ) => FactProvenance::AssistantResponse {
            run_id: run_id.clone(),
            step: *step,
            response: *response,
        },
        _ => FactProvenance::Message {
            message_id: message_id.to_string(),
            ordinal,
        },
    };
    Ok(FactKey::new(
        thread_id,
        FactRole::Transcript(role),
        provenance,
    ))
}

impl ManagedState {
    pub(super) fn append_root_terminal_error(
        record: &mut SessionRecord,
        terminal: &RunLifecycleEvent,
        outcome_owns_failure: bool,
        snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<(), StateError> {
        if outcome_owns_failure {
            return Ok(());
        }
        let Some(fault) =
            awaken_managed_session_events::run_fault(terminal, snapshot).map_err(projection_error)?
        else {
            return Ok(());
        };
        record.publish_fact_once(
            &TimelineFact::new(
                FactKey::new(
                    record.session.id.clone(),
                    FactRole::RootLifecycleError,
                    FactProvenance::Lifecycle {
                        cursor: terminal.cursor.0,
                    },
                ),
                FactKind::RunError {
                    fault,
                    advisor: false,
                },
            ),
            None,
        )?;
        Ok(())
    }

    pub(super) fn project_root_prefix(
        &self,
        record: &mut SessionRecord,
        session_id: &str,
        persisted: &awaken_session_state_contract::PersistedSession,
        evidence: &CandidateEvidence,
        index: &OrderIndex<'_>,
    ) -> Result<RootProjection, StateError> {
        let all_accepted_lifecycle = &evidence.acceptance.accepted;
        let messages = evidence.root_messages();
        let historical_pending = &evidence.historical_pending;
        // Harvest what the gate decided before the transcript is projected. The
        // decision is committed audit truth about a call; the transcript alone
        // cannot distinguish a refused call from an executed one whose tool
        // returned an error.
        record.permission_decisions = permission_harvest::harvest_permission_decisions(
            evidence.root.iter().chain(evidence.children.values()),
        );
        merge_durable_inbound_projections(record, &evidence.durable_inbound);
        record.primary_thread_usage = evidence.primary_thread_usage.clone();
        // Cause C1: an Awaiting interval and its answerable target committed;
        // C2: a later resume/cancel terminal committed before Managed observed
        // that interval; C3: no ToolUse message carries the target. Effect E1:
        // project the same qualified tool event before lifecycle idle reasons;
        // a committed message or prior event remains the canonical projection.
        // Rules: H1=C1+C2+C3=>E1; H2=message/prior event=>reuse unchanged.
        self.append_synthetic_historical_pending_projections(
            record,
            session_id,
            all_accepted_lifecycle,
            historical_pending,
            evidence.root.as_ref(),
            &evidence.children,
        )?;
        let projected_budget_reach = evidence
            .budget_reach
            .iter()
            .filter(|(generation, _, _)| {
                *generation > record.checkpoint.source.budget_reach_generation
            })
            .cloned()
            .collect::<Vec<_>>();
        if evidence.acceptance.cursor > record.checkpoint.source.lifecycle_cursor {
            record.checkpoint.source.lifecycle_cursor = evidence.acceptance.cursor;
        }
        let transcript_evidence = index.transcript_evidence(session_id);
        let new_messages = if evidence.root_blocked(session_id) {
            Vec::new()
        } else {
            messages
                .iter()
                .filter(|message| {
                    message.role == awaken_agent_contract::agent::message::Role::User
                        || !record.message_was_projected(session_id, &message.id.0)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let root_terminal = all_accepted_lifecycle.iter().rev().find(|event| {
            event.thread_id.0 == session_id
                && awaken_session_event_contract::SessionAgentRunBoundary::is_boundary(event.kind)
        });
        // Each opening is decided from the entire committed lifecycle. A
        // boundary following a boundary without an intervening opening owns
        // its own Running fact on both an incremental and a cold reader.
        for fact in awaken_managed_session_events::root_lifecycle_opening_facts(
            session_id,
            all_accepted_lifecycle,
        ) {
            record.publish_fact_once(&fact, None)?;
        }
        if !evidence.root_blocked(session_id)
            && let Some(snapshot) = evidence.root.as_ref()
        {
            Self::append_run_observation_projections(record, session_id, snapshot)?;
        }
        let root_pending_source_run_id = root_terminal
            .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
            .map(|event| &event.run_id);
        let folds = awaken_managed_session_events::plan_folds(
            session_id,
            &new_messages,
            &evidence.root_response_coordinates,
            historical_pending,
            all_accepted_lifecycle,
            root_pending_source_run_id,
        );
        for fold in folds {
            let transcript_evidence = transcript_evidence
                .expect("a committed root Message belongs to the same ordering index");
            let message = fold.message;
            let coordinates = evidence.root_response_coordinates.get(&message.id.0);
            let message_source_run_id = fold.source_run_id;
            let tool_index = record.projected_tool_index_for(None);
            let pending_history = fold.pending_history();
            let selected = transcript_evidence.project_message(
                message,
                &tool_index.sources,
                message_source_run_id.map(|run_id| run_id.0.as_str()),
                &pending_history,
            );
            // This Message's exact committed Awaiting history classifies its
            // calls. Supplying the current wait to an unrelated older Message
            // would synthesize a Run-qualified tool before the owning Message.
            let mut projected = project_messages_with_mcp_ids(
                std::slice::from_ref(&selected.message),
                None,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
                &pending_history,
            );
            if let Some(input) = transcript_user_input(
                message,
                session_id,
                &persisted.event_batches,
                evidence
                    .root
                    .as_ref()
                    .map_or(&[], |snapshot| snapshot.runs.as_slice()),
            ) {
                record.append_fact_once(
                    transcript_fact_key(session_id, &message.id.0, 0, &input, None)?,
                    input,
                );
            }
            self.qualify_tool_projection(
                record,
                None,
                std::slice::from_ref(&selected.message),
                message_source_run_id,
                &mut projected,
            )?;
            for (ordinal, projected) in projected.into_iter().enumerate() {
                let kind = projected.kind;
                match projected.id {
                    Some(id) => {
                        record.append_event_once(Event {
                            id,
                            kind,
                            processed_at: None,
                        });
                    }
                    None => {
                        let key = transcript_fact_key(
                            session_id,
                            &message.id.0,
                            ordinal,
                            &kind,
                            coordinates,
                        )?;
                        record.append_fact_once(key, kind);
                    }
                }
            }
            if selected.fully_classified {
                record.consume_message(session_id, &message.id.0);
            }
        }
        // Outcome observation is an immediate, side-effect-free committed fact.
        // The retained DefineOutcome selects the aggregate: Completed adds only
        // evaluation facts, while Errored adds one infrastructure error and no
        // partial evaluation. General transcript/lifecycle projection remains
        // the sole owner unless an exact failure source Run is correlated below.
        append_durable_outcome_projections(record, &evidence.outcomes);
        Ok(RootProjection {
            projected_budget_reach,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Message, Role};
    use awaken_agent_contract::agent::run::{Id as RunId, Record, RunState};
    use awaken_session_event_contract::{SessionEventBatch, SessionEventInput};
    use awaken_session_types::SessionId;

    #[test]
    fn public_input_has_one_source_and_internal_feedback_is_never_customer_evidence() {
        // C1 customer-role transcript input; C2 exact Managed command already
        // owns its identity; C3 typed internal child/steer/continuation input;
        // C4 assistant/system role. R1 C1 without C2/C3 -> byte-faithful input;
        // R2 C2 -> existing receipt only; R3 C3/C4 -> no customer event.
        // A normal message containing internal-looking words remains public:
        // identity/role, never text or a language filter, selects these rules.
        let session = SessionId::from("session");
        let run_id = RunId("run".into());
        let runs = [Record {
            id: run_id.clone(),
            thread_id: awaken_agent_contract::agent::thread::Id(session.as_str().into()),
            state: RunState::Running,
        }];
        let text = "API 成本尚未提供，不能把它当成没有成本。";
        let ordinary = Message::text(Id("sdk-input".into()), Role::User, text);
        assert!(
            matches!(transcript_user_input(&ordinary, session.as_str(), &[], &runs),
            Some(OutboundKind::UserMessage { content }) if content == ordinary.content),
            "R1 exact public bytes"
        );
        let batch = SessionEventBatch::compile(
            &session,
            "batch",
            vec![SessionEventInput::UserMessage {
                content: ordinary.content.clone(),
            }],
        )
        .unwrap();
        let managed = Message::text(
            Id::session_event_input(session.as_str(), batch.events[0].event.operation_id()),
            Role::User,
            text,
        );
        assert!(
            transcript_user_input(&managed, session.as_str(), &[batch], &runs).is_none(),
            "R2 no duplicate receipt"
        );
        for input in [
            Message::text(Id::agent_thread_report(&run_id), Role::User, text),
            Message::text(Id::steer(&run_id, 1), Role::User, text),
            Message::text(Id::continuation(&run_id, 0, 1), Role::User, text),
            Message::text(Id("assistant".into()), Role::Assistant, text),
            Message::text(Id("system".into()), Role::System, text),
        ] {
            assert!(
                transcript_user_input(&input, session.as_str(), &[], &runs).is_none(),
                "R3 {:?}",
                input.id
            );
        }
    }
}
