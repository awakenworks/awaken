// @formal entrypoint session_terminal.unstarted_reservation_is_rejected_after_cancel
//! Terminal delegation quiescence over durable dispatch authority.

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::{
    DispatchControlReads as _, DispatchReads as _, SessionRunReservations as _,
};
use awaken_dispatch_execution::durable_control::RunResidence;
use awaken_runtime_contract::control::LiveRunControl;
use awaken_runtime_contract::runs::RunError;

use super::CoordinatedThreads;

/// Where a terminal Session's root attempt executes, as seen by the process
/// that quiesces it.
///
/// Decision table of one dispatch's cancellation residence (test
/// `root_attempt_reaches_only_its_own_threads_dispatches`):
/// | Rule | root attempt | dispatch on the root's Thread | Effect |
/// |---|---|---|---|
/// | Q1 | `Resident` | yes | `RunResidence::Resident(live control)` |
/// | Q2 | `Resident` | no (a coordinated child) | `RunResidence::Elsewhere` |
/// | Q3 | `Elsewhere` | any | `RunResidence::Elsewhere` |
#[derive(Clone, Copy)]
pub enum RootAttempt<'a> {
    /// Resident in this process: after each root dispatch's durable intent its
    /// live control receives the cancellation, and the first pass cancels its
    /// foreground token and waits for that attempt to leave.
    Resident(&'a dyn ResidentAttempt),
    /// Cold, remote, or in a process that executes no attempt (a Coordinator):
    /// durable cancellation is the whole quiescence.
    Elsewhere,
}

impl<'a> RootAttempt<'a> {
    /// The cancellation residence of one dispatch: only a dispatch on the
    /// root's own Thread reaches the resident attempt; a child, cold or remote
    /// attempt keeps the durable claim path alone.
    fn residence_of(self, root_thread: bool) -> RunResidence<'a> {
        match self {
            Self::Resident(attempt) if root_thread => {
                RunResidence::Resident(attempt.live_control())
            }
            Self::Resident(_) | Self::Elsewhere => RunResidence::Elsewhere,
        }
    }
}

/// The Session's root attempt resident in this process: the local
/// accelerator terminal quiescence signals after the durable intent.
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
    /// cleanup covers. `root` says whether the root's attempt is resident here.
    pub async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
        root: RootAttempt<'_>,
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
                    let residence = root.residence_of(dispatch.thread_id == thread_id);
                    let control = self.durable_run_control()?;
                    control
                        .persist_cancellation(&dispatch.run_id, residence)
                        .await?;
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
                && let RootAttempt::Resident(resident) = root
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::control::{Error as LiveControlError, LiveCommand};

    struct IdleResident;

    impl LiveRunControl for IdleResident {
        fn deliver(&self, _command: LiveCommand) -> Result<(), LiveControlError> {
            Ok(())
        }
    }

    impl ResidentAttempt for IdleResident {
        fn live_control(&self) -> &(dyn LiveRunControl + Send + Sync) {
            self
        }

        fn cancel_foreground(&self) -> Result<(), RunError> {
            Ok(())
        }

        fn has_foreground_run(&self) -> Result<bool, RunError> {
            Ok(false)
        }
    }

    #[test]
    fn root_attempt_reaches_only_its_own_threads_dispatches() {
        let resident = IdleResident;
        let root = RootAttempt::Resident(&resident);
        assert!(
            matches!(root.residence_of(true), RunResidence::Resident(_)),
            "Q1"
        );
        assert!(
            matches!(root.residence_of(false), RunResidence::Elsewhere),
            "Q2"
        );
        for root_thread in [true, false] {
            assert!(
                matches!(
                    RootAttempt::Elsewhere.residence_of(root_thread),
                    RunResidence::Elsewhere
                ),
                "Q3"
            );
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;
    use awaken_dispatch_contract::DispatchState;

    const EVERY_STATE: [DispatchState; 7] = [
        DispatchState::Reserved,
        DispatchState::ReservationLeased,
        DispatchState::Pending,
        DispatchState::Leased,
        DispatchState::Awaiting,
        DispatchState::DeadLetter,
        DispatchState::Superseded,
    ];

    const fn listed_state(state: DispatchState) -> usize {
        match state {
            DispatchState::Reserved => 0,
            DispatchState::ReservationLeased => 1,
            DispatchState::Pending => 2,
            DispatchState::Leased => 3,
            DispatchState::Awaiting => 4,
            DispatchState::DeadLetter => 5,
            DispatchState::Superseded => 6,
        }
    }

    #[test]
    fn every_dispatch_state_is_listed() {
        for (index, state) in EVERY_STATE.into_iter().enumerate() {
            assert_eq!(listed_state(state), index);
        }
    }

    /// @exhaustive DispatchState(7) = 7
    /// @obligation session_terminal.unstarted_reservation_is_rejected_after_cancel
    #[test]
    fn terminal_quiescence_rejects_only_unstarted_reservations() {
        // E1 Reserved/ReservationLeased is canceled and guarded-rejected because
        // no executor was admitted; E2 Pending/Leased/Awaiting/DeadLetter follows
        // the ordinary cancellation/settlement authority; E3 Superseded is
        // absorbing and ignored.
        for state in EVERY_STATE {
            let action = terminal_dispatch_action(state);
            assert_eq!(
                action == TerminalDispatchAction::CancelAndRejectReservation,
                matches!(
                    state,
                    DispatchState::Reserved | DispatchState::ReservationLeased
                )
            );
            assert_eq!(
                action == TerminalDispatchAction::Ignore,
                state == DispatchState::Superseded
            );
        }
    }
}
