//! Terminal delegation quiescence over durable dispatch authority.

use super::*;
use awaken_dispatch_contract::ControlDispatchQueue;

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

impl SharedHost {
    pub async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, HostError> {
        // Terminal control is deliberately Environment-free. A cold projection
        // must never materialize the sandbox, MCP connections, or current Agent
        // configuration merely to tear the Session down.
        let resident = self
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten();
        let mut coordinated_thread_ids = std::collections::HashSet::new();
        if let Ok(store) = self.dispatch_store() {
            let thread_id = ThreadId(thread.to_string());
            // Cause/effect decision table: P1 root dispatch and P2 every row
            // whose trusted parent affinity names this Session are the complete
            // execution set; C1 before the resident-run fence and C2 after it
            // close the last-admission race. Each pass records logical child ids
            // before cancellation; no process-local child registry participates.
            for pass in 0..2 {
                let dispatches = store
                    .list_dispatches()
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?;
                for dispatch in dispatches.iter().filter(|dispatch| {
                    dispatch.thread_id == thread_id
                        || dispatch.session_thread_id.as_ref() == Some(&thread_id)
                }) {
                    if dispatch.session_thread_id.as_ref() == Some(&thread_id)
                        && dispatch.thread_id != thread_id
                    {
                        coordinated_thread_ids.insert(dispatch.thread_id.clone());
                    }
                }
                for dispatch in dispatches.into_iter().filter(|dispatch| {
                    dispatch.thread_id == thread_id
                        || dispatch.session_thread_id.as_ref() == Some(&thread_id)
                }) {
                    let action = terminal_dispatch_action(dispatch.state);
                    if action == TerminalDispatchAction::Ignore {
                        continue;
                    }
                    // The root's resident attempt receives the same post-intent
                    // accelerator as an explicit interrupt. Child/cold/remote
                    // attempts retain the durable claim path without a second
                    // process-local registry.
                    let live_runtime = resident
                        .as_ref()
                        .filter(|_| dispatch.thread_id == thread_id)
                        .map(|ctx| ctx.runtime.as_ref());
                    self.persist_dispatch_cancellation(&dispatch.run_id, live_runtime)
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
                            .map_err(|error| HostError::internal(error.to_string()))?;
                    }
                }

                // Direct ACP and Outcome attempts may own only the foreground
                // token. Any durable rows have crossed the intent boundary above,
                // so this legacy-compatible nudge cannot precede durable truth.
                if pass == 0
                    && let Some(ctx) = &resident
                    && let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref()
                {
                    token.cancel();
                }
                if pass == 0
                    && let Some(ctx) = &resident
                {
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if ctx
                            .active_run
                            .lock()
                            .expect("active run mutex poisoned")
                            .is_none()
                        {
                            break;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            return Err(HostError::internal(format!(
                                "terminal quiescence timed out for Thread `{thread}`"
                            )));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
                // Cancellation is complete only after every local or remote
                // Worker settles all root/parent-affined runnable rows. A local
                // root join does not cover recovered child Workers.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let still_active = store
                        .list_dispatches()
                        .await
                        .map_err(|error| HostError::internal(error.to_string()))?
                        .into_iter()
                        .any(|dispatch| {
                            (dispatch.thread_id == thread_id
                                || dispatch.session_thread_id.as_ref() == Some(&thread_id))
                                && matches!(
                                    dispatch.state,
                                    awaken_dispatch_contract::DispatchState::Reserved
                                        | awaken_dispatch_contract::DispatchState::ReservationLeased
                                        | awaken_dispatch_contract::DispatchState::Pending
                                        | awaken_dispatch_contract::DispatchState::Leased
                                        | awaken_dispatch_contract::DispatchState::Awaiting
                                )
                        });
                    if !still_active {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(HostError::internal(format!(
                            "terminal dispatch quiescence timed out for Session `{thread}`"
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        } else if let Some(ctx) = resident {
            // A direct/non-dispatch attempt has no durable cancellation intent
            // to order before this legacy foreground signal.
            if let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref() {
                token.cancel();
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if ctx
                    .active_run
                    .lock()
                    .expect("active run mutex poisoned")
                    .is_none()
                {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(HostError::internal(format!(
                        "terminal quiescence timed out for Thread `{thread}`"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        // Rebuild committed links only after the dispatch fence. Cold or
        // malformed projection data may make enrichment fail, but it can never
        // prevent cancellation of already-admitted Session children.
        coordinated_thread_ids.extend(
            self.coordinated_threads(thread, None)
                .await?
                .into_iter()
                .map(|link| link.thread_id),
        );
        let mut snapshot = self.delegated_run_snapshot(thread).await?;
        snapshot.coordinated_thread_ids = coordinated_thread_ids.into_iter().collect();
        snapshot
            .coordinated_thread_ids
            .sort_by(|left, right| left.0.cmp(&right.0));
        Ok(snapshot)
    }
}

#[cfg(kani)]
#[kani::proof]
fn terminal_quiescence_rejects_only_unstarted_reservations() {
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
