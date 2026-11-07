//! T08-C02 Run half: the Run is already terminal when the Worker's Work
//! acknowledgement (the lease-fenced Work release of the settle bridge) loses
//! its response. The retry re-presents the same claim; it may only replay the
//! original Work identity, never re-executes, and settles the Run with its own
//! committed result, never one synthesised from the Work outcome.

use super::*;

/// Work authority whose first release commits and then loses its response.
/// Every presented lease is recorded so the retry identity is observable.
#[derive(Default)]
struct LostReleaseWork {
    acquisitions: AtomicUsize,
    released: Mutex<Vec<awaken_session_application_contract::SessionWorkGrant>>,
    retired: AtomicBool,
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeaseAuthority for LostReleaseWork {
    async fn acquire_session_work(
        &self,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
        _acquisition: awaken_session_application_contract::SessionWorkAcquisition,
    ) -> Result<
        awaken_session_application_contract::SessionWorkOwnership,
        awaken_runtime_contract::runs::RunError,
    > {
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
        if self.retired.load(Ordering::SeqCst) {
            return Ok(awaken_session_application_contract::SessionWorkOwnership::Unowned);
        }
        Ok(
            awaken_session_application_contract::SessionWorkOwnership::Leased(
                awaken_session_types::SessionWorkLease::from_persisted(
                    format!("work-{session_id}"),
                    "env",
                    session_id.clone(),
                    worker_owner,
                    7,
                    OBSERVED_WORK_EXPIRY_MS,
                )
                .map(awaken_session_application_contract::SessionWorkGrant::from_transfer)
                .expect("nonzero epoch"),
            ),
        )
    }

    async fn release_session_work(
        &self,
        lease: &awaken_session_application_contract::SessionWorkGrant,
    ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
        self.released.lock().unwrap().push(lease.clone());
        if self.retired.swap(true, Ordering::SeqCst) {
            return Ok(true);
        }
        Err(awaken_runtime_contract::runs::RunError::unavailable(
            "Work release committed but its response was lost",
        ))
    }

    async fn release_worker_session_work(
        &self,
        _worker_owner: &str,
    ) -> Result<usize, awaken_runtime_contract::runs::RunError> {
        Ok(0)
    }
}

/// Records the committed Run state each pre-settlement effect is handed.
/// `before_settle` is a retry-safe effect that runs on every settle attempt
/// (its error retains the dispatch row as retry evidence), so it is not the
/// publication point; it must only ever see the committed Run truth.
#[derive(Default)]
struct CommittedStateObserver {
    states: Mutex<Vec<RunState>>,
}

/// Records what a settlement actually publishes: the completion sink fires
/// only after the Dispatch CAS applied.
#[derive(Default)]
struct PublishedSettlements {
    settled: Mutex<Vec<(RunId, RunState)>>,
}

impl awaken_dispatch::CompletionSink for PublishedSettlements {
    fn settled(&self, run_id: &RunId, state: &RunState) {
        self.settled
            .lock()
            .unwrap()
            .push((run_id.clone(), state.clone()));
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for CommittedStateObserver {
    async fn before_settle(
        &self,
        _dispatch: &RunDispatch,
        _claim: &RunClaim,
        committed_state: &RunState,
        _cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        self.states.lock().unwrap().push(committed_state.clone());
        Ok(())
    }
}

fn router_over(
    store: Arc<MemoryDispatchStore>,
    recovery: Arc<StaticRecovery>,
    observer: Arc<CommittedStateObserver>,
    published: Arc<PublishedSettlements>,
    work: Arc<LostReleaseWork>,
) -> Router {
    dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::new(
        RegisteredDispatchDependencies {
            lease_policy: Arc::new(FixedWorkerLeasePolicy::new(30_000)),
            recovery,
            terminal_observer: observer,
            completion: published,
            session_work: work,
            coordination: Arc::new(RecordingSessionCoordination::default()),
            ..worker_dispatch_dependencies(
                store,
                ready_workers(&["terminal-worker"]),
                Arc::new(ManualClock::new(0)),
            )
        },
    )))
}

#[tokio::test]
async fn terminal_run_work_ack_loss_retries_only_the_original_identity() {
    // | Row | Run              | Work release            | effect                               |
    // | A1  | Ended(Cancelled) | commits, response lost  | 500, dispatch retained, 1 release, nothing published |
    // | A2  | same claim retry | Work now retired        | 200 Done, no second release, no claim |
    // | A3  | committed result | -                       | published exactly once as Cancelled; every pre-settle effect saw Cancelled |
    // NC A4: Running Run + a Work authority that would succeed -> 400, the
    // dispatch is kept and no Work is released: Work success is never turned
    // into a Run result.
    let (store, claim, session_id, thread_id) =
        claimed_terminal_dispatch("work-ack-loss", TerminalThread::Primary).await;
    let cancelled = RunState::Ended(EndCause::Cancelled);
    let recovery = terminal_recovery(
        &session_id,
        &thread_id,
        &claim.run_id,
        Ok(terminal_snapshot(
            &thread_id,
            &claim.run_id,
            cancelled.clone(),
        )),
    );
    let observer = Arc::new(CommittedStateObserver::default());
    let published = Arc::new(PublishedSettlements::default());
    let work = Arc::new(LostReleaseWork::default());
    let router = router_over(
        store.clone(),
        recovery.clone(),
        observer.clone(),
        published.clone(),
        work.clone(),
    );

    // A1
    assert_eq!(
        settle_terminal(&router, &claim).await.0,
        StatusCode::INTERNAL_SERVER_ERROR,
        "A1 lost Work response is not success"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap().len(),
        1,
        "A1 retained"
    );
    assert!(
        published.settled.lock().unwrap().is_empty(),
        "A1 nothing published"
    );
    assert_eq!(
        observer.states.lock().unwrap().as_slice(),
        std::slice::from_ref(&cancelled),
        "A1 the pre-settle effect saw only the committed Run truth"
    );

    // A2: the Worker retries the same claim.
    let (status, value) = settle_terminal(&router, &claim).await;
    assert_eq!(status, StatusCode::OK, "A2: {value}");
    assert_eq!(value["settled"], true, "A2");
    let released = work.released.lock().unwrap().clone();
    assert_eq!(released.len(), 1, "A2 one physical Work release");
    assert_eq!(
        (released[0].owner(), released[0].epoch().get()),
        (claim.owner.as_str(), 7),
        "A2 the original claim owner and Work epoch"
    );
    assert_eq!(
        work.acquisitions.load(Ordering::SeqCst),
        2,
        "A2 re-observed, not re-leased"
    );
    assert!(
        store.list_dispatches().await.unwrap().is_empty(),
        "A2 settled"
    );
    assert!(
        store
            .claim("other-worker", 30_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "A2 nothing becomes claimable: the Run is not re-executed"
    );
    assert_eq!(
        recovery.observed.lock().unwrap().len(),
        2,
        "A2 the committed Run was read twice and never re-driven"
    );

    // A3
    assert_eq!(
        published.settled.lock().unwrap().as_slice(),
        [(claim.run_id.clone(), cancelled.clone())],
        "A3 the Run publishes its own committed result exactly once"
    );
    assert_eq!(
        observer.states.lock().unwrap().as_slice(),
        [cancelled.clone(), cancelled],
        "A3 each settle attempt handed the observer the committed result, never a Work-derived one"
    );

    // A4 negative control.
    let (store, claim, session_id, thread_id) =
        claimed_terminal_dispatch("work-ack-running", TerminalThread::Primary).await;
    let recovery = terminal_recovery(
        &session_id,
        &thread_id,
        &claim.run_id,
        Ok(terminal_snapshot(
            &thread_id,
            &claim.run_id,
            RunState::Running,
        )),
    );
    let observer = Arc::new(CommittedStateObserver::default());
    let published = Arc::new(PublishedSettlements::default());
    let work = Arc::new(LostReleaseWork::default());
    work.retired.store(true, Ordering::SeqCst);
    let router = router_over(
        store.clone(),
        recovery,
        observer.clone(),
        published.clone(),
        work.clone(),
    );
    let (status, value) = settle_terminal(&router, &claim).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "A4: {value}");
    assert_eq!(
        store.list_dispatches().await.unwrap().len(),
        1,
        "A4 retained"
    );
    assert!(
        work.released.lock().unwrap().is_empty(),
        "A4 no Work release"
    );
    assert!(
        observer.states.lock().unwrap().is_empty(),
        "A4 no synthesised result"
    );
    assert!(
        published.settled.lock().unwrap().is_empty(),
        "A4 nothing published"
    );
}
