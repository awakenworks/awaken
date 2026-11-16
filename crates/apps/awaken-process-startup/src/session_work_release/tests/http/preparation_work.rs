//! Canonical preparation is revalidated after the authenticated HTTP Work gate.
use super::*;
use awaken_session_application_contract::SessionWorkerLeaseAuthority;
use awaken_session_execution_contract::SessionRealizationControl as _;
use awaken_time::WallClockExt as _;
use awaken_worker_contract::{WorkerIncarnationSource, WorkerObservationSource};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

pub(super) struct HttpPreparationRoles {
    pub(super) directory: Arc<MemoryWorkerDirectory>,
    pub(super) work: Arc<dyn SessionWorkerLeaseAuthority>,
}

struct ProfileObservations {
    directory: Arc<MemoryWorkerDirectory>,
    withdrawn: AtomicBool,
}

#[async_trait::async_trait]
impl WorkerObservationSource for ProfileObservations {
    async fn list(
        &self,
    ) -> Result<Vec<awaken_worker_contract::RegisteredWorker>, awaken_worker_contract::RegistryError>
    {
        let mut workers = WorkerObservationSource::list(self.directory.as_ref()).await?;
        if self.withdrawn.load(Ordering::SeqCst) {
            for worker in &mut workers {
                worker.snapshot.manifest.capacity.max_concurrent = 0;
                worker.snapshot.capability_fingerprint = worker
                    .snapshot
                    .manifest
                    .fingerprint()
                    .expect("fixture profile remains valid");
            }
        }
        Ok(workers)
    }
}

#[async_trait::async_trait]
impl WorkerIncarnationSource for ProfileObservations {
    async fn incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<
        Option<awaken_worker_contract::RegisteredWorker>,
        awaken_worker_contract::RegistryError,
    > {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|worker| worker.snapshot.identity == *identity))
    }
}

/// Only the HTTP dependency borrows this wrapper. The canonical application
/// retains its original WorkAssignments, so this cut follows its first acquire
/// and post-acquisition Root/Worker checks and the actual second Work effect.
struct SecondWorkAcquire {
    inner: Arc<awaken_session_application::SessionWorkAssignments>,
    pool: PgPool,
    calls: AtomicUsize,
    rows: Mutex<Option<(WorkRow, WorkRow)>>,
    reached: Notify,
    proceed: Semaphore,
}

#[async_trait::async_trait]
impl SessionWorkerLeaseAuthority for SecondWorkAcquire {
    async fn acquire_session_work(
        &self,
        session: &SessionId,
        owner: &str,
        cause: SessionWorkAcquisition,
    ) -> Result<SessionWorkOwnership, awaken_runtime_contract::runs::RunError> {
        assert_eq!(cause, SessionWorkAcquisition::ClaimedPreparation);
        assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
        let before = work_row_on(&self.pool, session).await;
        assert_eq!(
            before.0, "active",
            "canonical application already acquired its Work"
        );
        assert_eq!(before.1.as_deref(), Some(owner));
        let ownership = self
            .inner
            .acquire_session_work(session, owner, cause)
            .await?;
        assert!(ownership.permits_effect_for(owner));
        let after = work_row_on(&self.pool, session).await;
        *self.rows.lock().unwrap() = Some((before, after));
        self.reached.notify_one();
        self.proceed.acquire().await.unwrap().forget();
        Ok(ownership)
    }

    async fn release_session_work(
        &self,
        _: &SessionWorkGrant,
    ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
        panic!("an unissued preparation handoff cannot release a shared Work grant")
    }

    async fn release_worker_session_work(
        &self,
        _: &str,
    ) -> Result<usize, awaken_runtime_contract::runs::RunError> {
        panic!("an unissued preparation handoff cannot release Worker-wide Work")
    }
}

async fn pending_preparation(f: &Fixture, http: &HttpFixture) -> SessionId {
    let application = f.managed.session_application();
    let repository = application.session_repository_handle();
    let template = f
        .managed
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams::new("assistant", ENV),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let template = repository.get(&SessionId::from(template.id)).await.unwrap();
    let id = SessionId::from(format!("preparation-{}", f.tag));
    let mut baseline = template.frozen_baseline().unwrap().clone();
    baseline.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
    let mut root = awaken_session_contract::PersistedSession::frozen_with_budget(
        id.as_str(),
        baseline,
        template.resources,
        Default::default(),
        None,
        Default::default(),
        template.tools,
        template.budget,
    );
    root.execution = awaken_session_contract::SessionExecutionState::Idle;
    root.realization = Some(
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: http.identity.worker_id.clone(),
            runtime_incarnation: http.identity.lease_owner(),
            epoch: 3,
            expires_at_unix_ms: f.clock.now_ms() + 10_000,
        },
    );
    // Reuse the existing historical Idle-root fixture shape. The original
    // constructors and serde defaults retain supported pre-cutover history.
    let mut wire = serde_json::to_value(root).unwrap();
    wire.as_object_mut()
        .unwrap()
        .remove("environment_materialization");
    wire.as_object_mut().unwrap().remove("run_activities");
    let root: awaken_session_contract::PersistedSession = serde_json::from_value(wire).unwrap();
    assert!(root.requires_external_environment_owner());
    repository
        .create(
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
            root,
            awaken_session_contract::IdempotencyRecord {
                key: format!("seed:{id}"),
                payload_hash: format!("seed:{id}"),
            },
            Vec::new(),
        )
        .await
        .unwrap();
    let outcome = application.update_commands().update_session(&id, awaken_session_application::SessionUpdateCommand {
        title: None, metadata: None, budget: None, tools: None,
        mcp_update: Some(awaken_session_application::SessionMcpUpdate::PublicReplacement(vec![
            awaken_session_application::McpAttachmentCandidate {
                name: "docs".into(),
                target: awaken_session_application::McpAttachmentCandidateTarget::HttpUrl("https://preparation.example.test/mcp".into()),
                prompts_as_skills: false, published_credential: None,
                origin: awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
            },
        ])),
        idempotency_key: Some("preparation-handoff-demand".into()),
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&("preparation-handoff", id.as_str())),
        if_match: None,
    }).await.unwrap();
    assert!(outcome.command_applied);
    assert!(outcome.session.is_run_free_preparation_candidate());
    assert_eq!(
        f.work_row(&id).await.0,
        "queued",
        "accepted MCP demand wakes its real Work projection"
    );
    id
}

#[derive(Clone, Copy, Debug)]
enum HandoffChange {
    Current,
    RootRevision,
    WorkerDraining,
    WorkerProfile,
    ReplacementLease,
}

#[tokio::test]
#[ignore = "live PostgreSQL canonical actual-HTTP preparation handoff qualification"]
async fn live_postgres_http_preparation_handoff_rechecks_canonical_root_worker_and_profile_after_work()
 {
    for change in [
        HandoffChange::Current,
        HandoffChange::RootRevision,
        HandoffChange::WorkerDraining,
        HandoffChange::WorkerProfile,
        HandoffChange::ReplacementLease,
    ] {
        let directory = Arc::new(MemoryWorkerDirectory::new());
        let observations = Arc::new(ProfileObservations {
            directory: directory.clone(),
            withdrawn: AtomicBool::new(false),
        });
        let probe = Arc::new(awaken_session_application::ExecutionObservationProbe::default());
        let f = Fixture::with_execution(observations.clone(), probe).await;
        let gate = Arc::new(SecondWorkAcquire {
            inner: f.assignments.clone(),
            pool: f.work.clone(),
            calls: AtomicUsize::new(0),
            rows: Mutex::new(None),
            reached: Notify::new(),
            proceed: Semaphore::new(0),
        });
        let http = HttpFixture::with_controls(
            &f,
            None,
            None,
            None,
            Some(HttpPreparationRoles {
                directory: directory.clone(),
                work: gate.clone(),
            }),
            false,
        )
        .await;
        let id = pending_preparation(&f, &http).await;
        let app = f.managed.session_application();
        let repository = app.session_repository_handle();
        let target = awaken_session_execution_contract::SessionRealizationTarget {
            owner: http.identity.worker_id.clone(),
            runtime_incarnation: http.identity.lease_owner(),
            lease_expires_at_unix_ms: f.clock.now_ms() + 10_000,
            reassign_existing_lease: false,
        };
        let client = http.client.clone();
        let identity = http.identity.clone();
        let requested = target.clone();
        let request = tokio::spawn(async move {
            client
                .claim_next_session_preparation(&identity, requested)
                .await
        });
        tokio::time::timeout(PROGRESS, gate.reached.notified())
            .await
            .expect("the canonical selector and second HTTP Work acquire both completed");
        let issued = repository.get(&id).await.unwrap();
        let original_lease = issued.realization.clone().unwrap();
        let work = f.work_row(&id).await;
        let (first, second) = gate.rows.lock().unwrap().clone().unwrap();
        assert_eq!(
            first, second,
            "same-owner second acquire retains the original Work tenure"
        );
        assert_eq!(work, second);
        // This is the original capped product pool, available while the HTTP
        // caller waits after the real Work operation.
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
        match change {
            HandoffChange::Current => {}
            HandoffChange::RootRevision => {
                app.update_commands()
                    .update_session(
                        &id,
                        awaken_session_application::SessionUpdateCommand {
                            title: None,
                            metadata: Some(
                                awaken_session_application::SessionMetadataUpdate::Patch(
                                    [("handoff".into(), Some("advanced".into()))]
                                        .into_iter()
                                        .collect(),
                                ),
                            ),
                            budget: None,
                            tools: None,
                            mcp_update: None,
                            idempotency_key: Some("handoff-root-revision".into()),
                            request_fingerprint: "handoff-root-revision".into(),
                            if_match: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            HandoffChange::WorkerDraining => {
                awaken_dispatch::WorkerDirectory::begin_drain(
                    directory.as_ref(),
                    &http.identity,
                    f.clock.now_ms() + 20_000,
                )
                .await
                .unwrap();
            }
            HandoffChange::WorkerProfile => observations.withdrawn.store(true, Ordering::SeqCst),
            HandoffChange::ReplacementLease => {
                let replacement = app
                    .realizations()
                    .begin_session_realization(
                        awaken_session_execution_contract::BeginSessionRealization {
                            session_id: id.clone(),
                            expected_resources: None,
                            target: awaken_session_execution_contract::SessionRealizationTarget {
                                reassign_existing_lease: true,
                                ..target.clone()
                            },
                        },
                    )
                    .await
                    .unwrap();
                assert!(replacement.authority.lease.epoch > original_lease.epoch);
            }
        }
        let current = repository.get(&id).await.unwrap();
        gate.proceed.add_permits(1);
        let assignment = tokio::time::timeout(PROGRESS, request)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let after = repository.get(&id).await.unwrap();
        if matches!(change, HandoffChange::Current) {
            let assignment = assignment.expect("unchanged current handoff reaches the Worker");
            assert_eq!(assignment.session_id, id);
            assert_eq!(assignment.directive.authority.lease, original_lease);
            assert_eq!(after, current);
        } else {
            assert!(
                assignment.is_none(),
                "{change:?} withholds the old assignment"
            );
            if matches!(change, HandoffChange::ReplacementLease) {
                assert_eq!(
                    after, current,
                    "retiring the old grant preserves the complete replacement Root"
                );
                assert!(after.realization.as_ref().unwrap().epoch > original_lease.epoch);
                assert!(after.realization.as_ref().unwrap().expires_at_unix_ms > f.clock.now_ms());
            } else {
                let lease = after.realization.as_ref().unwrap();
                assert_eq!(lease.epoch, original_lease.epoch);
                assert_eq!(
                    lease.runtime_incarnation,
                    original_lease.runtime_incarnation
                );
                assert_eq!(
                    lease.expires_at_unix_ms, 0,
                    "{change:?} retires only the unissued Root grant"
                );
            }
        }
        assert_eq!(
            gate.calls.load(Ordering::SeqCst),
            1,
            "one upstream Work read for this HTTP claim"
        );
        assert_eq!(
            f.work_row(&id).await,
            work,
            "{change:?} retains the actual shared Work grant"
        );
        assert!(after.active_activity_epochs.is_empty());
        drop(http);
        f.cleanup().await;
    }
}
