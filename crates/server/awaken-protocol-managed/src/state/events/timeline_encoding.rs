// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Managed wire encoding consumes the existing derived timeline and a borrowed
// @formal reason: retained update fact, checking its Session/revision identity before rendering it.
// @formal reason: No event or payload is reconstructed from current catalog state; protocol tests
// @formal reason: own wire compatibility and cold/warm evidence, not external effect qualification.
//! Managed wire encoding of timeline facts.
//!
//! The timeline domain decides which facts exist and what each says; this
//! module is the one place those facts become Managed events: public Thread
//! ids and Agent names, the wire stop reason (each answerable call resolved to
//! its public tool event), error classification, priced usage, and the
//! disposable Thread records and stream owners a fact updates.
//!
//! Consumers: every Managed projector that publishes a timeline fact.

use super::*;
use awaken_managed_session_events::{
    DerivedSessionEvent, ManagedEventType, RunFault, SessionEventKind, StopFact,
};

/// Resolve a stop fact to the Managed stop reason, naming each answerable
/// call by the public tool event that `owner_thread_id`'s stream published.
/// A pause whose call has no published event fails the candidate closed.
pub(super) fn resolve_stop(
    record: &SessionRecord,
    owner_thread_id: Option<&str>,
    stop: &StopFact,
) -> Result<StopReason, StateError> {
    Ok(match stop {
        StopFact::EndTurn => StopReason::EndTurn,
        StopFact::RetriesExhausted => StopReason::RetriesExhausted,
        StopFact::BudgetReached => StopReason::BudgetReached,
        StopFact::RequiresAction { calls } => {
            let index = record.projected_tool_index_for(owner_thread_id);
            StopReason::RequiresAction {
                event_ids: calls
                    .iter()
                    .map(|call| {
                        index.latest_id(call).ok_or_else(|| {
                            StateError::Run(RunError::internal(
                                "Managed pending ticket has no answerable public tool event",
                            ))
                        })
                    })
                    .collect::<Result<_, _>>()?,
            }
        }
    })
}

/// The Managed error of one Run fault.
pub(super) fn fault_error(fault: &RunFault) -> SessionError {
    SessionError::from_fault(fault)
}

fn owner(record: &SessionRecord, thread_id: &str) -> Option<String> {
    (thread_id != record.session.id).then(|| thread_id.to_string())
}

fn child_name(record: &SessionRecord, thread_id: &str) -> Result<String, StateError> {
    record
        .child_threads
        .iter()
        .find(|child| child.id == thread_id)
        .map(|child| child.agent.display_name().to_string())
        .ok_or_else(|| {
            StateError::Run(RunError::internal(format!(
                "timeline fact names coordinated Thread `{thread_id}` outside this Session"
            )))
        })
}

/// Encode one timeline fact as the Managed event kind it becomes.
pub(super) fn encode_fact(
    record: &SessionRecord,
    fact: &DerivedSessionEvent,
    price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
    update: Option<(&str, &awaken_session_state_contract::SessionUpdateFact)>,
) -> Result<OutboundKind, StateError> {
    let thread_id = fact.key.thread.as_str();
    let primary = thread_id == record.session.id;
    Ok(match &fact.kind {
        SessionEventKind::SessionUpdated => encode_session_update(record, fact, update)?,
        SessionEventKind::ThreadCreated => OutboundKind::SessionThreadCreated {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        SessionEventKind::ThreadRunning if primary => {
            primary_thread_status_kind(record, PrimaryThreadStatusProjection::Running)
        }
        SessionEventKind::ThreadRunning => OutboundKind::SessionThreadStatusRunning {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        SessionEventKind::ThreadRescheduled if primary => {
            primary_thread_status_kind(record, PrimaryThreadStatusProjection::Rescheduled)
        }
        SessionEventKind::ThreadRescheduled => OutboundKind::SessionThreadStatusRescheduled {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        SessionEventKind::ThreadIdle(stop) if primary => primary_thread_status_kind(
            record,
            PrimaryThreadStatusProjection::Idle {
                stop_reason: resolve_stop(record, None, stop)?,
            },
        ),
        SessionEventKind::ThreadIdle(stop) => OutboundKind::SessionThreadStatusIdle {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
            stop_reason: resolve_stop(record, Some(thread_id), stop)?,
            stop_details: None,
        },
        SessionEventKind::ThreadTerminated if primary => {
            primary_thread_status_kind(record, PrimaryThreadStatusProjection::Terminated)
        }
        SessionEventKind::ThreadTerminated => OutboundKind::SessionThreadStatusTerminated {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        SessionEventKind::RunError { fault, advisor } => OutboundKind::SessionError {
            error: if *advisor {
                SessionError::classify("advisor_consultation_failed", ADVISOR_FAILURE_NOTICE)
            } else {
                fault_error(fault)
            },
        },
        SessionEventKind::SessionRunning => OutboundKind::SessionStatusRunning {},
        SessionEventKind::SessionRescheduled => OutboundKind::SessionStatusRescheduled {},
        SessionEventKind::SessionIdle { stop, calls_owner } => OutboundKind::SessionStatusIdle {
            stop_reason: resolve_stop(record, owner(record, calls_owner).as_deref(), stop)?,
            stop_details: None,
        },
        SessionEventKind::SessionTerminated => OutboundKind::SessionStatusTerminated {},
        SessionEventKind::IntervalUsage(interval) => {
            let mut usage = session_usage_value(interval.usage.clone(), price_snapshot)
                .map_err(StateError::Run)?;
            let budget = interval.max_list_cost_minor.map(|amount| {
                let max_list_cost = crate::types::MonetaryAmount {
                    amount: amount.to_string(),
                    currency: crate::types::Currency::USD,
                };
                if usage.list_cost.is_none() {
                    usage.list_cost = Some(crate::types::MonetaryAmount {
                        amount: "0".to_string(),
                        currency: crate::types::Currency::USD,
                    });
                }
                crate::types::BudgetLimit::Limit { max_list_cost }
            });
            OutboundKind::SessionUsage { usage, budget }
        }
        SessionEventKind::BudgetReachUsage {
            usage,
            list_cost_minor,
            max_list_cost_minor,
        } => {
            let usd = |amount: u64| crate::types::MonetaryAmount {
                amount: amount.to_string(),
                currency: crate::types::Currency::USD,
            };
            let mut usage = session_usage_value(usage.clone(), None).map_err(StateError::Run)?;
            usage.list_cost = list_cost_minor.map(usd);
            OutboundKind::SessionUsage {
                usage,
                budget: Some(crate::types::BudgetLimit::Limit {
                    max_list_cost: usd(*max_list_cost_minor),
                }),
            }
        }
        SessionEventKind::ContextCompacted => OutboundKind::ThreadContextCompacted {},
        SessionEventKind::ModelRequestStart => OutboundKind::SpanModelRequestStart {},
        SessionEventKind::ModelRequestEnd {
            start,
            is_error,
            usage,
        } => OutboundKind::SpanModelRequestEnd {
            model_request_start_id: record.fact_id(start),
            is_error: Some(*is_error),
            model_usage: SpanModelUsage::from(*usage),
        },
        SessionEventKind::MessageSent { message } => OutboundKind::AgentThreadMessageSent {
            to_session_thread_id: thread_id.to_string(),
            to_agent_name: Some(child_name(record, thread_id)?),
            content: vec![ContentBlock::text(message.clone())],
        },
        SessionEventKind::ReportReceived { text } => OutboundKind::AgentThreadMessageReceived {
            from_session_thread_id: thread_id.to_string(),
            from_agent_name: Some(child_name(record, thread_id)?),
            content: vec![ContentBlock::text(text.clone())],
        },
        SessionEventKind::ActivationFailed { message } => OutboundKind::SessionError {
            error: SessionError::classify(
                awaken_session_state_contract::SessionExecutionState::ActivationFailed.as_str(),
                message.clone(),
            ),
        },
        SessionEventKind::OutcomeEvaluationStart {
            outcome_id,
            iteration,
        } => OutboundKind::SpanOutcomeEvaluationStart {
            outcome_id: outcome_id.clone(),
            iteration: *iteration,
        },
        SessionEventKind::OutcomeEvaluationOngoing {
            outcome_id,
            iteration,
        } => OutboundKind::SpanOutcomeEvaluationOngoing {
            outcome_id: outcome_id.clone(),
            iteration: *iteration,
        },
        SessionEventKind::OutcomeEvaluationEnd(verdict) => OutboundKind::SpanOutcomeEvaluationEnd {
            outcome_id: verdict.outcome_id.clone(),
            outcome_evaluation_start_id: record.fact_id(&verdict.start),
            iteration: verdict.iteration,
            result: verdict.result.clone(),
            explanation: verdict.explanation.clone(),
            usage: verdict.usage.map(SpanModelUsage::from).unwrap_or_default(),
        },
        SessionEventKind::OutcomeError { code, message } => OutboundKind::SessionError {
            error: SessionError::classify(code, message.clone()),
        },
    })
}

/// How publishing a fact updates the disposable coordinated Thread record.
fn apply_thread_effect(record: &mut SessionRecord, fact: &DerivedSessionEvent) {
    let archived_at = record.session.archived_at.clone();
    let Some(child) = record
        .child_threads
        .iter_mut()
        .find(|child| child.id == fact.key.thread)
    else {
        return;
    };
    let unrecorded = || LEGACY_UNRECORDED_AT.to_string();
    match fact.key.role {
        ManagedEventType::LifecycleStatusRunning => {
            child.status = SessionThreadStatus::Running;
            child.updated_at = unrecorded();
        }
        ManagedEventType::LifecycleStatusRescheduled => {
            child.status = SessionThreadStatus::Rescheduling;
            child.updated_at = unrecorded();
        }
        ManagedEventType::RescheduledLifecycleStatusRunning => {
            child.status = SessionThreadStatus::Running;
        }
        ManagedEventType::CoordinationStatusRunning => {
            child.status = SessionThreadStatus::Running;
            child.updated_at = unrecorded();
        }
        ManagedEventType::LifecycleStatusIdle => {
            child.status = SessionThreadStatus::Idle;
            child.updated_at = unrecorded();
        }
        ManagedEventType::FailedLifecycleStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.updated_at = unrecorded();
        }
        ManagedEventType::AdvisorLifecycleStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.archived_at = Some(unrecorded());
        }
        ManagedEventType::ArchivedStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.archived_at = Some(unrecorded());
            child.updated_at = unrecorded();
        }
        ManagedEventType::ParentTerminalChildStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.updated_at = archived_at.clone().unwrap_or_else(unrecorded);
            child.archived_at = archived_at;
        }
        _ => {}
    }
}

/// Facts shown only on their own child Thread's stream.
fn child_stream_only(role: ManagedEventType) -> bool {
    matches!(
        role,
        ManagedEventType::LifecycleError
            | ManagedEventType::RunContextCompacted
            | ManagedEventType::ModelRequestStart
            | ManagedEventType::ModelRequestEnd
    )
}

impl SessionRecord {
    /// Encode and append one timeline fact, then apply its Thread effect.
    pub(super) fn publish_fact(
        &mut self,
        fact: &DerivedSessionEvent,
        price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
    ) -> Result<(), StateError> {
        let kind = encode_fact(self, fact, price_snapshot, None)?;
        if child_stream_only(fact.key.role)
            && let Some(owner) = owner(self, &fact.key.thread)
        {
            let id = self.fact_id(&fact.key);
            self.event_thread_owners.insert(id, owner);
        }
        self.push_fact(fact.key.clone(), kind);
        apply_thread_effect(self, fact);
        Ok(())
    }

    /// Encode and append one timeline fact unless its id is already published.
    pub(super) fn publish_fact_once(
        &mut self,
        fact: &DerivedSessionEvent,
        price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
    ) -> Result<bool, StateError> {
        if self.has_event_id(&self.fact_id(&fact.key)) {
            return Ok(false);
        }
        self.publish_fact(fact, price_snapshot)?;
        Ok(true)
    }
}

/// Encode the complete immutable update fact; it does not decide changes or order.
fn encode_session_update(
    record: &SessionRecord,
    fact: &DerivedSessionEvent,
    source: Option<(&str, &awaken_session_state_contract::SessionUpdateFact)>,
) -> Result<OutboundKind, StateError> {
    let (root_id, update) = source.ok_or_else(|| {
        StateError::Run(RunError::internal(
            "Session update has no immutable Root fact",
        ))
    })?;
    if fact.key.thread != record.session.id
        || root_id != fact.key.thread
        || fact.key.role != ManagedEventType::SessionUpdated
        || fact.key.provenance
            != (awaken_managed_session_events::FactProvenance::SessionUpdate {
                revision: update.committed_revision().0,
            })
    {
        return Err(StateError::Run(RunError::internal(
            "Session update fact identity does not match its Root source",
        )));
    }
    Ok(OutboundKind::SessionUpdated {
        title: update.title().map(|value| match value {
            awaken_session_state_contract::SessionFieldUpdate::Clear => None,
            awaken_session_state_contract::SessionFieldUpdate::Replace(value) => {
                Some(value.clone())
            }
        }),
        metadata: update.metadata().cloned().unwrap_or_default(),
        agent: update
            .agent()
            .map(|value| match value {
                awaken_session_state_contract::SessionUpdateAgentConfiguration::Recorded(value) => {
                    Ok(crate::state::sessions::session_agent_from_configuration(
                        value,
                    ))
                }
                awaken_session_state_contract::SessionUpdateAgentConfiguration::Unrecorded => {
                    Err(StateError::Run(RunError::unavailable(
                        "Session update Agent configuration was not recorded",
                    )))
                }
            })
            .transpose()?,
        budget: update.budget().map(|value| match value {
            awaken_session_state_contract::SessionFieldUpdate::Clear => None,
            awaken_session_state_contract::SessionFieldUpdate::Replace(value) => {
                Some(crate::types::BudgetLimit::from_minor(*value))
            }
        }),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn coordination_send_is_classified_by_the_tool_that_owns_the_id() {
        // The timeline classifies committed `send_message` calls by its own
        // spelling of the id; the builtin coordination tool owns the id.
        assert_eq!(
            awaken_managed_session_events::SEND_MESSAGE_TOOL_ID,
            awaken_ext_builtin_tools::SEND_MESSAGE
        );
    }
}
