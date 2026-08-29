//! Terminal delegation quiescence over durable dispatch authority.

use super::*;

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
                    (dispatch.thread_id == thread_id
                        || dispatch.session_thread_id.as_ref() == Some(&thread_id))
                        && matches!(
                            dispatch.state,
                            awaken_run_ingress_contract::DispatchState::Reserved
                                | awaken_run_ingress_contract::DispatchState::ReservationLeased
                                | awaken_run_ingress_contract::DispatchState::Pending
                                | awaken_run_ingress_contract::DispatchState::Leased
                                | awaken_run_ingress_contract::DispatchState::Awaiting
                                | awaken_run_ingress_contract::DispatchState::DeadLetter
                        )
                }) {
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
                                    awaken_run_ingress_contract::DispatchState::Reserved
                                        | awaken_run_ingress_contract::DispatchState::ReservationLeased
                                        | awaken_run_ingress_contract::DispatchState::Pending
                                        | awaken_run_ingress_contract::DispatchState::Leased
                                        | awaken_run_ingress_contract::DispatchState::Awaiting
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
            self.coordinated_threads(thread)
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
