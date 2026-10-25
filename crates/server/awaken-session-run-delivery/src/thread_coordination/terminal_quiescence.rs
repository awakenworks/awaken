// @formal entrypoint session_terminal.unstarted_reservation_is_rejected_after_cancel
//! Terminal delegation quiescence over durable dispatch authority.

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::{
    DispatchControlReads as _, DispatchReads as _, SessionRunReservations as _,
};
use awaken_runtime_contract::control::LiveRunControl;
use awaken_session_types::RunError;

use super::CoordinatedThreads;

/// The Session's root attempt resident in this process, when one is: the
/// local accelerator terminal quiescence signals after the durable intent.
pub trait ResidentAttempt: Send + Sync {
    /// The resident Runtime's live control of the root's attempts.
    fn live_control(&self) -> &(dyn LiveRunControl + Send + Sync);
    /// Cancel the resident foreground attempt, if one holds a token.
    fn cancel_foreground(&self) -> Result<(), RunError>;
    /// Whether a foreground attempt is still admitted.
    fn has_foreground_run(&self) -> Result<bool, RunError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalDispatchAction {
    Ignore,
    Cancel,
    CancelAndRejectReservation,
}

/// Terminal Session truth makes an unstarted reservation permanently
/// unexecutable. Reuse the reservation owner's guarded rejection after durable
/// cancellation instead of waiting for its ordinary admission-recovery TTL.
/// Every already-activated row still follows the canonical cancellation claim
/// and physical-quiescence path.
const fn terminal_dispatch_action(
    state: awaken_dispatch_contract::DispatchState,
) -> TerminalDispatchAction {
    match state {
        awaken_dispatch_contract::DispatchState::Reserved
        | awaken_dispatch_contract::DispatchState::ReservationLeased => {
            TerminalDispatchAction::CancelAndRejectReservation
        }
        awaken_dispatch_contract::DispatchState::Pending
        | awaken_dispatch_contract::DispatchState::Leased
        | awaken_dispatch_contract::DispatchState::Awaiting
        | awaken_dispatch_contract::DispatchState::DeadLetter => TerminalDispatchAction::Cancel,
        awaken_dispatch_contract::DispatchState::Superseded => TerminalDispatchAction::Ignore,
    }
}

impl CoordinatedThreads {
    /// Quiesce every dispatch of terminal Session `thread` and its coordinated
    /// children, then read the delegated Runs and coordinated Threads its
    /// cleanup covers. `resident` is the root's attempt in this process, if any.
    pub async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
        resident: Option<&dyn ResidentAttempt>,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        // Terminal control is deliberately Environment-free. A cold projection
        // must never materialize the sandbox, MCP connections, or current Agent
        // configuration merely to tear the Session down.
        let mut coordinated_thread_ids = std::collections::HashSet::new();
        let store = self.owner().ok();
        let thread_id = ThreadId(thread.to_string());
        // The two passes bracket resident quiescence: each captures every
        // parent-affined child before canceling it. Durable cancellation must
        // precede the shared local accelerator, including the direct-run case.
        for pass in 0..2 {
            let mut expired_physical = Vec::new();
            if let Some(store) = &store {
                let dispatches = store
                    .list_dispatches()
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?;
                for dispatch in dispatches.into_iter().filter(|dispatch| {
                    dispatch.thread_id == thread_id
                        || dispatch.session_thread_id.as_ref() == Some(&thread_id)
                }) {
                    if dispatch.thread_id != thread_id {
                        coordinated_thread_ids.insert(dispatch.thread_id.clone());
                    }
                    let action = terminal_dispatch_action(dispatch.state);
                    if action == TerminalDispatchAction::Ignore {
                        continue;
                    }
                    // Capture expiry BEFORE cancellation revokes even a live
                    // Claim. The frozen physical slot is checked again after
                    // cancellation; this observation never clears it.
                    if dispatch.physical_attempt_active {
                        match awaken_dispatch_execution::read_expired_physical_attempt(
                            store.as_ref(),
                            &dispatch.run_id,
                        )
                        .await
                        {
                            Ok(Some(claim)) => {
                                expired_physical.push((dispatch.run_id.clone(), claim))
                            }
                            Ok(None) => {}
                            Err(error) => tracing::warn!(run_id = %dispatch.run_id.0, %error,
                                "terminal quiescence expiry probe unavailable; retaining ordinary wait"),
                        }
                    }
                    // The root's resident attempt receives the same post-intent
                    // accelerator as an explicit interrupt. Child/cold/remote
                    // attempts retain the durable claim path without a second
                    // process-local registry.
                    let live = resident
                        .filter(|_| dispatch.thread_id == thread_id)
                        .map(ResidentAttempt::live_control);
                    let control = self.durable_run_control()?;
                    control.persist_cancellation(&dispatch.run_id, live).await?;
                    if action == TerminalDispatchAction::CancelAndRejectReservation {
                        // The surrounding Session terminal transition is the
                        // absorbing authority. A still-Reserved row has no Run
                        // truth or physical effect to settle, so the canonical
                        // reservation rejection removes it. If activation won
                        // this race, guarded rejection returns false and the
                        // next cancellation pass handles the resulting Pending
                        // row through its ordinary Worker path.
                        store
                            .reject_session_run_reservation(&dispatch.run_id)
                            .await
                            .map_err(|error| RunError::internal(error.to_string()))?;
                    }
                }
            }
            // Direct ACP and Outcome attempts may own only the foreground
            // token. Any durable rows have crossed the intent boundary above,
            // so this legacy-compatible nudge cannot precede durable truth.
            if pass == 0
                && let Some(resident) = resident
            {
                resident.cancel_foreground()?;
                let deadline =
                    awaken_time::mono::MonoDeadline::after(std::time::Duration::from_secs(10));
                while resident.has_foreground_run()? {
                    if deadline.has_passed() {
                        return Err(RunError::internal(format!(
                            "terminal quiescence timed out for Thread `{thread}`"
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            if let Some(store) = &store {
                // Scope discovery needs an absorbing logical write boundary,
                // not a physical-stop receipt. An exact committed Ended Run
                // cannot expand its Thread prefix, even when Dispatch still
                // retains its old executor installed Session. Live or missing Run truth
                // keeps the ordinary bounded wait below. Source I/O separately
                // observes those same retained installed Sessions before preparation.
                let deadline =
                    awaken_time::mono::MonoDeadline::after(std::time::Duration::from_secs(10));
                let mut inspect_expired_physical = !expired_physical.is_empty();
                loop {
                    let dispatches = store
                        .list_dispatches()
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?;
                    let active_here = |dispatch: &awaken_dispatch_contract::DispatchSummary| {
                        (dispatch.thread_id == thread_id
                            || dispatch.session_thread_id.as_ref() == Some(&thread_id))
                            && dispatch.state.remains_unsettled()
                    };
                    let mut logically_unsettled = Vec::new();
                    for dispatch in dispatches.iter().filter(|row| active_here(row)) {
                        let committed = self
                            .reads
                            .commit_for_read(&dispatch.thread_id.0)
                            .await?
                            .authoritative_run(&dispatch.run_id)
                            .await
                            .map_err(RunError::unavailable)?;
                        if let Some(run) = &committed
                            && (run.id != dispatch.run_id || run.thread_id != dispatch.thread_id)
                        {
                            return Err(RunError::internal(
                                "terminal dispatch Run belongs to another Thread",
                            ));
                        }
                        if !committed.is_some_and(|run| matches!(run.state, RunState::Ended(_))) {
                            logically_unsettled.push(dispatch);
                        }
                    }
                    if logically_unsettled.is_empty() {
                        break;
                    }
                    if inspect_expired_physical {
                        for (run_id, claim) in &expired_physical {
                            if logically_unsettled.iter().any(|dispatch| {
                                dispatch.run_id == *run_id && dispatch.physical_attempt_active
                            }) {
                                match store.load_physical_attempt(run_id).await {
                                    Ok(Some(physical))
                                        if awaken_dispatch_execution::exact_expired_slot_remains(claim, Some(&physical)) =>
                                    {
                                        return Err(RunError::internal(format!(
                                            "terminal dispatch quiescence awaits exact retirement of an expired physical attempt for Session `{thread}`"
                                        )));
                                    }
                                    Err(error) => {
                                        tracing::warn!(run_id = %run_id.0, %error,
                                            "terminal quiescence physical recheck unavailable; retaining ordinary wait");
                                    }
                                    _ => {}
                                }
                            }
                        }
                        inspect_expired_physical = false;
                    }
                    if deadline.has_passed() {
                        return Err(RunError::internal(format!(
                            "terminal dispatch quiescence timed out for Session `{thread}`"
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
        // Rebuild committed links only after the dispatch fence. Cold or
        // malformed projection data may make enrichment fail, but it can never
        // prevent cancellation of already-admitted Session children. One
        // atomic root prefix supplies both relationship and pending-call targets;
        // a lost child-admission response cannot make that child disappear.
        let (mut snapshot, root) = self.reads.delegated_runs_and_root(thread).await?;
        if let Some(root) = root {
            coordinated_thread_ids.extend(
                crate::project_committed_coordination(
                    &awaken_session_types::SessionId::from(thread),
                    None,
                    &root,
                    crate::CoordinationReadPurpose::TerminalScope,
                )?
                .cleanup_thread_ids,
            );
        }
        snapshot.coordinated_thread_ids = coordinated_thread_ids.into_iter().collect();
        snapshot
            .coordinated_thread_ids
            .sort_by(|left, right| left.0.cmp(&right.0));
        Ok(snapshot)
    }
}

#[cfg(kani)]
/// @obligation session_terminal.unstarted_reservation_is_rejected_after_cancel
#[kani::proof]
fn terminal_quiescence_rejects_only_unstarted_reservations() {
    // The reused Runtime physical-marker primitive only replaces the two
    // existing is_none probes. Cancellation intent still precedes local signal;
    // pass/deadline/retry and persisted physical-slot decisions stay below.
    // A local inactive marker cannot prove a remote Worker or request is closed.
    // Cause/effect decision table: C1 is the closed durable Dispatch phase.
    // E1 Reserved/ReservationLeased is canceled and guarded-rejected because no
    // executor was admitted; E2 Pending/Leased/Awaiting/DeadLetter follows the
    // ordinary cancellation/settlement authority; E3 Superseded is absorbing
    // and ignored. The seven-state partition is exhaustive, so terminal cleanup
    // can neither delete activated work as a reservation nor wait on the normal
    // reservation TTL after Session terminal truth is already durable.
    let state = match kani::any::<u8>() % 7 {
        0 => awaken_dispatch_contract::DispatchState::Reserved,
        1 => awaken_dispatch_contract::DispatchState::ReservationLeased,
        2 => awaken_dispatch_contract::DispatchState::Pending,
        3 => awaken_dispatch_contract::DispatchState::Leased,
        4 => awaken_dispatch_contract::DispatchState::Awaiting,
        5 => awaken_dispatch_contract::DispatchState::DeadLetter,
        _ => awaken_dispatch_contract::DispatchState::Superseded,
    };
    let action = terminal_dispatch_action(state);
    assert_eq!(
        action == TerminalDispatchAction::CancelAndRejectReservation,
        matches!(
            state,
            awaken_dispatch_contract::DispatchState::Reserved
                | awaken_dispatch_contract::DispatchState::ReservationLeased
        )
    );
    assert_eq!(
        action == TerminalDispatchAction::Ignore,
        state == awaken_dispatch_contract::DispatchState::Superseded
    );
}
