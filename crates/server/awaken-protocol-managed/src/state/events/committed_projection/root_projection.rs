// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Root Thread reduction of one projection candidate: inbound receipts, the
//! aggregate opening, primary Thread transitions, run observations, the root
//! transcript and its answerable ticket, and Outcome facts.
//!
//! Consumer: the candidate builder, before the delegation projector and the
//! aggregate close.

use super::candidate_evidence::CandidateEvidence;
use super::*;
use awaken_managed_session_events::{EventIdentity, TranscriptRole};

/// What the root reduction decided that the aggregate close reuses.
pub(super) struct RootProjection {
    /// Budget reaches not yet projected by an earlier refresh.
    pub(super) projected_budget_reach: Vec<awaken_managed_session_events::BudgetReach>,
}

/// The public identity of one Message-identified transcript fold (see
/// [`awaken_managed_session_events::transcript_fact_identity`]); a wire kind
/// that is not such a fold has none.
pub(in crate::state::events) fn transcript_fact_key(
    thread_id: &str,
    message_id: &str,
    ordinal: usize,
    kind: &OutboundKind,
    coordinates: Option<&(String, usize, usize)>,
) -> Result<EventIdentity, StateError> {
    let role = TranscriptRole::from_wire_type(kind.type_str()).ok_or_else(|| {
        StateError::Run(RunError::internal(format!(
            "Managed transcript fold `{}` has no Message identity",
            kind.type_str()
        )))
    })?;
    Ok(awaken_managed_session_events::transcript_fact_identity(
        thread_id,
        message_id,
        ordinal,
        role,
        coordinates,
    ))
}

impl ManagedState {
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
        record.project_recorded_permissions(index)?;
        merge_durable_inbound_projections(record, &evidence.durable_inbound);
        record.primary_thread_usage = evidence.primary_thread_usage.clone();
        // Cause C1: an Awaiting interval and its answerable target committed;
        // C2: a later resume/cancel terminal committed before Managed observed
        // that interval; C3: no ToolUse message carries the target. Effect E1:
        // project the same qualified tool event before lifecycle idle reasons;
        // a committed message or prior event remains the canonical projection.
        // Rules: H1=C1+C2+C3=>E1; H2=message/prior event=>reuse unchanged.
        self.append_synthetic_historical_pending_projections(record, session_id, index)?;
        let projected_budget_reach = evidence
            .budget_reach
            .iter()
            .filter(|reach| reach.generation > record.checkpoint.source.budget_reach_generation)
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
        let root_terminal =
            awaken_managed_session_events::root_run_boundaries(session_id, all_accepted_lifecycle)
                .next_back();
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
            let (selected, projected) =
                super::super::transcript_projection::project_committed_message(
                    record,
                    None,
                    &fold,
                    index,
                    None,
                    transcript_evidence,
                )?;
            if awaken_managed_session_events::is_public_user_input(
                message,
                session_id,
                &persisted.event_batches,
                evidence
                    .root
                    .as_ref()
                    .map_or(&[], |snapshot| snapshot.runs.as_slice()),
            ) {
                let input = OutboundKind::UserMessage {
                    content: message.content.clone(),
                };
                record.append_fact_once(
                    transcript_fact_key(session_id, &message.id.0, 0, &input, None)?,
                    input,
                );
            }
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
        append_committed_outcomes(record, &evidence.outcomes)?;
        Ok(RootProjection {
            projected_budget_reach,
        })
    }
}
