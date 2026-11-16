//! Consumer preparation through the original authenticated PostgreSQL Resume.
use super::*;
use crate::worker_claimed_persistence::test_cut::{FinalAdmissionCutKind, FinalAdmissionTestCut};
use awaken_session_application::{
    SessionMcpUpdate, SessionUpdateCommand, SessionUpdateCommands, SessionUpdateError,
    SessionUpdates,
};
use awaken_session_contract::{IdempotencyRecord, SessionRealizationRetirement};
use awaken_session_execution_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    ClaimedSessionCredentialPreparationContext, FailSessionRealization, RenewSessionRealization,
    RetireSessionRealization, SessionRealizationControl,
    SessionRealizationControlFailure as Failure, SessionRealizationDirective,
    SessionRealizationLeaseReceipt, SessionRealizationTarget,
};
use awaken_time::WallClockExt as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy)]
enum UpdateMode {
    Keyed,
    Unkeyed,
}

/// The issuer pause is outside the selected Dispatch transaction. The update
/// remains the real credential-only Session command and its original Root CAS.
/// This empty-MCP fixture exercises the preparation seam, command receipt and
/// no-change admission. Credential material/generation laws have their own
/// Session/Coordinator owners and the consumer's real expired-pin regressions.
struct PreparationControl {
    updates: Arc<dyn SessionUpdates>,
    clock: Arc<awaken_lease_testkit::ManualClock>,
    mode: UpdateMode,
    pause_next: AtomicBool,
    fail_next: AtomicBool,
    calls: AtomicUsize,
    outcomes: Mutex<Vec<(u64, bool)>>,
    reached: Notify,
    proceed: Semaphore,
}

impl PreparationControl {
    fn new(f: &Fixture, mode: UpdateMode, pause: bool, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            updates: f.managed.session_application().update_commands(),
            clock: f.clock.clone(),
            mode,
            pause_next: AtomicBool::new(pause),
            fail_next: AtomicBool::new(fail),
            calls: AtomicUsize::new(0),
            outcomes: Mutex::new(Vec::new()),
            reached: Notify::new(),
            proceed: Semaphore::new(0),
        })
    }

    fn key(id: &SessionId) -> String {
        format!("http-claimed-credential-preparation:{id}")
    }

    fn command(&self, id: &SessionId) -> SessionUpdateCommand {
        SessionUpdateCommand {
            title: None,
            metadata: None,
            budget: None,
            tools: None,
            mcp_update: Some(SessionMcpUpdate::CredentialLifecycle {
                source_id: "http-preparation-source".into(),
                revoked: false,
                candidates: Vec::new(),
            }),
            idempotency_key: matches!(self.mode, UpdateMode::Keyed).then(|| Self::key(id)),
            request_fingerprint: awaken_agent_contract::stable_fingerprint(&(
                "http-claimed-credential-preparation-v1",
                id.as_str(),
            )),
            if_match: None,
        }
    }

    fn record(&self, id: &SessionId) -> IdempotencyRecord {
        IdempotencyRecord {
            key: format!(
                "managed:update-command:{id}:{}",
                SessionUpdateCommands::update_operation_id(id.as_str(), &Self::key(id)),
            ),
            payload_hash: self.command(id).request_fingerprint,
        }
    }

    async fn reached(&self) {
        tokio::time::timeout(PROGRESS, self.reached.notified())
            .await
            .expect("the actual preparation callback reaches its issuer pause");
    }

    fn release(&self) {
        self.proceed.add_permits(1);
    }
}

fn update_failure(error: SessionUpdateError) -> Failure {
    match error {
        SessionUpdateError::NotIdle => Failure::NotReady,
        SessionUpdateError::Conflict => Failure::Conflict,
        SessionUpdateError::NotFound => Failure::NotFound,
        SessionUpdateError::Unavailable(message) => Failure::Unavailable(message),
        SessionUpdateError::Rejected(error) => match error.kind {
            awaken_runtime_contract::runs::RunErrorKind::BadRequest => {
                Failure::Invalid(error.message)
            }
            awaken_runtime_contract::runs::RunErrorKind::Conflict => Failure::Conflict,
            awaken_runtime_contract::runs::RunErrorKind::Unavailable => {
                Failure::Unavailable(error.message)
            }
            awaken_runtime_contract::runs::RunErrorKind::Internal => {
                Failure::Internal(error.message)
            }
        },
        error => Failure::Unavailable(error.to_string()),
    }
}

#[async_trait::async_trait]
impl SessionRealizationControl for PreparationControl {
    async fn prepare_claimed_session_credentials(
        &self,
        context: &dyn ClaimedSessionCredentialPreparationContext,
        command: &BeginSessionRealization,
    ) -> Result<(), Failure> {
        context
            .verify_current_session_at(command.session_id.as_str(), self.clock.now_ms())
            .await?;
        assert!(
            context.verify_at(self.clock.now_ms()).is_err(),
            "a released PG observation cannot masquerade as a held synchronous guard"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.pause_next.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.proceed
                .acquire()
                .await
                .expect("the fixture retains its issuer signal")
                .forget();
        }
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(Failure::Unavailable("fixture issuer was withdrawn".into()));
        }
        context
            .verify_current_session_at(command.session_id.as_str(), self.clock.now_ms())
            .await?;
        let outcome = self
            .updates
            .update_claimed_session_credentials(
                context,
                &command.session_id,
                self.command(&command.session_id),
            )
            .await
            .map_err(update_failure)?;
        self.outcomes
            .lock()
            .unwrap()
            .push((outcome.command_revision.0, outcome.command_applied));
        Ok(())
    }

    async fn begin_session_realization(
        &self,
        _: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, Failure> {
        unreachable!("PG resumes through its original application Begin and RootGate")
    }

    async fn renew_session_realization(
        &self,
        _: RenewSessionRealization,
    ) -> Result<SessionRealizationLeaseReceipt, Failure> {
        unreachable!("PG continuation renewal stays with the selected application owner")
    }

    async fn activate_session_realization(
        &self,
        _: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, Failure> {
        unreachable!("this fixture only invokes Resume")
    }

    async fn acknowledge_session_realization(
        &self,
        _: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, Failure> {
        unreachable!("this fixture only invokes Resume")
    }

    async fn fail_session_realization(&self, _: FailSessionRealization) -> Result<(), Failure> {
        unreachable!("this fixture only invokes Resume")
    }

    async fn retire_session_realization(
        &self,
        _: RetireSessionRealization,
    ) -> Result<SessionRealizationRetirement, Failure> {
        unreachable!("this fixture only invokes Resume")
    }
}

fn request(
    http: &HttpFixture,
    entry: &Entry,
    work: &SessionWorkAuthorityReceipt,
) -> tokio::task::JoinHandle<(axum::http::StatusCode, serde_json::Value)> {
    let upstream = http.upstream.clone();
    let identity = http.identity.clone();
    let claim = entry.claim.clone();
    let session = entry.id.clone();
    let work = work.clone();
    tokio::spawn(async move {
        let path = "/v1/worker/session/resume";
        let url = format!("{}{}", upstream.base_url(), path);
        let response = upstream
            .authorize("POST", path, upstream.client().post(url))
            .unwrap()
            .json(&serde_json::json!({
                "identity": identity,
                "claim": claim,
                "session_id": session,
                "session_work_authority": work,
                "continuation": null,
            }))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap())
    })
}

async fn response(
    request: tokio::task::JoinHandle<(axum::http::StatusCode, serde_json::Value)>,
) -> (axum::http::StatusCode, serde_json::Value) {
    tokio::time::timeout(PROGRESS, request)
        .await
        .expect("the actual HTTP request progresses on the original capacity-one pool")
        .unwrap()
}

async fn assert_unconsumed(f: &Fixture, entry: &Entry) {
    let guard = tokio::time::timeout(PROGRESS, f.dispatch.lock_commit_epoch(&entry.claim))
        .await
        .expect("the original capacity-one connection is returned")
        .unwrap()
        .expect("preparation did not consume the original claim");
    assert!(!guard.cancellation_requested());
    assert!(guard.permits_settlement());
    guard.release().await.unwrap();
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP claimed credential preparation qualification"]
async fn live_postgres_http_claimed_preparation_commits_once_and_replays_original_receipt() {
    let f = Fixture::new(1, true, true).await;
    let control = PreparationControl::new(&f, UpdateMode::Keyed, false, false);
    let http = HttpFixture::with_preparation(&f, control.clone(), None).await;
    let entry = f.entry_for(&http.identity.lease_owner()).await;
    finish_fixture_attempt(&f, &entry).await;
    let work = http.receipt(&f, &entry).await;
    let repository = f.managed.session_application().session_repository_handle();
    let record = control.record(&entry.id);
    assert!(
        repository
            .idempotency_receipt(&entry.id, &record.key)
            .await
            .unwrap()
            .is_none()
    );
    let first = tokio::time::timeout(
        PROGRESS,
        http.client.resume_session(
            &http.identity,
            &entry.claim,
            entry.id.as_str(),
            Some(&work),
            None,
        ),
    )
    .await
    .expect("the issuer and real Session update make progress on cap1")
    .unwrap()
    .unwrap();
    let accepted = repository
        .idempotency_receipt(&entry.id, &record.key)
        .await
        .unwrap()
        .unwrap();
    let current = repository.get(&entry.id).await.unwrap();
    assert_eq!(accepted.payload_hash, record.payload_hash);
    assert!(
        current.revision.0 > accepted.committed_revision.0,
        "Begin may advance the Root after the original credential command receipt"
    );
    tokio::time::timeout(
        PROGRESS,
        http.client.resume_session(
            &http.identity,
            &entry.claim,
            entry.id.as_str(),
            Some(&work),
            Some(&first.authority.lease),
        ),
    )
    .await
    .expect("replay validates the current Root while preserving the older receipt")
    .unwrap()
    .unwrap();
    assert_eq!(control.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *control.outcomes.lock().unwrap(),
        vec![
            (accepted.committed_revision.0, true),
            (accepted.committed_revision.0, false),
        ]
    );
    assert_eq!(
        repository
            .idempotency_receipt(&entry.id, &record.key)
            .await
            .unwrap()
            .unwrap(),
        accepted
    );
    assert_eq!(
        repository.get(&entry.id).await.unwrap(),
        current,
        "exact replay authors no second effect"
    );
    assert_unconsumed(&f, &entry).await;
    drop(repository);
    drop(http);
    drop(control);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP claimed credential preparation qualification"]
async fn live_postgres_http_unkeyed_credential_no_change_keeps_the_current_root() {
    let f = Fixture::new(1, true, true).await;
    let control = PreparationControl::new(&f, UpdateMode::Unkeyed, false, false);
    let http = HttpFixture::with_preparation(&f, control.clone(), None).await;
    let entry = f.entry_for(&http.identity.lease_owner()).await;
    finish_fixture_attempt(&f, &entry).await;
    let work = http.receipt(&f, &entry).await;
    let first = tokio::time::timeout(
        PROGRESS,
        http.client.resume_session(
            &http.identity,
            &entry.claim,
            entry.id.as_str(),
            Some(&work),
            None,
        ),
    )
    .await
    .expect("unkeyed no-change preparation progresses")
    .unwrap()
    .unwrap();
    let repository = f.managed.session_application().session_repository_handle();
    let before = repository.get(&entry.id).await.unwrap();
    tokio::time::timeout(
        PROGRESS,
        http.client.resume_session(
            &http.identity,
            &entry.claim,
            entry.id.as_str(),
            Some(&work),
            Some(&first.authority.lease),
        ),
    )
    .await
    .expect("the final no-change read validates on cap1")
    .unwrap()
    .unwrap();
    assert_eq!(repository.get(&entry.id).await.unwrap(), before);
    assert_eq!(control.calls.load(Ordering::SeqCst), 2);
    assert_unconsumed(&f, &entry).await;
    drop(repository);
    drop(http);
    drop(control);
    f.cleanup().await;
}

#[derive(Clone, Copy, Debug)]
enum AuthorityChange {
    Cancel,
    Takeover,
    Expire,
    Occupy,
    ReplaceWorkOwner,
}

async fn change_authority(f: &Fixture, entry: &Entry, change: AuthorityChange) {
    tokio::time::timeout(PROGRESS, async {
        match change {
            AuthorityChange::Cancel => {
                f.dispatch.cancel(&entry.claim.run_id).await.unwrap();
            }
            AuthorityChange::Takeover => {
                f.dispatch.cancel(&entry.claim.run_id).await.unwrap();
                let successor = f
                    .dispatch
                    .claim_run(
                        &entry.claim.run_id,
                        "http-preparation-successor",
                        60_000,
                        &Default::default(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert!(successor.lease.epoch() > entry.claim.epoch);
            }
            AuthorityChange::Expire => {
                let guard = f
                    .dispatch
                    .lock_commit_epoch(&entry.claim)
                    .await
                    .unwrap()
                    .unwrap();
                let expiry = guard.observation().expires_ms;
                guard.release().await.unwrap();
                f.clock.set(expiry);
            }
            AuthorityChange::Occupy => {
                assert_eq!(
                    f.dispatch.begin_attempt(&entry.claim, None).await.unwrap(),
                    awaken_dispatch::AttemptAdmission::Applied
                );
                let physical = f
                    .dispatch
                    .load_physical_attempt(&entry.claim.run_id)
                    .await
                    .unwrap()
                    .expect("the exact live claim now occupies its physical slot");
                assert_eq!(physical.claim, entry.claim);
                assert!(!physical.closing);
            }
            AuthorityChange::ReplaceWorkOwner => {
                assert!(
                    f.assignments
                        .release_session_work(&entry.grant)
                        .await
                        .unwrap()
                );
                let acquired = f
                    .assignments
                    .acquire_session_work(
                        &entry.id,
                        "http-preparation-work-successor",
                        SessionWorkAcquisition::ClaimedRun,
                    )
                    .await
                    .unwrap();
                assert!(matches!(acquired, SessionWorkOwnership::Leased(_)));
            }
        }
    })
    .await
    .expect("slow issuer holds no Dispatch connection while current authority changes");
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP claimed credential preparation qualification"]
async fn live_postgres_http_slow_issuer_rechecks_claim_work_expiry_and_physical_slot() {
    for change in [
        AuthorityChange::Cancel,
        AuthorityChange::Takeover,
        AuthorityChange::Expire,
        AuthorityChange::Occupy,
        AuthorityChange::ReplaceWorkOwner,
    ] {
        let f = Fixture::new(1, true, true).await;
        let control = PreparationControl::new(&f, UpdateMode::Keyed, true, false);
        let http = HttpFixture::with_preparation(&f, control.clone(), None).await;
        let entry = if matches!(change, AuthorityChange::Occupy) {
            f.entry_without_attempt_for(&http.identity.lease_owner())
                .await
        } else {
            let entry = f.entry_for(&http.identity.lease_owner()).await;
            finish_fixture_attempt(&f, &entry).await;
            entry
        };
        let work = http.receipt(&f, &entry).await;
        let repository = f.managed.session_application().session_repository_handle();
        let pending = request(&http, &entry, &work);
        control.reached().await;
        let root = repository.get(&entry.id).await.unwrap();
        // This checkout is the SAME capped product pool used by the request.
        assert_eq!(
            tokio::time::timeout(
                PROGRESS,
                sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&f.d)
            )
            .await
            .unwrap()
            .unwrap(),
            1
        );
        change_authority(&f, &entry, change).await;
        let current_work = f.work_row(&entry.id).await;
        let physical = f
            .dispatch
            .load_physical_attempt(&entry.claim.run_id)
            .await
            .unwrap();
        control.release();
        let (status, body) = response(pending).await;
        let (expected_status, expected_kind) = match change {
            AuthorityChange::Occupy => (axum::http::StatusCode::CONFLICT, "not_ready"),
            AuthorityChange::ReplaceWorkOwner => (axum::http::StatusCode::BAD_REQUEST, "invalid"),
            _ => (axum::http::StatusCode::CONFLICT, "stale_ownership"),
        };
        assert_eq!(status, expected_status, "{change:?}: {body}");
        assert_eq!(
            body["realization_error"]["kind"], expected_kind,
            "{change:?}"
        );
        assert_eq!(repository.get(&entry.id).await.unwrap(), root);
        assert_eq!(f.work_row(&entry.id).await, current_work);
        assert_eq!(
            f.dispatch
                .load_physical_attempt(&entry.claim.run_id)
                .await
                .unwrap(),
            physical
        );
        assert!(
            repository
                .idempotency_receipt(&entry.id, &control.record(&entry.id).key)
                .await
                .unwrap()
                .is_none()
        );
        assert!(control.outcomes.lock().unwrap().is_empty());
        drop(repository);
        drop(http);
        drop(control);
        f.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP claimed credential preparation qualification"]
async fn live_postgres_http_credential_commit_and_no_change_read_keep_the_final_claim_gate() {
    for (mode, kind) in [
        (
            UpdateMode::Keyed,
            FinalAdmissionCutKind::PreparedResumeRootCommit,
        ),
        (
            UpdateMode::Unkeyed,
            FinalAdmissionCutKind::PreparedResumeReadValidation,
        ),
    ] {
        let f = Fixture::new(1, true, true).await;
        let cut = Arc::new(FinalAdmissionTestCut::new(kind));
        let control = PreparationControl::new(&f, mode, false, false);
        let http = HttpFixture::with_preparation(&f, control.clone(), Some(cut.clone())).await;
        let entry = f.entry_for(&http.identity.lease_owner()).await;
        finish_fixture_attempt(&f, &entry).await;
        let work = http.receipt(&f, &entry).await;
        let repository = f.managed.session_application().session_repository_handle();
        let pending = request(&http, &entry, &work);
        tokio::time::timeout(PROGRESS, cut.reached())
            .await
            .expect("the credential command reaches its own final Root boundary");
        let root = repository.get(&entry.id).await.unwrap();
        let work_at_cut = f.work_row(&entry.id).await;
        f.dispatch.cancel(&entry.claim.run_id).await.unwrap();
        cut.release();
        let (status, body) = response(pending).await;
        assert_eq!(
            status,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "{kind:?}: {body}"
        );
        assert_eq!(body["realization_error"]["kind"], "unavailable");
        assert_eq!(repository.get(&entry.id).await.unwrap(), root);
        assert_eq!(f.work_row(&entry.id).await, work_at_cut);
        assert!(
            repository
                .idempotency_receipt(&entry.id, &control.record(&entry.id).key)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            control.outcomes.lock().unwrap().is_empty(),
            "the callback never reports a stale credential command as accepted"
        );
        drop(repository);
        drop(http);
        drop(control);
        f.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP claimed credential preparation qualification"]
async fn live_postgres_http_issuer_failure_preserves_the_same_claim_for_retry() {
    let f = Fixture::new(1, true, true).await;
    let control = PreparationControl::new(&f, UpdateMode::Keyed, false, true);
    let http = HttpFixture::with_preparation(&f, control.clone(), None).await;
    let entry = f.entry_for(&http.identity.lease_owner()).await;
    finish_fixture_attempt(&f, &entry).await;
    let work = http.receipt(&f, &entry).await;
    let repository = f.managed.session_application().session_repository_handle();
    let before = repository.get(&entry.id).await.unwrap();
    let (status, body) = response(request(&http, &entry, &work)).await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["realization_error"]["kind"], "unavailable");
    assert_eq!(repository.get(&entry.id).await.unwrap(), before);
    assert!(
        repository
            .idempotency_receipt(&entry.id, &control.record(&entry.id).key)
            .await
            .unwrap()
            .is_none()
    );
    assert_unconsumed(&f, &entry).await;
    let (status, body) = response(request(&http, &entry, &work)).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(control.calls.load(Ordering::SeqCst), 2);
    assert_eq!(control.outcomes.lock().unwrap().len(), 1);
    assert_unconsumed(&f, &entry).await;
    drop(repository);
    drop(http);
    drop(control);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP claimed credential preparation qualification"]
async fn live_postgres_http_aborted_preparation_returns_cap_one_for_the_same_http_retry() {
    let f = Fixture::new(1, true, true).await;
    let control = PreparationControl::new(&f, UpdateMode::Keyed, true, false);
    let http = HttpFixture::with_preparation(&f, control.clone(), None).await;
    let entry = f.entry_for(&http.identity.lease_owner()).await;
    finish_fixture_attempt(&f, &entry).await;
    let work = http.receipt(&f, &entry).await;
    let repository = f.managed.session_application().session_repository_handle();
    let before = repository.get(&entry.id).await.unwrap();
    // Dropping a client HTTP future does not guarantee Axum cancels its server
    // handler. Abort the exact selected owner's future at the issuer pause,
    // then retry through the authenticated HTTP edge over that SAME owner.
    let sessions = http.sessions.clone();
    let preparation = control.clone();
    let now = f.clock.now_ms();
    let command = awaken_dispatch_http::WorkerSessionResumeCommand {
        claim: entry.claim.clone(),
        session_id: entry.id.clone(),
        work_authority: Some(work.clone()),
        continuation: None,
        target: SessionRealizationTarget {
            owner: http.identity.worker_id.clone(),
            runtime_incarnation: http.identity.lease_owner(),
            lease_expires_at_unix_ms: now + 30_000,
            reassign_existing_lease: true,
        },
        registry_expires_at_unix_ms: now + 30_000,
    };
    let pending = tokio::spawn(async move { sessions.resume(command, preparation.as_ref()).await });
    control.reached().await;
    assert_eq!(
        tokio::time::timeout(
            PROGRESS,
            sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&f.d)
        )
        .await
        .unwrap()
        .unwrap(),
        1
    );
    pending.abort();
    assert!(
        tokio::time::timeout(PROGRESS, pending)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(repository.get(&entry.id).await.unwrap(), before);
    assert!(
        repository
            .idempotency_receipt(&entry.id, &control.record(&entry.id).key)
            .await
            .unwrap()
            .is_none()
    );
    assert_unconsumed(&f, &entry).await;
    let (status, body) = response(request(&http, &entry, &work)).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(control.calls.load(Ordering::SeqCst), 2);
    assert_eq!(control.outcomes.lock().unwrap().len(), 1);
    assert_unconsumed(&f, &entry).await;
    drop(repository);
    drop(http);
    drop(control);
    f.cleanup().await;
}
