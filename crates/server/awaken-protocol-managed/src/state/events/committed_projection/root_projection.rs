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
    FactKey, FactKind, FactProvenance, FactRole, TimelineFact, TranscriptRole, is_opening,
    period_opening,
};

/// What the root reduction decided that the aggregate close reuses.
pub(super) struct RootProjection<'e> {
    /// The latest accepted root Run boundary of this candidate's new suffix.
    pub(super) root_terminal: Option<&'e RunLifecycleEvent>,
    /// `root_terminal`, unless an earlier refresh already projected it.
    pub(super) projected_root_terminal: Option<&'e RunLifecycleEvent>,
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

    pub(super) fn project_root_prefix<'e>(
        &self,
        record: &mut SessionRecord,
        session_id: &str,
        persisted: &awaken_session_state_contract::PersistedSession,
        evidence: &'e CandidateEvidence,
    ) -> Result<RootProjection<'e>, StateError> {
        let persisted_status = Self::wire_session_status(persisted);
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
            messages,
            &evidence.transcripts,
        )?;
        let projected_budget_reach = evidence
            .budget_reach
            .iter()
            .filter(|(generation, _, _)| {
                *generation > record.checkpoint.source.budget_reach_generation
            })
            .cloned()
            .collect::<Vec<_>>();
        let accepted_lifecycle = &evidence.new_lifecycle;
        if evidence.acceptance.cursor > record.checkpoint.source.lifecycle_cursor {
            record.checkpoint.source.lifecycle_cursor = evidence.acceptance.cursor;
        }
        let root_pending = evidence.root_pending.as_ref();
        let transcript_evidence = project::committed_transcript_evidence(messages, root_pending);
        let new_messages = if evidence.root_blocked(session_id) {
            Vec::new()
        } else {
            messages
                .iter()
                .filter(|message| !record.message_was_projected(session_id, &message.id.0))
                .cloned()
                .collect::<Vec<_>>()
        };
        let root_terminal = accepted_lifecycle.iter().rev().find(|event| {
            event.thread_id.0 == session_id
                && awaken_session_event_contract::SessionAgentRunBoundary::is_boundary(event.kind)
        });
        let projected_root_terminal = root_terminal.filter(|event| {
            !record
                .checkpoint
                .progress
                .terminal_cursors
                .contains(&event.cursor)
        });
        // The public aggregate opening belongs to the exact committed root
        // opening transition, not to this Coordinator's process-local
        // observation order. A warm replica can see Running before a cold peer
        // first sees the same prefix at terminal; a resumed Run can also open a
        // later interval under the same Run id. The lifecycle cursor is the one
        // durable coordinate that is both known at the opening and unique for
        // every reopen.
        let is_root_opening =
            |event: &&RunLifecycleEvent| event.thread_id.0 == session_id && is_opening(event.kind);
        let latest_interval_opening =
            awaken_managed_session_events::latest_interval_opening(persisted, all_accepted_lifecycle);
        let aggregate_running_key = latest_interval_opening
            .or_else(|| {
                root_terminal
                    .and_then(|terminal| {
                        // The opening may have been consumed by an earlier refresh
                        // while Session activity was not yet Running. A later terminal
                        // must still recover that exact bracket from the complete
                        // accepted prefix. Start from the latest opening of this same
                        // Run: one Run can Await and Resume more than once.
                        all_accepted_lifecycle
                            .iter()
                            .rev()
                            .filter(is_root_opening)
                            .find(|event| event.run_id == terminal.run_id)
                    })
                    .or_else(|| accepted_lifecycle.iter().rev().find(is_root_opening))
                    // The aggregate opens at the first commit of that period, not
                    // at whichever later Rescheduled commit this read saw last.
                    .map(|latest| period_opening(latest, 0, all_accepted_lifecycle))
            })
            .map(|event| {
                FactKey::new(
                    record.session.id.clone(),
                    FactRole::AggregateStatusRunning,
                    FactProvenance::Lifecycle {
                        cursor: event.cursor.0,
                    },
                )
            })
            .or_else(|| {
                // A fast Run can be first observed only at its terminal
                // boundary. In that case there is no retained opening cursor,
                // but the terminal prefix already reconstructs the primary
                // Thread Running edge. Use the same immutable terminal cursor
                // for a distinct aggregate fallback identity; an exact opening
                // always wins when one is available.
                root_terminal.map(|terminal| {
                    FactKey::new(
                        record.session.id.clone(),
                        FactRole::AggregateTerminalStatusRunning,
                        FactProvenance::LifecyclePrefix {
                            cursor: terminal.cursor.0,
                        },
                    )
                })
            });
        let aggregate_running_transition =
            Self::should_append_aggregate_running(record, persisted_status);
        if aggregate_running_transition && let Some(key) = aggregate_running_key.clone() {
            // A recovery refresh may see the same opening while the aggregate
            // status and lifecycle prefix catch up independently. Reuse the one
            // interval projection identity owner; repeated reads are not events.
            Self::append_aggregate_running_once(record, key);
        }
        // A warm reader or a restarted replica may first observe a Run only
        // after its terminal commit and the aggregate has already returned to
        // Idle. Reconstruct the missing opening edge before projecting output;
        // otherwise cold recovery yields agent output before
        // `session.status_running`, unlike the ordinary foreground path. The
        // terminal lifecycle cursor plus the aggregate Event tail are the
        // existing idempotency authorities, so no create-specific marker is
        // needed. An active child would keep the durable aggregate Running; an
        // Idle root is therefore already the closed-interval case.
        if projected_root_terminal.is_some()
            && persisted_status == SessionStatus::Idle
            && !aggregate_running_transition
            && Self::needs_aggregate_running_edge(record)
            && let Some(key) = aggregate_running_key
        {
            // A recovery refresh may see the same opening while the aggregate
            // status and lifecycle prefix catch up independently. Reuse the one
            // interval projection identity owner; repeated reads are not events.
            Self::append_aggregate_running_once(record, key);
        }
        // Root Thread lifecycle is projected from the same accepted lifecycle
        // prefix as the aggregate Session edges. A foreground command already
        // inserted the bracket when its terminal cursor is present in the one
        // terminal set; otherwise warm/cold recovery emits every newly accepted
        // Running/Rescheduled edge. If a cold prefix starts at a terminal, the
        // event tail reconstructs the missing opening edge before output.
        let project_root_transitions = root_terminal.is_none() || projected_root_terminal.is_some();
        let mut projected_root_opening = false;
        if project_root_transitions {
            for lifecycle in accepted_lifecycle
                .iter()
                .filter(|event| event.thread_id.0 == session_id && is_opening(event.kind))
            {
                let at = |role| {
                    FactKey::new(
                        session_id,
                        role,
                        FactProvenance::Lifecycle {
                            cursor: lifecycle.cursor.0,
                        },
                    )
                };
                let transitions: &[(FactRole, FactKind)] = match lifecycle.kind {
                    RunLifecycleEventKind::Running | RunLifecycleEventKind::Resumed => &[(
                        FactRole::RootLifecycleStatusRunning,
                        FactKind::ThreadRunning,
                    )],
                    RunLifecycleEventKind::Rescheduled => &[
                        (
                            FactRole::RootLifecycleSessionStatusRescheduled,
                            FactKind::SessionRescheduled,
                        ),
                        (
                            FactRole::RootLifecycleStatusRescheduled,
                            FactKind::ThreadRescheduled,
                        ),
                        (
                            FactRole::RootRescheduledLifecycleStatusRunning,
                            FactKind::ThreadRunning,
                        ),
                    ],
                    _ => unreachable!("filtered root lifecycle transition"),
                };
                for (role, kind) in transitions {
                    record.publish_fact(&TimelineFact::new(at(*role), kind.clone()), None)?;
                }
                projected_root_opening = true;
            }
        }
        if !projected_root_opening
            && !Self::primary_thread_status_is_open(record)
            && let Some(terminal) = projected_root_terminal
        {
            Self::append_terminal_thread_running_once(record, terminal);
        }
        if !evidence.root_blocked(session_id)
            && let Some(snapshot) = evidence.root.as_ref()
        {
            Self::append_run_observation_projections(record, session_id, snapshot)?;
        }
        let root_pending_source_run_id = root_terminal
            .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
            .map(|event| &event.run_id);
        let mut pending_is_in_new_message = false;
        let folds = awaken_managed_session_events::plan_folds(
            session_id,
            &new_messages,
            &evidence.root_response_coordinates,
            historical_pending,
            all_accepted_lifecycle,
            root_pending_source_run_id,
        );
        for fold in folds {
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
            pending_is_in_new_message |= selected.pending_occurrence;
            // The exact current ticket wins over any earlier call in this
            // source message. Historical permissions are applied per call by
            // the shared encoder, never chosen by HashMap iteration order.
            let message_pending = selected
                .retained_pending_occurrence
                .then_some(root_pending)
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
        // A committed ticket may precede its ToolUse transcript block. Retain
        // the existing synthetic path, qualified by the Awaiting Run, but only
        // when no revisitable source Message already owns that occurrence.
        if root_pending.is_some() && !pending_is_in_new_message {
            let tool_index = record.projected_tool_index_for(None);
            let mut projected = project_messages_with_mcp_ids(
                &[],
                root_pending,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
                &[],
            );
            self.qualify_tool_projection(
                record,
                None,
                &[],
                root_pending_source_run_id,
                &mut projected,
            )?;
            for projected in projected {
                let id = projected.id.ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "synthetic root pending projection has no qualified tool id",
                    ))
                })?;
                record.append_event_once(Event {
                    id,
                    kind: projected.kind,
                    processed_at: None,
                });
            }
        }

        // Outcome observation is an immediate, side-effect-free committed fact.
        // The retained DefineOutcome selects the aggregate: Completed adds only
        // evaluation facts, while Errored adds one infrastructure error and no
        // partial evaluation. General transcript/lifecycle projection remains
        // the sole owner unless an exact failure source Run is correlated below.
        append_durable_outcome_projections(record, &evidence.outcomes);
        Ok(RootProjection {
            root_terminal,
            projected_root_terminal,
            projected_budget_reach,
        })
    }
}
