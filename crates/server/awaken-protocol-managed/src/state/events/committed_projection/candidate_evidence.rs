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
use awaken_time::WallClockExt as _;
use std::collections::{HashMap, HashSet};

/// One budget-reach transition whose owning interval has closed, with its
/// public usage and limit already priced.
pub(super) type BudgetReachProjection = (u64, crate::types::Usage, crate::types::BudgetLimit);

pub(super) struct CandidateEvidence {
    pub(super) root: Option<RunRecoverySnapshot>,
    pub(super) links: Vec<CoordinatedThreadLink>,
    pub(super) children: awaken_session_application::ProjectionThreadSnapshots,
    pub(super) outcomes: Vec<AnchoredOutcomeProjection>,
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
    pub(super) budget_reach: Vec<BudgetReachProjection>,
    /// The complete accepted lifecycle prefix and its checkpoint cursor.
    pub(super) acceptance: LifecycleAcceptance,
    pub(super) historical_pending: HashMap<RunLifecycleCursor, Pending>,
    pub(super) timings: awaken_session_application::SessionProjectionReadTimings,
    pub(super) usage_done: std::time::Duration,
}

impl CandidateEvidence {
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
            state.roles.clock.now_ms(),
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
        // The target of every Awaiting commit, read from that commit's own
        // audit row. A live ticket is never an input: the audit is committed
        // with the ticket and outlives it, so a pause classifies the same
        // before and after its ticket is consumed. A pause whose target no
        // client answers (a scheduled action, a delegation) is still
        // classified by it; only a pause that left no target blocks.
        let targets =
            awaken_managed_session_events::audited_await_targets(&lifecycle, |thread_id| {
                if thread_id == session_id {
                    root.as_ref()
                } else {
                    children.get(thread_id)
                }
            })
            .map_err(projection_error)?;
        // Health and fact admission consume the same exact-cursor audit map.
        // A later repeated Awaiting audit cannot authorize an older wait.
        for snapshot in root.iter().chain(children.values()) {
            ManagedState::committed_pause_health(snapshot, &lifecycle, &targets)?;
        }
        let audited = targets
            .iter()
            .filter_map(|(cursor, target)| Some((*cursor, Pending::from_await_target(target)?)))
            .collect::<HashMap<_, _>>();
        let acceptance = accept_lifecycle(
            lifecycle,
            &AcceptanceFence {
                session_id,
                root: root.as_ref(),
                children: &children,
                known_children: &known_children,
            },
            // Every candidate begins from an empty committed timeline; the
            // acceptance fence sees the full durable lifecycle prefix.
            RunLifecycleCursor(0),
            |event| targets.contains_key(&event.cursor),
        );
        let historical_pending = acceptance
            .accepted
            .iter()
            .filter_map(|event| {
                audited
                    .get(&event.cursor)
                    .map(|pending| (event.cursor, pending.clone()))
            })
            .collect::<HashMap<_, _>>();
        // Price only the transitions whose public pause is anchored by this
        // accepted committed prefix. A crossing hidden behind an answerable
        // ticket cannot be inferred from the current Session status.
        let budget_reach = persisted
            .budget
            .reach_transitions()
            .iter()
            .filter(|transition| {
                awaken_managed_session_events::budget_reach_projection_close_cursor(
                    persisted,
                    transition,
                    &acceptance.accepted,
                    &historical_pending,
                )
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
        // A Thread's current answerable call is its latest accepted
        // lifecycle commit when that commit is an audited pause.
        let mut latest_by_thread = HashMap::<&str, &RunLifecycleEvent>::new();
        for event in &acceptance.accepted {
            latest_by_thread.insert(event.thread_id.0.as_str(), event);
        }
        let current_pending = |thread_id: &str| {
            latest_by_thread
                .get(thread_id)
                .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
                .and_then(|event| historical_pending.get(&event.cursor))
                .cloned()
        };
        let child_pending = children
            .keys()
            .filter_map(|thread_id| {
                current_pending(thread_id).map(|pending| (thread_id.clone(), pending))
            })
            .collect::<HashMap<_, _>>();
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

    /// The Session usage as of one commit: every Thread's committed state
    /// commands up to `source_commit_cursor`, and the active time of the
    /// intervals closed by then. A pause published at a boundary carries this
    /// value, so reading it later does not change it.
    pub(super) fn usage_as_of(
        &self,
        persisted: &awaken_session_state_contract::PersistedSession,
        session_id: &str,
        source_commit_cursor: u64,
    ) -> SessionUsage {
        let thread_usage = |snapshot: &RunRecoverySnapshot| {
            let state = if snapshot.state_commit_cursors.len() == snapshot.state.len() {
                snapshot
                    .state
                    .iter()
                    .zip(&snapshot.state_commit_cursors)
                    .filter(|(_, cursor)| **cursor <= source_commit_cursor)
                    .map(|(command, _)| command.clone())
                    .collect::<Vec<_>>()
            } else {
                snapshot.state.clone()
            };
            awaken_session_application_contract::session_usage_from_thread_usage(
                awaken_runtime_contract::llm::ThreadUsage::from_committed_state(&state),
            )
        };
        let mut usage = awaken_session_application::SessionApplication::project_session_usage(
            persisted,
            self.root.iter().map(thread_usage).chain(
                self.children
                    .iter()
                    .filter(|(id, _)| id.as_str() != session_id)
                    .map(|(_, snapshot)| thread_usage(snapshot)),
            ),
            0,
        );
        let active_millis = persisted
            .closed_runtime_intervals
            .iter()
            .filter(|interval| {
                awaken_managed_session_events::interval_close_cursor(interval)
                    .is_some_and(|close| close <= source_commit_cursor)
            })
            .map(|interval| {
                interval
                    .ended_at_unix_ms
                    .saturating_sub(interval.started_at_unix_ms)
            })
            .sum::<u64>();
        usage.active_seconds = active_millis / 1_000;
        usage
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
            lifecycle_events: &self.acceptance.accepted,
            latest_run_states: &self.child_latest_run_states,
            pending: &self.child_pending,
            historical_pending: &self.historical_pending,
            dispositions: &self.child_dispositions,
            usage: &self.child_thread_usage,
        }
    }
}
