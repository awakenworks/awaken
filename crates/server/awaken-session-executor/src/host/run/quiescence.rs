// @formal entrypoint session_terminal.unstarted_reservation_is_rejected_after_cancel
//! Terminal delegation quiescence over durable dispatch authority.

use super::*;

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

impl crate::SessionRuns {
    pub async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, SessionExecutionError>
    {
        // Terminal control is deliberately Environment-free. A cold projection
        // must never materialize the sandbox, MCP connections, or current Agent
        // configuration merely to tear the Session down.
        let resident = self
            .installed_sessions
            .read_run(thread, |run| run.context.clone())?
            .flatten();
        let mut coordinated_thread_ids = std::collections::HashSet::new();
        let store = self.coordinator_dispatch().ok();
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
                    .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
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
                        .as_ref()
                        .filter(|_| dispatch.thread_id == thread_id)
                        .map(|ctx| ctx.live_control());
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
                            .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
                    }
                }
            }
            // Direct ACP and Outcome attempts may own only the foreground
            // token. Any durable rows have crossed the intent boundary above,
            // so this legacy-compatible nudge cannot precede durable truth.
            if pass == 0
                && let Some(ctx) = &resident
            {
                if let Some(token) = ctx.foreground.observe()?.cancellation {
                    token.cancel();
                }
                let deadline =
                    awaken_time::mono::MonoDeadline::after(std::time::Duration::from_secs(10));
                while ctx.foreground.has_run()? {
                    if deadline.has_passed() {
                        return Err(SessionExecutionError::internal(format!(
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
                        .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
                    let active_here = |dispatch: &awaken_dispatch_contract::DispatchSummary| {
                        (dispatch.thread_id == thread_id
                            || dispatch.session_thread_id.as_ref() == Some(&thread_id))
                            && dispatch.state.remains_unsettled()
                    };
                    let mut logically_unsettled = Vec::new();
                    for dispatch in dispatches.iter().filter(|row| active_here(row)) {
                        let committed = self
                            .thread_reads
                            .commit_for_read(&dispatch.thread_id.0)
                            .await?
                            .authoritative_run(&dispatch.run_id)
                            .await
                            .map_err(SessionExecutionError::unavailable)?;
                        if let Some(run) = &committed
                            && (run.id != dispatch.run_id || run.thread_id != dispatch.thread_id)
                        {
                            return Err(SessionExecutionError::internal(
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
                                        return Err(SessionExecutionError::internal(format!(
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
                        return Err(SessionExecutionError::internal(format!(
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
        let (mut snapshot, root) = self.thread_reads.delegated_runs_and_root(thread).await?;
        if let Some(root) = root {
            coordinated_thread_ids.extend(
                crate::thread_reads::project_committed_coordination(
                    &awaken_session_types::SessionId::from(thread),
                    None,
                    &root,
                    crate::thread_reads::CoordinationReadPurpose::TerminalScope,
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

use awaken_dispatch_contract::{
    DispatchControlReads as _, DispatchReads as _, SessionRunReservations as _,
};

#[cfg(test)]
mod terminal_scope_tests {
    use super::*;
    use awaken_agent_contract::agent::run::EndCause;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_dispatch_contract::{
        DispatchClaims as _, DispatchEnqueue as _, DispatchExecution as _,
    };

    #[tokio::test]
    async fn committed_terminal_run_freezes_scope_without_retiring_unknown_physical_execution() {
        // Cause/effect graph: C1 the parent Run has an authoritative Ended fact;
        // C2 its original expired physical entry remains bound and unclosed;
        // C3 no coordination/delegation call exists in the committed prefix.
        // Rule L1=C1+C2+C3 -> freeze the root-only logical target prefix; keep
        // the exact physical entry and create no Environment. Ended is the
        // store's absorbing write fence, not proof that external effects ended.
        // Resource preparation/disposal and Work's business-effect decision
        // retain their independent prerequisites; this tests only scope discovery.
        // Negative controls L2/L3 keep the same expired installed Session with missing or
        // Running Run truth: neither cancellation nor expiry may freeze scope.
        use crate::host::worker_resolver::test_support::{adoption_host_over, test_activation};
        for disposition in [
            None,
            Some(RunDisposition::running(RunId("terminal-scope-run".into()))),
            Some(RunDisposition::ended(
                RunId("terminal-scope-run".into()),
                EndCause::Indeterminate,
            )),
            Some(RunDisposition::ended(
                RunId("terminal-scope-run".into()),
                EndCause::Cancelled,
            )),
        ] {
            let ended = disposition
                .as_ref()
                .is_some_and(|run| matches!(run, RunDisposition::Ended { .. }));
            let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
            let store = Arc::new(
                awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                    .await
                    .unwrap(),
            );
            let host = adoption_host_over(Arc::new(
                awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(
                    store.clone(),
                ),
            ))
            .await;
            let thread = ThreadId("terminal-scope-parent".into());
            let run = RunId("terminal-scope-run".into());
            store
                .enqueue(
                    // This row represents a previously accepted Session
                    // activity, not a fresh reservation awaiting its receipt.
                    RunDispatch::new(test_activation(&thread.0, &run.0))
                        .for_session(thread.clone())
                        .with_session_activity_epoch(1)
                        // This physical binding was admitted by a registered
                        // remote Worker using the current protocol, not v0.
                        .with_placement(
                            awaken_dispatch_contract::PlacementRequirements::remote_required(),
                        ),
                )
                .await
                .unwrap();
            let (directory, worker) =
                awaken_dispatch_testkit::worker_http::ready_worker("old-worker").await;
            let registered = directory.incarnation(&worker).await.unwrap().unwrap();
            let claimed = store
                .claim_run_compatible(&run, &registered.snapshot, 10)
                .await
                .unwrap()
                .unwrap();
            let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
            let binding = awaken_dispatch_contract::dispatch::AttemptExecutionBinding::new(
                worker,
                "fixture-provider",
                "original-unclassified-execution",
            )
            .unwrap();
            store.begin_attempt(&claim, Some(&binding)).await.unwrap();
            let original = store.load_physical_attempt(&run).await.unwrap().unwrap();
            if let Some(disposition) = disposition {
                let commit = host.thread_reads.commit_for_read(&thread.0).await.unwrap();
                commit
                    .commit(ThreadCommit::assemble(
                        thread.clone(),
                        disposition,
                        true,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                    ))
                    .await
                    .unwrap();
            }
            clock.set(200);
            let scope = host
                .session_runs
                .quiesce_terminal_delegations(&thread.0)
                .await;
            if ended {
                let scope = scope.unwrap();
                assert!(scope.delegated_runs.is_empty());
                assert!(scope.coordinated_thread_ids.is_empty());
                assert!(
                    scope.runtime_commit_cursor > 0,
                    "L1 uses the actual committed prefix"
                );
            } else {
                assert!(
                    scope
                        .unwrap_err()
                        .message
                        .contains("expired physical attempt"),
                    "L2/L3 missing/live Run truth stays pending"
                );
            }
            assert_eq!(
                store.load_physical_attempt(&run).await.unwrap().unwrap(),
                original,
                "L1 scope discovery cannot acknowledge or retire physical execution"
            );
            assert!(
                host.realization_executor
                    .environments()
                    .session_environment(&thread.0)
                    .await
                    .expect("installed Sessions")
                    .is_none()
            );
        }
    }
}
