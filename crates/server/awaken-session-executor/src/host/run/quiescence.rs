//! Terminal delegation quiescence for this executor's Sessions: the root's
//! resident attempt, when one is installed, is the local accelerator of
//! `awaken_session_run_delivery::CoordinatedThreads::quiesce_terminal_delegations`.

use super::*;

/// One installed Session's resident context as the root attempt terminal
/// quiescence signals.
struct ResidentContext(Arc<SessionCtx>);

impl awaken_session_run_delivery::ResidentAttempt for ResidentContext {
    fn live_control(
        &self,
    ) -> &(dyn awaken_runtime_contract::control::LiveRunControl + Send + Sync) {
        self.0.live_control()
    }

    fn cancel_foreground(&self) -> Result<(), awaken_session_types::RunError> {
        let foreground = self
            .0
            .foreground
            .observe()
            .map_err(|error| crate::session_execution_error::to_run_error(error.into()))?;
        if let Some(token) = foreground.cancellation {
            token.cancel();
        }
        Ok(())
    }

    fn has_foreground_run(&self) -> Result<bool, awaken_session_types::RunError> {
        self.0
            .foreground
            .has_run()
            .map_err(|error| crate::session_execution_error::to_run_error(error.into()))
    }
}

impl crate::SessionRuns {
    pub async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, SessionExecutionError>
    {
        let resident = self
            .installed_sessions
            .read_run(thread, |run| run.context.clone())?
            .flatten()
            .map(ResidentContext);
        Ok(self
            .threads
            .quiesce_terminal_delegations(
                thread,
                match &resident {
                    Some(resident) => awaken_session_run_delivery::RootAttempt::Resident(resident),
                    None => awaken_session_run_delivery::RootAttempt::Elsewhere,
                },
            )
            .await?)
    }
}

#[cfg(test)]
use awaken_dispatch_contract::DispatchControlReads as _;

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
                            awaken_worker_contract::PlacementRequirements::remote_required(),
                        )
                        .with_worker_codec_requirements(),
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
            let claim = awaken_worker_contract::RunClaim::from(&claimed.lease);
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
