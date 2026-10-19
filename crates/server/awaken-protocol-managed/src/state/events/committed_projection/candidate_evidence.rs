// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! The committed evidence of one projection candidate and what it derives.
//!
//! Read once through the Session application's read-only projection port,
//! then derived without further I/O: the accepted lifecycle prefix, per-Thread
//! answerable tickets, dispositions, usage and the anchored Outcomes. Every
//! later stage of the candidate borrows this value; nothing here mutates the
//! record. Consumer: the candidate builder in `committed_projection`.

use super::*;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_managed_session_events::LifecycleAcceptance;
use std::collections::HashMap;

/// Every retained terminal Outcome the root prefix anchors; an invalid one
/// fails the candidate.
fn committed_outcomes(
    terminals: impl IntoIterator<Item = (String, CommittedOutcomeProjection)>,
    root: Option<&RunRecoverySnapshot>,
) -> Result<Vec<awaken_managed_session_events::CommittedOutcome>, StateError> {
    terminals
        .into_iter()
        .filter_map(|(outcome_id, terminal)| {
            awaken_managed_session_events::CommittedOutcome::select(outcome_id, terminal, root)
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(projection_error)
}

pub(super) struct CandidateEvidence {
    pub(super) root: Option<RunRecoverySnapshot>,
    pub(super) links: Vec<CoordinatedThreadLink>,
    pub(super) children: awaken_session_application::ProjectionThreadSnapshots,
    pub(super) outcomes: Vec<awaken_managed_session_events::CommittedOutcome>,
    pub(super) outcome_query_incomplete: bool,
    /// Child transcripts, minus every Thread whose transcript is blocked on a
    /// ticket the prefix does not expose yet.
    pub(super) transcripts: HashMap<String, Vec<awaken_agent_contract::agent::message::Message>>,
    pub(super) child_latest_run_states:
        HashMap<String, awaken_agent_contract::agent::run::RunState>,
    pub(super) child_pending: HashMap<String, Pending>,
    pub(super) child_dispositions: HashMap<String, awaken_agent_contract::ThreadDisposition>,
    pub(super) root_response_coordinates: HashMap<String, (String, usize, usize)>,
    pub(super) durable_inbound: Vec<DurableInboundProjection>,
    pub(super) usage: SessionUsage,
    pub(super) primary_thread_usage: Option<crate::types::SessionThreadUsage>,
    pub(super) child_thread_usage: HashMap<String, Option<crate::types::SessionThreadUsage>>,
    pub(super) price_snapshot: Option<awaken_session_budget_contract::ManagedListPriceSnapshot>,
    pub(super) budget_reach: Vec<awaken_managed_session_events::BudgetReach>,
    /// The complete accepted lifecycle prefix and its checkpoint cursor.
    pub(super) acceptance: LifecycleAcceptance,
    pub(super) historical_pending: HashMap<RunLifecycleCursor, Pending>,
    pub(super) timings: awaken_session_application::SessionProjectionReadTimings,
    pub(super) usage_done: std::time::Duration,
}

impl CandidateEvidence {
    /// Derive both existing domain plans from this one fenced prefix before
    /// any disposable record is changed. Source failures abort the candidate.
    pub(super) fn derive_plans(
        &self,
        persisted: &awaken_session_state_contract::PersistedSession,
        index: &OrderIndex<'_>,
    ) -> Result<
        (
            awaken_managed_session_events::AggregatePlan,
            awaken_managed_session_events::CoordinationEvents,
        ),
        StateError,
    > {
        let aggregate = awaken_managed_session_events::plan_aggregate(
            persisted,
            &self.acceptance.accepted,
            &self.historical_pending,
            &self.links,
            index,
        )
        .map_err(projection_error)?;
        let coordination = awaken_managed_session_events::derive_coordination_events(
            &awaken_managed_session_events::CoordinationEvidence {
                session_id: persisted.session_id.as_str(),
                root: self.root.as_ref(),
                root_blocked: self.root_blocked(persisted.session_id.as_str()),
                root_response_coordinates: &self.root_response_coordinates,
                links: &self.links,
                lifecycle: &self.acceptance.accepted,
                historical_pending: &self.historical_pending,
                order_index: index,
            },
            managed_tool_event_id,
        )
        .map_err(projection_error)?;
        Ok((aggregate, coordination))
    }

    pub(super) async fn read(
        state: &ManagedState,
        session_id: &str,
        persisted: &awaken_session_state_contract::PersistedSession,
        started: awaken_time::mono::Stopwatch,
    ) -> Result<Self, StateError> {
        let price_snapshot = persisted.budget.price_snapshot().cloned();
        // The Session application reads the Runtime evidence through its
        // read-only projection port, in the order that keeps it one prefix:
        // the root snapshot before terminal Outcome queries,
        // its coordinated links and children, then their merged lifecycle.
        let awaken_session_application::CommittedSessionFacts {
            outcome_terminals,
            outcome_query_incomplete,
            root,
            links,
            children,
            lifecycle,
            timings,
        } = state
            .roles
            .projection
            .read_projection_evidence(persisted, started)
            .await
            .map_err(StateError::Run)?;
        // ThreadCommit validation does not have the previous transcript, so
        // a repeated Message id can reach recovery. Exact replay is
        // idempotent; conflicting content under one id cannot select a stable
        // public payload or result cursor and must fail before any candidate
        // changes the warm cache.
        for snapshot in root.iter().chain(children.values()) {
            awaken_managed_session_events::validate_committed_message_identities(&snapshot.messages)
                .map_err(|error| {
                    StateError::projection_recovery_required(format!(
                        "Runtime recovery for Thread {:?} has conflicting committed Message identity: {error}; interrupt the Session to settle it",
                        snapshot.thread_id.0
                    ))
                })?;
        }
        let outcomes = committed_outcomes(outcome_terminals, root.as_ref())?;
        let mut transcripts = children
            .iter()
            .map(|(thread_id, snapshot)| (thread_id.clone(), snapshot.messages.clone()))
            .collect::<HashMap<_, _>>();
        // A child with no committed Run yet has no state to project; a damaged
        // prefix is surfaced instead of silently dropping the child's state.
        let awaken_managed_session_events::ChildThreadStates {
            latest_run_states: child_latest_run_states,
            dispositions: child_dispositions,
        } = awaken_managed_session_events::child_thread_states(&children)
            .map_err(projection_error)?;
        let root_response_coordinates = root.as_ref().map_or_else(HashMap::new, |snapshot| {
            let run_ids = snapshot
                .runs
                .iter()
                .map(|run| run.id.clone())
                .collect::<Vec<_>>();
            ManagedState::assistant_response_coordinates(&snapshot.messages, &run_ids)
        });
        let durable_inbound = durable_inbound_projections(session_id, &persisted.event_batches);
        let usage = committed_session_usage(persisted, session_id, root.as_ref(), &children);
        let primary_thread_usage = usage_projection::project_committed_thread_usage(
            root.as_ref(),
            price_snapshot.as_ref(),
        )?;
        let mut child_thread_usage = HashMap::new();
        for link in &links {
            let usage = usage_projection::project_committed_thread_usage(
                children.get(&link.thread_id.0),
                price_snapshot.as_ref(),
            )?;
            child_thread_usage.insert(link.thread_id.0.clone(), usage);
        }
        let usage_done = started.elapsed();

        // The accepted lifecycle and every accepted pause's ticket, each read
        // from that pause's own committed audit, never from a live ticket.
        let accepted = awaken_managed_session_events::accept_committed_lifecycle(
            session_id,
            root.as_ref(),
            &children,
            &links,
            lifecycle,
        )
        .map_err(projection_error)?;
        // Only the reaches whose public pause this accepted committed prefix
        // anchors; one hidden behind an answerable ticket stays withheld.
        let budget_reach = awaken_managed_session_events::BudgetReach::exposed(
            persisted,
            &accepted.acceptance.accepted,
            &accepted.historical_pending,
        )
        .map_err(projection_error)?;
        // A Thread's current answerable call is its latest accepted
        // lifecycle commit when that commit is an audited pause.
        let child_pending = accepted.current_pending(children.keys());
        let awaken_managed_session_events::AcceptedLifecycle {
            acceptance,
            historical_pending,
        } = accepted;
        for thread_id in &acceptance.blocked_threads {
            transcripts.remove(thread_id);
        }
        Ok(Self {
            root,
            links,
            children,
            outcomes,
            outcome_query_incomplete,
            transcripts,
            child_latest_run_states,
            child_pending,
            child_dispositions,
            root_response_coordinates,
            durable_inbound,
            usage,
            primary_thread_usage,
            child_thread_usage,
            price_snapshot,
            budget_reach,
            acceptance,
            historical_pending,
            timings,
            usage_done,
        })
    }

    /// Whether the root transcript is blocked on a ticket not yet visible.
    pub(super) fn root_blocked(&self, session_id: &str) -> bool {
        self.acceptance.blocked_threads.contains(session_id)
    }

    /// The root prefix's committed Messages.
    pub(super) fn root_messages(&self) -> &[awaken_agent_contract::agent::message::Message] {
        self.root
            .as_ref()
            .map(|snapshot| snapshot.messages.as_slice())
            .unwrap_or_default()
    }

    /// The borrowed coordination evidence consumed by the child projector.
    pub(super) fn delegation<'a>(
        &'a self,
        order_index: &'a OrderIndex<'a>,
    ) -> DelegationProjectionEvidence<'a> {
        DelegationProjectionEvidence {
            order_index,
            links: &self.links,
            snapshots: &self.children,
            transcripts: &self.transcripts,
            lifecycle_events: &self.acceptance.accepted,
            latest_run_states: &self.child_latest_run_states,
            pending: &self.child_pending,
            historical_pending: &self.historical_pending,
            dispositions: &self.child_dispositions,
            usage: &self.child_thread_usage,
        }
    }
}

/// The Session usage of one committed prefix.
///
/// The same frozen snapshots already own per-Thread usage. Avoid reopening
/// every Thread and the Session root for the aggregate, which would read a
/// second accounting prefix only to validate it away later. Child snapshots
/// are keyed by distinct Thread; root is counted once. Final source
/// validation and the existing publication CAS still fence the entire
/// candidate, including this aggregate. Active time is the durable closed
/// total only: an open interval has no committed end, so reading the clock
/// here would make the same prefix project different usage at different read
/// times. It advances when the interval closes.
fn committed_session_usage(
    persisted: &awaken_session_state_contract::PersistedSession,
    session_id: &str,
    root: Option<&RunRecoverySnapshot>,
    children: &awaken_session_application::ProjectionThreadSnapshots,
) -> SessionUsage {
    let mut usage = awaken_session_application::SessionApplication::project_session_usage(
        persisted,
        root.into_iter()
            .chain(
                children
                    .iter()
                    .filter(|(id, _)| id.as_str() != session_id)
                    .map(|(_, snapshot)| snapshot),
            )
            .map(|snapshot| {
                awaken_session_application_contract::session_usage_from_thread_usage(
                    awaken_runtime_contract::llm::ThreadUsage::from_committed_state(
                        &snapshot.state,
                    ),
                )
            }),
        0,
    );
    usage.active_seconds = persisted.runtime_active_millis / 1_000;
    usage
}
