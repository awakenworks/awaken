//! Current run-free Root grants acquire Work through the lease role's
//! preparation claim, which records the hold a Run acquisition clears.
use super::*;
use awaken_session_application_contract::{SessionWorkOwnership, SessionWorkerLeases};
use awaken_session_types::SessionWorkLease;
use awaken_time::WallClockExt as _;
use tokio::sync::{Notify, Semaphore};

const WORK_TTL_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreparationWorkState {
    Missing,
    Queued,
    Stopped,
    Active(SessionWorkLease),
}

/// A lease-role fixture with a cut after its persisted effect and before ACK.
/// It records adapter calls, leaving actual Work backend qualification with
/// the existing PostgreSQL and registered-Worker HTTP owners.
struct PreparationWork {
    state: Mutex<PreparationWorkState>,
    clock: Arc<awaken_lease_testkit::ManualClock>,
    acquisitions: AtomicUsize,
    repairs: AtomicUsize,
    releases: AtomicUsize,
    pause: AtomicBool,
    reached: Notify,
    proceed: Semaphore,
}

impl PreparationWork {
    fn new(state: PreparationWorkState, pause: bool) -> Self {
        Self {
            state: Mutex::new(state),
            clock: Arc::new(awaken_lease_testkit::ManualClock::new(1_000)),
            acquisitions: AtomicUsize::new(0),
            repairs: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            pause: AtomicBool::new(pause),
            reached: Notify::new(),
            proceed: Semaphore::new(0),
        }
    }

    fn lease(&self) -> SessionWorkLease {
        let PreparationWorkState::Active(lease) = &*self.state.lock().unwrap() else {
            panic!("the original acquire role must have persisted its Work grant")
        };
        lease.clone()
    }

    async fn wait_until_acquired(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), self.reached.notified())
            .await
            .expect("the original lease role reaches its post-effect cut");
    }
}

#[async_trait::async_trait]
impl SessionWorkerLeases for PreparationWork {
    async fn acquire_session(
        &self,
        _: &str,
        _: &SessionId,
        _: &str,
    ) -> Result<Option<SessionWorkLease>, RunError> {
        panic!("a run-free preparation claims Work through the preparation role")
    }

    async fn acquire_preparation_session(
        &self,
        environment: &str,
        session: &SessionId,
        owner: &str,
    ) -> Result<Option<SessionWorkLease>, RunError> {
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
        let result = {
            let mut state = self.state.lock().unwrap();
            match &*state {
                PreparationWorkState::Stopped => None,
                PreparationWorkState::Active(held) if held.owner() != owner => Some(held.clone()),
                _ => {
                    let (work_id, epoch) = match &*state {
                        PreparationWorkState::Active(held) => {
                            (held.work_id().to_string(), held.epoch())
                        }
                        PreparationWorkState::Missing => {
                            self.repairs.fetch_add(1, Ordering::SeqCst);
                            (format!("work:{session}"), 1)
                        }
                        PreparationWorkState::Queued => (format!("work:{session}"), 1),
                        PreparationWorkState::Stopped => unreachable!(),
                    };
                    let lease = SessionWorkLease::from_persisted(
                        work_id,
                        environment,
                        session.clone(),
                        owner,
                        epoch,
                        self.clock.now_ms().saturating_add(WORK_TTL_MS),
                    )
                    .unwrap();
                    *state = PreparationWorkState::Active(lease.clone());
                    Some(lease)
                }
            }
        };
        if self.pause.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.proceed.acquire().await.unwrap().forget();
        }
        Ok(result)
    }

    async fn renew_session(
        &self,
        _: &str,
        _: &SessionId,
        _: &str,
    ) -> Result<Option<SessionWorkLease>, RunError> {
        panic!("run-free acquisition must call the original acquire role")
    }

    async fn release_session(&self, _: &SessionWorkLease) -> Result<bool, RunError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }

    async fn release_owner(&self, _: &str) -> Result<usize, RunError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }

    async fn release_preparation_session(
        &self,
        _: &str,
        _: &SessionId,
        _: &str,
    ) -> Result<bool, RunError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerAssignment for PreparationWork {
    async fn enqueue_session(&self, _: &str, _: &SessionId) -> Result<String, RunError> {
        panic!("preparation acquisition repairs projection through its original lease adapter")
    }
    async fn wake_session(&self, _: &str, _: &SessionId) -> Result<String, RunError> {
        panic!("a preparation grant cannot revive stopped Work")
    }
    async fn retire_session(&self, _: &str, _: &SessionId) -> Result<(), RunError> {
        panic!("an unissued preparation grant is not terminal cleanup")
    }
}

struct PreparationFixture {
    id: SessionId,
    repo: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    app: Arc<crate::SessionApplication>,
    observations: Arc<RecordingWorkerObservations>,
    clock: Arc<awaken_lease_testkit::ManualClock>,
    work: Arc<PreparationWork>,
    target: awaken_session_execution_contract::SessionRealizationTarget,
}

impl PreparationFixture {
    async fn new(id: &str, state: PreparationWorkState, pause: bool) -> Self {
        let id = SessionId::from(id);
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000));
        let mut root = idle_mcp_update(id.as_str(), true, true);
        root.realization.as_mut().unwrap().expires_at_unix_ms = 10_000;
        create(repo.as_ref(), root).await;
        let target = awaken_session_execution_contract::SessionRealizationTarget {
            owner: "worker".into(),
            runtime_incarnation: "worker:1:boot".into(),
            lease_expires_at_unix_ms: 10_000,
            reassign_existing_lease: false,
        };
        let observations = Arc::new(RecordingWorkerObservations::default());
        observations.replace(vec![terminal_cleanup_worker(
            &target,
            terminal_cleanup_manifest(),
        )]);
        let work = Arc::new(PreparationWork::new(state, pause));
        let probe = Arc::new(crate::ExecutionObservationProbe::default());
        let app = Arc::new(configured_application(
            Arc::new(NoopRuntime),
            SessionPersistence::new(repo.clone(), clock.clone()),
            crate::SessionEnvironments {
                projection: Arc::new(RecordingEnvironmentSource::default()),
                assignment: work.clone(),
                leases: work.clone(),
            },
            Default::default(),
            None,
            crate::SessionExecutionAdmission::new(
                probe,
                observations.clone(),
                Arc::new(FixtureSessionPlacement),
            ),
        ));
        Self {
            id,
            repo,
            app,
            observations,
            clock,
            work,
            target,
        }
    }
}

#[tokio::test]
async fn preparation_acquires_queued_or_missing_work_without_reviving_stopped_work() {
    for state in [
        PreparationWorkState::Queued,
        PreparationWorkState::Missing,
        PreparationWorkState::Stopped,
    ] {
        let fixture = PreparationFixture::new("preparation-work-state", state.clone(), false).await;
        let root = fixture.repo.get(&fixture.id).await.unwrap();
        let outcome = fixture
            .app
            .work_assignments
            .acquire_for_preparation(&fixture.id, root.realization.as_ref().unwrap())
            .await
            .unwrap();
        if state == PreparationWorkState::Stopped {
            assert!(matches!(outcome, SessionWorkOwnership::Unowned));
            assert_eq!(
                *fixture.work.state.lock().unwrap(),
                PreparationWorkState::Stopped
            );
        } else {
            let SessionWorkOwnership::Leased(grant) = outcome else {
                panic!("{state:?} must acquire its own Work")
            };
            let lease = grant.to_transfer();
            assert_eq!(lease.owner(), fixture.target.runtime_incarnation);
            assert_eq!(
                lease.expires_at_unix_ms(),
                fixture.work.clock.now_ms() + WORK_TTL_MS
            );
            assert_eq!(
                fixture.work.repairs.load(Ordering::SeqCst),
                usize::from(state == PreparationWorkState::Missing)
            );
        }
        assert_eq!(fixture.work.acquisitions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.work.releases.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.repo.get(&fixture.id).await.unwrap(), root);
    }
}

#[tokio::test]
async fn preparation_preserves_a_foreign_work_grant_and_closes_only_its_unissued_root_lease() {
    let id = "preparation-foreign-work";
    let foreign = SessionWorkLease::from_persisted(
        "work:foreign-held",
        "env-worker",
        SessionId::from(id),
        "foreign:1:boot",
        7,
        31_000,
    )
    .unwrap();
    let fixture =
        PreparationFixture::new(id, PreparationWorkState::Active(foreign.clone()), false).await;
    assert!(
        fixture
            .app
            .realizations
            .claim_next_session_preparation(fixture.target.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.work.lease(), foreign);
    assert_eq!(fixture.work.releases.load(Ordering::SeqCst), 0);
    let root = fixture.repo.get(&fixture.id).await.unwrap();
    assert_eq!(root.realization.as_ref().unwrap().expires_at_unix_ms, 0);
    assert_eq!(root.realization.as_ref().unwrap().epoch, 3);
}

#[tokio::test]
async fn preparation_requires_the_current_live_root_lease_before_any_work_acquisition() {
    for change in [
        "owner",
        "incarnation",
        "epoch",
        "asserted-beyond-current",
        "current-expiry",
    ] {
        let fixture = PreparationFixture::new(
            "preparation-work-fence",
            PreparationWorkState::Queued,
            false,
        )
        .await;
        let root = fixture.repo.get(&fixture.id).await.unwrap();
        let mut asserted = root.realization.clone().unwrap();
        match change {
            "owner" => asserted.owner = "foreign".into(),
            "incarnation" => asserted.runtime_incarnation = "foreign:1:boot".into(),
            "epoch" => {
                asserted.epoch =
                    awaken_lease_contract::fence::Epoch::from_persisted(asserted.epoch)
                        .expect("fixture Root epoch was granted")
                        .next()
                        .expect("fixture Root epoch has a successor")
                        .get();
            }
            "asserted-beyond-current" => asserted.expires_at_unix_ms += 1,
            "current-expiry" => fixture.clock.set(asserted.expires_at_unix_ms),
            _ => unreachable!(),
        }
        assert!(matches!(fixture.app.work_assignments.acquire_for_preparation(&fixture.id, &asserted).await,
            Err(awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership)), "{change}");
        assert_eq!(
            fixture.work.acquisitions.load(Ordering::SeqCst),
            0,
            "{change}"
        );
        assert_eq!(
            *fixture.work.state.lock().unwrap(),
            PreparationWorkState::Queued,
            "{change}"
        );
        assert_eq!(
            fixture.repo.get(&fixture.id).await.unwrap(),
            root,
            "{change}"
        );
    }
    // A historical asserted deadline may lapse while its exact Root tenure is
    // still live under a nonregressed renewal. Only the current horizon owns
    // the time check, and the old observation grants no new tenure.
    let fixture = PreparationFixture::new(
        "preparation-work-renewed-root",
        PreparationWorkState::Queued,
        false,
    )
    .await;
    let root = fixture.repo.get(&fixture.id).await.unwrap();
    let mut asserted = root.realization.clone().unwrap();
    asserted.expires_at_unix_ms = fixture.clock.now_ms() - 1;
    assert!(asserted.expires_at_unix_ms < fixture.clock.now_ms());
    assert!(root.realization.as_ref().unwrap().expires_at_unix_ms > fixture.clock.now_ms());
    let ownership = fixture
        .app
        .work_assignments
        .acquire_for_preparation(&fixture.id, &asserted)
        .await
        .unwrap();
    assert!(matches!(ownership, SessionWorkOwnership::Leased(_)));
    assert_eq!(fixture.work.acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.work.releases.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.repo.get(&fixture.id).await.unwrap(), root);
}

#[tokio::test]
async fn preparation_rechecks_root_worker_and_profile_after_acquiring_work_without_releasing_it() {
    for change in ["root-revision", "worker-draining", "worker-profile"] {
        let fixture = PreparationFixture::new(
            "preparation-work-post-acquire",
            PreparationWorkState::Queued,
            true,
        )
        .await;
        let app = fixture.app.clone();
        let target = fixture.target.clone();
        let pending = tokio::spawn(async move {
            app.realizations
                .claim_next_session_preparation(target)
                .await
        });
        fixture.work.wait_until_acquired().await;
        let grant = fixture.work.lease();
        if change == "root-revision" {
            fixture
                .app
                .update_commands
                .update_session(
                    &fixture.id,
                    crate::SessionUpdateCommand {
                        title: None,
                        metadata: Some(crate::SessionMetadataUpdate::Patch(
                            [("fixture".into(), Some("advanced".into()))]
                                .into_iter()
                                .collect(),
                        )),
                        budget: None,
                        tools: None,
                        mcp_update: None,
                        idempotency_key: Some("preparation-post-acquire-revision".into()),
                        request_fingerprint: "preparation-post-acquire-revision".into(),
                        if_match: None,
                    },
                )
                .await
                .unwrap();
        } else {
            let mut worker = terminal_cleanup_worker(&fixture.target, terminal_cleanup_manifest());
            if change == "worker-draining" {
                worker.snapshot.state = awaken_worker_contract::WorkerState::Draining;
            } else {
                worker.snapshot.manifest.capacity.max_concurrent = 0;
                worker.snapshot.capability_fingerprint =
                    worker.snapshot.manifest.fingerprint().unwrap();
            }
            fixture.observations.replace(vec![worker]);
        }
        fixture.work.proceed.add_permits(1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_none(),
            "{change}"
        );
        let root = fixture.repo.get(&fixture.id).await.unwrap();
        let lease = root.realization.as_ref().unwrap();
        assert_eq!(lease.owner, fixture.target.owner, "{change}");
        assert_eq!(
            lease.epoch, 3,
            "{change} retains exact unissued lease evidence"
        );
        assert_eq!(
            lease.expires_at_unix_ms, 0,
            "{change} closes the unissued Root grant"
        );
        assert_eq!(
            fixture.work.lease(),
            grant,
            "{change} does not release a shared Work tenure"
        );
        assert_eq!(fixture.work.releases.load(Ordering::SeqCst), 0, "{change}");
        assert!(
            root.active_activity_epochs.is_empty(),
            "{change} grants no model activity"
        );
    }
}

#[tokio::test]
async fn aborted_preparation_reuses_the_current_root_and_work_tenure_on_duplicate_claim() {
    let fixture =
        PreparationFixture::new("preparation-work-abort", PreparationWorkState::Queued, true).await;
    let app = fixture.app.clone();
    let target = fixture.target.clone();
    let pending = tokio::spawn(async move {
        app.realizations
            .claim_next_session_preparation(target)
            .await
    });
    fixture.work.wait_until_acquired().await;
    let root = fixture.repo.get(&fixture.id).await.unwrap();
    let original = fixture.work.lease();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.repo.get(&fixture.id).await.unwrap(), root);
    assert_eq!(fixture.work.lease(), original);
    fixture.work.clock.set(2_000);
    // The original candidate cursor may wrap once after an examined root.
    let mut assignment = None;
    for _ in 0..2 {
        assignment = fixture
            .app
            .realizations
            .claim_next_session_preparation(fixture.target.clone())
            .await
            .unwrap();
        if assignment.is_some() {
            break;
        }
    }
    let assignment = assignment.expect("the same current Root and Work grant remain recoverable");
    assert_eq!(
        assignment.directive.authority.lease,
        root.realization.unwrap()
    );
    let renewed = fixture.work.lease();
    assert_eq!(renewed.work_id(), original.work_id());
    assert_eq!(renewed.owner(), original.owner());
    assert_eq!(renewed.epoch(), original.epoch());
    assert_eq!(
        renewed.expires_at_unix_ms(),
        fixture.work.clock.now_ms() + WORK_TTL_MS
    );
    assert!(renewed.expires_at_unix_ms() > original.expires_at_unix_ms());
    assert_eq!(fixture.work.releases.load(Ordering::SeqCst), 0);
    let after = fixture.repo.get(&fixture.id).await.unwrap();
    assert_eq!(
        after.realization.as_ref(),
        Some(&assignment.directive.authority.lease)
    );
    assert!(after.active_activity_epochs.is_empty());
    fixture.work.clock.set(3_000);
    let mut duplicate = None;
    for _ in 0..2 {
        duplicate = fixture
            .app
            .realizations
            .claim_next_session_preparation(fixture.target.clone())
            .await
            .unwrap();
        if duplicate.is_some() {
            break;
        }
    }
    let duplicate = duplicate.expect("an acknowledged duplicate retains the same Root grant");
    assert_eq!(
        duplicate.directive.authority.lease,
        assignment.directive.authority.lease
    );
    let repeated = fixture.work.lease();
    assert_eq!(repeated.work_id(), renewed.work_id());
    assert_eq!(repeated.owner(), renewed.owner());
    assert_eq!(repeated.epoch(), renewed.epoch());
    assert_eq!(
        repeated.expires_at_unix_ms(),
        fixture.work.clock.now_ms() + WORK_TTL_MS
    );
    assert_eq!(fixture.work.releases.load(Ordering::SeqCst), 0);
}
