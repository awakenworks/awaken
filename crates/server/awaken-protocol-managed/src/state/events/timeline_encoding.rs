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
    FactKind, FactRole, IssuedToolEvent, RunFault, StopFact, TimelineFact,
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
            let latest = record.projected_tool_index_for(owner_thread_id).latest;
            StopReason::RequiresAction {
                event_ids: calls
                    .iter()
                    .map(|call| {
                        latest.get(call).cloned().ok_or_else(|| {
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
    fact: &TimelineFact,
    price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
) -> Result<OutboundKind, StateError> {
    let thread_id = fact.key.thread.as_str();
    let primary = thread_id == record.session.id;
    Ok(match &fact.kind {
        FactKind::ThreadCreated => OutboundKind::SessionThreadCreated {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        FactKind::ThreadRunning if primary => {
            primary_thread_status_kind(record, PrimaryThreadStatusProjection::Running)
        }
        FactKind::ThreadRunning => OutboundKind::SessionThreadStatusRunning {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        FactKind::ThreadRescheduled if primary => {
            primary_thread_status_kind(record, PrimaryThreadStatusProjection::Rescheduled)
        }
        FactKind::ThreadRescheduled => OutboundKind::SessionThreadStatusRescheduled {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        FactKind::ThreadIdle(stop) if primary => primary_thread_status_kind(
            record,
            PrimaryThreadStatusProjection::Idle {
                stop_reason: resolve_stop(record, None, stop)?,
            },
        ),
        FactKind::ThreadIdle(stop) => OutboundKind::SessionThreadStatusIdle {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
            stop_reason: resolve_stop(record, Some(thread_id), stop)?,
        },
        FactKind::ThreadTerminated if primary => {
            primary_thread_status_kind(record, PrimaryThreadStatusProjection::Terminated)
        }
        FactKind::ThreadTerminated => OutboundKind::SessionThreadStatusTerminated {
            session_thread_id: thread_id.to_string(),
            agent_name: child_name(record, thread_id)?,
        },
        FactKind::RunError { fault, advisor } => OutboundKind::SessionError {
            error: if *advisor {
                SessionError::classify("advisor_consultation_failed", ADVISOR_FAILURE_NOTICE)
            } else {
                fault_error(fault)
            },
        },
        FactKind::SessionRunning => OutboundKind::SessionStatusRunning {},
        FactKind::SessionRescheduled => OutboundKind::SessionStatusRescheduled {},
        FactKind::SessionIdle { stop, calls_owner } => OutboundKind::SessionStatusIdle {
            stop_reason: resolve_stop(record, owner(record, calls_owner).as_deref(), stop)?,
        },
        FactKind::SessionTerminated => OutboundKind::SessionStatusTerminated {},
        FactKind::IntervalUsage(interval) => {
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
        FactKind::ContextCompacted => OutboundKind::ThreadContextCompacted {},
        FactKind::ModelRequestStart => OutboundKind::SpanModelRequestStart {},
        FactKind::ModelRequestEnd {
            start,
            is_error,
            usage,
        } => OutboundKind::SpanModelRequestEnd {
            model_request_start_id: record.fact_id(start),
            is_error: Some(*is_error),
            model_usage: SpanModelUsage::from(*usage),
        },
        FactKind::MessageSent { message } => OutboundKind::AgentThreadMessageSent {
            to_session_thread_id: thread_id.to_string(),
            to_agent_name: Some(child_name(record, thread_id)?),
            content: vec![ContentBlock::text(message.clone())],
        },
        FactKind::ReportReceived { text } => OutboundKind::AgentThreadMessageReceived {
            from_session_thread_id: thread_id.to_string(),
            from_agent_name: Some(child_name(record, thread_id)?),
            content: vec![ContentBlock::text(text.clone())],
        },
        FactKind::ActivationFailed { message } => OutboundKind::SessionError {
            error: SessionError::classify(
                awaken_session_state_contract::SessionExecutionState::ActivationFailed.as_str(),
                message.clone(),
            ),
        },
    })
}

/// How publishing a fact updates the disposable coordinated Thread record.
fn apply_thread_effect(record: &mut SessionRecord, fact: &TimelineFact) {
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
        FactRole::LifecycleStatusRunning => {
            child.status = SessionThreadStatus::Running;
            child.updated_at = unrecorded();
        }
        FactRole::LifecycleStatusRescheduled => {
            child.status = SessionThreadStatus::Rescheduling;
            child.updated_at = unrecorded();
        }
        FactRole::RescheduledLifecycleStatusRunning => {
            child.status = SessionThreadStatus::Running;
        }
        FactRole::CoordinationStatusRunning => {
            child.status = SessionThreadStatus::Running;
            child.updated_at = unrecorded();
        }
        FactRole::LifecycleStatusIdle => {
            child.status = SessionThreadStatus::Idle;
            child.updated_at = unrecorded();
        }
        FactRole::FailedLifecycleStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.updated_at = unrecorded();
        }
        FactRole::AdvisorLifecycleStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.archived_at = Some(unrecorded());
        }
        FactRole::ArchivedStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.archived_at = Some(unrecorded());
            child.updated_at = unrecorded();
        }
        FactRole::ParentTerminalChildStatusTerminated => {
            child.status = SessionThreadStatus::Terminated;
            child.updated_at = archived_at.clone().unwrap_or_else(unrecorded);
            child.archived_at = archived_at;
        }
        _ => {}
    }
}

/// Facts shown only on their own child Thread's stream.
fn child_stream_only(role: FactRole) -> bool {
    matches!(
        role,
        FactRole::LifecycleError
            | FactRole::RunContextCompacted
            | FactRole::ModelRequestStart
            | FactRole::ModelRequestEnd
    )
}

impl SessionRecord {
    /// Encode and append one timeline fact, then apply its Thread effect.
    pub(super) fn publish_fact(
        &mut self,
        fact: &TimelineFact,
        price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
    ) -> Result<(), StateError> {
        let kind = encode_fact(self, fact, price_snapshot)?;
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
        fact: &TimelineFact,
        price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
    ) -> Result<bool, StateError> {
        if self.has_event_id(&self.fact_id(&fact.key)) {
            return Ok(false);
        }
        self.publish_fact(fact, price_snapshot)?;
        Ok(true)
    }
}

/// The published built-in tool uses and results, seen neutrally, in
/// publication order.
pub(super) fn issued_tool_events(record: &SessionRecord) -> Vec<IssuedToolEvent<'_>> {
    let session_id = record.session.id.as_str();
    let primary = public_thread_id(session_id, session_id);
    record
        .events
        .iter()
        .filter_map(|event| {
            let owned = record.event_thread_owners.contains_key(&event.id);
            match &event.kind {
                OutboundKind::AgentToolUse { name, input, .. } => {
                    let identity = decode_managed_tool_event_id(&event.id);
                    Some(IssuedToolEvent::Use {
                        event_id: &event.id,
                        call_id: identity.map_or(event.id.as_str(), |identity| identity.call_id),
                        source: identity.map(|identity| {
                            let thread = if identity.thread_id == primary {
                                session_id
                            } else {
                                identity.thread_id
                            };
                            (thread, identity.source_id)
                        }),
                        name,
                        input,
                        owned,
                    })
                }
                OutboundKind::AgentToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some(IssuedToolEvent::Result {
                    event_id: &event.id,
                    tool_use_id,
                    content,
                    is_error: *is_error,
                    owned,
                }),
                _ => None,
            }
        })
        .collect()
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
