// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Child Run states, dispositions, the audit-joined accepted lifecycle and per-Thread
// @formal reason: current tickets, moved unchanged from the Managed candidate evidence. Pure over the
// @formal reason: committed prefixes read once; a damaged prefix or an Awaiting Run without its exact
// @formal reason: audited target fails the candidate closed. Pause-health, acceptance and warm/cold tests
// @formal reason: cover it. Reducer TCB.
//! The committed Thread states and accepted lifecycle one candidate derives.
//!
//! Pure over the facts the Session application read: each child's current
//! Run and disposition, the accepted lifecycle prefix with the answerable
//! ticket of every accepted pause, and each child's current ticket. Nothing
//! here reads a live ticket, a clock or a published projection.
//!
//! Consumer: the Managed adapter's candidate evidence.

use std::collections::{HashMap, HashSet};

use awaken_agent_contract::agent::awaiting::{AwaitReason, AwaitTarget};
use awaken_agent_contract::agent::run::{Record, RunState};
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{
    RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind, ThreadDisposition,
};
use awaken_session_application_contract::Pending;
use awaken_session_execution_contract::CoordinatedThreadLink;

use crate::{
    AcceptanceFence, LifecycleAcceptance, ProjectionError, ProjectionThreadSnapshots,
    accept_lifecycle, audited_await_targets,
};

/// Resolve the one current Run carried by a recovery prefix through the
/// neutral contract rule ([`RunRecoverySnapshot::current_run`]). Strict
/// admission, projection, and interruption share it, so none can invent a
/// latest-versus-claimed fallback or silently skip a damaged prefix.
pub fn current_recovery_run(
    snapshot: &RunRecoverySnapshot,
) -> Result<Option<&Record>, ProjectionError> {
    snapshot.current_run().map_err(|error| {
        ProjectionError::recovery_required(format!(
            "Runtime recovery for Thread {:?} is damaged: {error}; interrupt the Session to settle it",
            snapshot.thread_id.0
        ))
    })
}

/// Refuse a committed prefix whose current Run is paused without the
/// target of that exact lifecycle commit. The timeline's one audit join
/// supplies `targets`; a newer audit without a new lifecycle transition
/// cannot retroactively repair the wait. BudgetReached needs no target.
pub fn committed_pause_health(
    snapshot: &RunRecoverySnapshot,
    lifecycle: &[RunLifecycleEvent],
    targets: &HashMap<RunLifecycleCursor, AwaitTarget>,
) -> Result<(), ProjectionError> {
    let Some(current) = current_recovery_run(snapshot)? else {
        return Ok(());
    };
    if !matches!(current.state, RunState::Awaiting) {
        return Ok(());
    }
    let Some(event) = lifecycle
        .iter()
        .rev()
        .find(|event| event.thread_id == snapshot.thread_id && event.run_id == current.id)
    else {
        return Err(ProjectionError::recovery_required(
            "Runtime recovery exposed an Awaiting Run without its answerable ticket; interrupt the Session to settle it",
        ));
    };
    if event.kind == RunLifecycleEventKind::Awaiting
        && (targets.contains_key(&event.cursor)
            || event.await_reason.as_ref() == Some(&AwaitReason::BudgetReached))
    {
        return Ok(());
    }
    Err(ProjectionError::recovery_required(
        "Runtime recovery exposed an Awaiting Run without its answerable ticket; interrupt the Session to settle it",
    ))
}

/// The committed state of every coordinated child prefix.
#[derive(Debug, Default)]
pub struct ChildThreadStates {
    /// The current Run state of each child with a committed Run. A child with
    /// no committed Run yet has no state to project.
    pub latest_run_states: HashMap<String, RunState>,
    pub dispositions: HashMap<String, ThreadDisposition>,
}

/// Read each child's current Run and disposition from its committed prefix.
/// A damaged prefix is surfaced instead of silently dropping the child's state.
pub fn child_thread_states(
    children: &ProjectionThreadSnapshots,
) -> Result<ChildThreadStates, ProjectionError> {
    let mut latest_run_states = HashMap::new();
    for (thread_id, snapshot) in children {
        if let Some(run) = current_recovery_run(snapshot)? {
            latest_run_states.insert(thread_id.clone(), run.state.clone());
        }
    }
    let dispositions = children
        .iter()
        .map(|(thread_id, snapshot)| {
            awaken_agent_contract::thread_disposition_from_committed_state(&snapshot.state)
                .map(|disposition| (thread_id.clone(), disposition))
                .map_err(|error| {
                    ProjectionError::corrupt(format!(
                        "recover coordinated Thread disposition: {error}"
                    ))
                })
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(ChildThreadStates {
        latest_run_states,
        dispositions,
    })
}

/// The accepted lifecycle prefix of one candidate and the answerable ticket
/// of each accepted pause, read from that pause's own committed audit.
#[derive(Debug, Default)]
pub struct AcceptedLifecycle {
    pub acceptance: LifecycleAcceptance,
    pub historical_pending: HashMap<RunLifecycleCursor, Pending>,
}

impl AcceptedLifecycle {
    /// A Thread's current answerable call: its latest accepted lifecycle
    /// commit, when that commit is an audited pause.
    #[must_use]
    pub fn current_pending<'a>(
        &self,
        thread_ids: impl IntoIterator<Item = &'a String>,
    ) -> HashMap<String, Pending> {
        let mut latest_by_thread = HashMap::<&str, &RunLifecycleEvent>::new();
        for event in &self.acceptance.accepted {
            latest_by_thread.insert(event.thread_id.0.as_str(), event);
        }
        thread_ids
            .into_iter()
            .filter_map(|thread_id| {
                latest_by_thread
                    .get(thread_id.as_str())
                    .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
                    .and_then(|event| self.historical_pending.get(&event.cursor))
                    .cloned()
                    .map(|pending| (thread_id.clone(), pending))
            })
            .collect()
    }
}

/// Accept the merged committed lifecycle of the root and its coordinated
/// children.
///
/// The target of every Awaiting commit is read from that commit's own audit
/// row. A live ticket is never an input: the audit is committed with the
/// ticket and outlives it, so a pause classifies the same before and after
/// its ticket is consumed. A pause whose target no client answers (a
/// scheduled action, a delegation) is still classified by it; only a pause
/// that left no target blocks. Health and fact admission consume the same
/// exact-cursor audit map, so a later repeated Awaiting audit cannot
/// authorize an older wait.
pub fn accept_committed_lifecycle(
    session_id: &str,
    root: Option<&RunRecoverySnapshot>,
    children: &ProjectionThreadSnapshots,
    links: &[CoordinatedThreadLink],
    lifecycle: Vec<RunLifecycleEvent>,
) -> Result<AcceptedLifecycle, ProjectionError> {
    let known_children = links
        .iter()
        .map(|link| link.thread_id.0.as_str())
        .collect::<HashSet<_>>();
    let targets = audited_await_targets(&lifecycle, |thread_id| {
        if thread_id == session_id {
            root
        } else {
            children.get(thread_id)
        }
    })?;
    for snapshot in root.into_iter().chain(children.values()) {
        committed_pause_health(snapshot, &lifecycle, &targets)?;
    }
    let audited = targets
        .iter()
        .filter_map(|(cursor, target)| Some((*cursor, Pending::from_await_target(target)?)))
        .collect::<HashMap<_, _>>();
    let acceptance = accept_lifecycle(
        lifecycle,
        &AcceptanceFence {
            session_id,
            root,
            children,
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
    Ok(AcceptedLifecycle {
        acceptance,
        historical_pending,
    })
}
