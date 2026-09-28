// @formal entrypoint session_terminal.unstarted_reservation_is_rejected_after_cancel
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

fn expired_claim_for_fast_pending(
    claim: awaken_dispatch::RunClaim,
    was_current: Option<bool>,
) -> Option<awaken_dispatch::RunClaim> {
    (was_current == Some(false)).then_some(claim)
}

fn exact_expired_slot_remains(
    expected: &awaken_dispatch::RunClaim,
    observed: Option<&awaken_dispatch::PhysicalAttempt>,
) -> bool {
    observed.is_some_and(|physical| physical.claim == *expected)
}

impl SharedHost {
    pub async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, HostError> {
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
                let mut expired_physical = Vec::new();
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
                    // Capture expiry BEFORE cancellation revokes even a live
                    // Claim. The frozen physical slot is checked again after
                    // cancellation; this observation never clears it.
                    if dispatch.physical_attempt_active {
                        let expired_claim = match store
                            .load_physical_attempt(&dispatch.run_id)
                            .await
                        {
                            Ok(Some(physical)) => {
                                let was_current = match store
                                    .claim_is_current(&physical.claim)
                                    .await
                                {
                                    Ok(current) => Some(current),
                                    Err(error) => {
                                        tracing::warn!(run_id = %dispatch.run_id.0, %error,
                                            "terminal quiescence expiry probe unavailable; retaining ordinary wait");
                                        None
                                    }
                                };
                                expired_claim_for_fast_pending(physical.claim, was_current)
                            }
                            Ok(None) => None,
                            Err(error) => {
                                tracing::warn!(run_id = %dispatch.run_id.0, %error,
                                    "terminal quiescence physical probe unavailable; retaining ordinary wait");
                                None
                            }
                        };
                        if let Some(claim) = expired_claim {
                            expired_physical.push((dispatch.run_id.clone(), claim));
                        }
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
                    let deadline = awaken_lease_contract::mono::MonoDeadline::after(
                        std::time::Duration::from_secs(10),
                    );
                    loop {
                        if ctx
                            .active_run
                            .lock()
                            .expect("active run mutex poisoned")
                            .is_none()
                        {
                            break;
                        }
                        if deadline.has_passed() {
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
                let deadline = awaken_lease_contract::mono::MonoDeadline::after(
                    std::time::Duration::from_secs(10),
                );
                let mut inspect_expired_physical = !expired_physical.is_empty();
                loop {
                    let dispatches = store
                        .list_dispatches()
                        .await
                        .map_err(|error| HostError::internal(error.to_string()))?;
                    let active_here = |dispatch: &awaken_dispatch::DispatchSummary| {
                        (dispatch.thread_id == thread_id
                            || dispatch.session_thread_id.as_ref() == Some(&thread_id))
                            && dispatch_remains_unsettled(dispatch.state)
                    };
                    if !dispatches.iter().any(&active_here) {
                        break;
                    }
                    if inspect_expired_physical {
                        for (run_id, claim) in &expired_physical {
                            if dispatches.iter().any(|dispatch| {
                                active_here(dispatch)
                                    && dispatch.run_id == *run_id
                                    && dispatch.physical_attempt_active
                            }) {
                                match store.load_physical_attempt(run_id).await {
                                    Ok(Some(physical))
                                        if exact_expired_slot_remains(claim, Some(&physical)) =>
                                    {
                                        return Err(HostError::internal(format!(
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
            let deadline = awaken_lease_contract::mono::MonoDeadline::after(
                std::time::Duration::from_secs(10),
            );
            loop {
                if ctx
                    .active_run
                    .lock()
                    .expect("active run mutex poisoned")
                    .is_none()
                {
                    break;
                }
                if deadline.has_passed() {
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
/// @obligation session_terminal.unstarted_reservation_is_rejected_after_cancel
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

#[cfg(test)]
mod fast_pending_tests {
    use super::*;

    #[test]
    fn only_a_pre_cancel_expired_and_still_exact_slot_can_skip_the_normal_wait() {
        // C1 the pre-cancel Claim is current/expired/unavailable; C2 the
        // post-cancel slot is exact/missing/replaced or its read is unavailable.
        // E1 only expired+exact permits prompt pending; E2 all other cases
        // retain the ordinary bounded wait. Rules P1=(expired,exact)=>E1;
        // P2=(current|unknown,any)|(expired,missing|replaced|unknown)=>E2.
        // Probe failure is an optimization miss, never a reason to skip
        // durable cancellation or to accept physical quiescence.
        let claim = awaken_dispatch::RunClaim {
            run_id: RunId("fast-pending-run".into()),
            owner: "worker-a".into(),
            epoch: 1,
        };
        for (current, expected) in [(Some(false), true), (Some(true), false), (None, false)] {
            assert_eq!(
                expired_claim_for_fast_pending(claim.clone(), current).is_some(),
                expected,
                "P1/P2 pre-cancel {current:?}"
            );
        }
        let exact = awaken_dispatch::PhysicalAttempt {
            closing: false,
            claim: claim.clone(),
            binding: None,
            thread_id: None,
            session_thread_id: None,
        };
        let replacement = awaken_dispatch::PhysicalAttempt {
            claim: awaken_dispatch::RunClaim {
                epoch: 2,
                ..claim.clone()
            },
            ..exact.clone()
        };
        assert!(exact_expired_slot_remains(&claim, Some(&exact)), "P1/E1");
        assert!(!exact_expired_slot_remains(&claim, None), "P2/E2");
        assert!(
            !exact_expired_slot_remains(&claim, Some(&replacement)),
            "P2/E2"
        );
    }
}
