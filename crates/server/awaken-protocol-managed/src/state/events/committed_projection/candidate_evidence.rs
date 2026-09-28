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
use awaken_managed_session_events::{AcceptanceFence, LifecycleAcceptance, accept_lifecycle};
use std::collections::{HashMap, HashSet};

/// One budget-reach transition whose owning interval has closed, with its
/// public usage and limit already priced.
pub(super) type BudgetReachProjection = (u64, crate::types::Usage, crate::types::BudgetLimit);

pub(super) struct CandidateEvidence {
    pub(super) root: Option<RunRecoverySnapshot>,
    pub(super) links: Vec<CoordinatedThreadLink>,
    pub(super) children: awaken_session_application::ProjectionThreadSnapshots,
    pub(super) outcomes: Vec<AnchoredOutcomeProjection>,
    /// Child transcripts, minus every Thread whose transcript is blocked on a
    /// ticket the prefix does not expose yet.
    pub(super) transcripts: HashMap<String, Vec<awaken_agent_contract::agent::message::Message>>,
    pub(super) child_latest_run_states:
        HashMap<String, awaken_agent_contract::agent::run::RunState>,
    pub(super) child_pending: HashMap<String, Pending>,
    pub(super) child_dispositions: HashMap<String, awaken_agent_contract::ThreadDisposition>,
    pub(super) root_pending: Option<Pending>,
    pub(super) root_response_coordinates: HashMap<String, (String, usize, usize)>,
    pub(super) durable_inbound: Vec<DurableInboundProjection>,
    pub(super) usage: SessionUsage,
    pub(super) primary_thread_usage: Option<crate::types::SessionThreadUsage>,
    pub(super) child_thread_usage: HashMap<String, Option<crate::types::SessionThreadUsage>>,
    pub(super) price_snapshot: Option<awaken_session_budget_contract::ManagedListPriceSnapshot>,
    pub(super) budget_reach: Vec<BudgetReachProjection>,
    /// The complete accepted lifecycle prefix and its checkpoint cursor.
    pub(super) acceptance: LifecycleAcceptance,
    /// The accepted suffix beyond the base record's lifecycle checkpoint.
    pub(super) new_lifecycle: Vec<RunLifecycleEvent>,
    pub(super) historical_pending: HashMap<RunLifecycleCursor, Pending>,
    pub(super) timings: awaken_session_application::SessionProjectionReadTimings,
    pub(super) usage_done: std::time::Duration,
}

impl CandidateEvidence {
    pub(super) async fn read(
        state: &ManagedState,
        session_id: &str,
        persisted: &awaken_session_state_contract::PersistedSession,
        initial_cursor: RunLifecycleCursor,
        started: std::time::Instant,
    ) -> Result<Self, StateError> {
        let price_snapshot = persisted.budget.price_snapshot().cloned();
        let budget_reach = persisted
            .budget
            .reach_transitions()
            .iter()
            .filter(|transition| {
                awaken_managed_session_events::budget_reach_close_cursor(persisted, transition)
                    .is_some()
            })
            .map(|transition| {
                let mut usage = session_usage_value(
                    transition
                        .usage_cursor
                        .to_session_usage()
                        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?,
                    None,
                )
                .map_err(StateError::Run)?;
                usage.list_cost = transition.public_list_cost_minor().map(|amount| {
                    crate::types::MonetaryAmount {
                        amount: amount.to_string(),
                        currency: crate::types::Currency::USD,
                    }
                });
                Ok((
                    transition.generation,
                    usage,
                    crate::types::BudgetLimit::Limit {
                        max_list_cost: crate::types::MonetaryAmount {
                            amount: transition.max_list_cost_minor.to_string(),
                            currency: crate::types::Currency::USD,
                        },
                    },
                ))
            })
            .collect::<Result<Vec<_>, StateError>>()?;
        // The Session application reads the Runtime evidence through its
        // read-only projection port, in the order that keeps it one prefix:
        // retained terminal Outcomes before the root fence, then the root,
        // its coordinated links and children, then their merged lifecycle.
        let awaken_session_application::SessionProjectionEvidence {
            outcome_terminals,
            root,
            links,
            children,
            lifecycle,
            timings,
        } = state
            .application
            .read_projection_evidence(persisted, started)
            .await
            .map_err(StateError::Run)?;
        let outcomes = outcome_terminals
            .into_iter()
            .map(|(outcome_id, terminal)| validate_durable_outcome_projection(outcome_id, terminal))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|projection| projection.anchor(root.as_ref()))
            .collect::<Vec<_>>();
        let mut transcripts = children
            .iter()
            .map(|(thread_id, snapshot)| (thread_id.clone(), snapshot.messages.clone()))
            .collect::<HashMap<_, _>>();
        // A child with no committed Run yet has no state to project; a damaged
        // prefix is surfaced instead of silently dropping the child's state.
        let mut child_latest_run_states = HashMap::new();
        for (thread_id, snapshot) in &children {
            if let Some(run) = ManagedState::current_recovery_run(snapshot)? {
                child_latest_run_states.insert(thread_id.clone(), run.state.clone());
            }
        }
        let mut child_pending = HashMap::new();
        for (thread_id, snapshot) in &children {
            if let Some(pending) = ManagedState::pending_from_recovery_snapshot(snapshot)? {
                child_pending.insert(thread_id.clone(), pending);
            }
        }
        let child_dispositions = children
            .iter()
            .map(|(thread_id, snapshot)| {
                awaken_agent_contract::thread_disposition_from_committed_state(&snapshot.state)
                    .map(|disposition| (thread_id.clone(), disposition))
                    .map_err(|error| {
                        StateError::Run(RunError::internal(format!(
                            "recover coordinated Thread disposition: {error}"
                        )))
                    })
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        let root_pending = root
            .as_ref()
            .map(ManagedState::pending_from_recovery_snapshot)
            .transpose()?
            .flatten();
        let root_response_coordinates = root.as_ref().map_or_else(HashMap::new, |snapshot| {
            let run_ids = snapshot
                .runs
                .iter()
                .map(|run| run.id.clone())
                .collect::<Vec<_>>();
            ManagedState::assistant_response_coordinates(&snapshot.messages, &run_ids)
        });
        let durable_inbound = durable_inbound_projections(session_id, &persisted.event_batches);
        // The same frozen snapshots already own per-Thread usage below. Avoid
        // reopening every Thread and the Session root for the aggregate, which
        // would read a second accounting prefix only to validate it away later.
        // Child snapshots are keyed by distinct Thread; root is counted once.
        // Final source validation and the existing publication CAS still fence
        // the entire candidate, including this aggregate.
        let usage = awaken_session_application::SessionApplication::project_session_usage(
            persisted,
            root.iter()
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
            state.application.now_unix_ms(),
        );
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

        let known_children = links
            .iter()
            .map(|link| link.thread_id.0.as_str())
            .collect::<HashSet<_>>();
        let acceptance = accept_lifecycle(
            lifecycle,
            &AcceptanceFence {
                session_id,
                root: root.as_ref(),
                children: &children,
                known_children: &known_children,
            },
            initial_cursor,
            |thread_id| {
                if thread_id == session_id {
                    root_pending.is_some()
                } else {
                    child_pending.contains_key(thread_id)
                }
            },
        );
        let historical_pending = awaken_managed_session_events::historical_pending_by_lifecycle(
            &acceptance.accepted,
            |thread_id| {
                if thread_id == session_id {
                    root.as_ref()
                } else {
                    children.get(thread_id)
                }
            },
        )
        .map_err(projection_error)?;
        for thread_id in &acceptance.blocked_threads {
            transcripts.remove(thread_id);
        }
        let new_lifecycle = acceptance
            .accepted
            .iter()
            .filter(|event| event.cursor > initial_cursor)
            .cloned()
            .collect();
        Ok(Self {
            root,
            links,
            children,
            outcomes,
            transcripts,
            child_latest_run_states,
            child_pending,
            child_dispositions,
            root_pending,
            root_response_coordinates,
            durable_inbound,
            usage,
            primary_thread_usage,
            child_thread_usage,
            price_snapshot,
            budget_reach,
            acceptance,
            new_lifecycle,
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
    pub(super) fn delegation(&self) -> DelegationProjectionEvidence<'_> {
        DelegationProjectionEvidence {
            links: &self.links,
            snapshots: &self.children,
            transcripts: &self.transcripts,
            lifecycle_events: &self.new_lifecycle,
            latest_run_states: &self.child_latest_run_states,
            pending: &self.child_pending,
            historical_pending: &self.historical_pending,
            dispositions: &self.child_dispositions,
            usage: &self.child_thread_usage,
        }
    }
}
