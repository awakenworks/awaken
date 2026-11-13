//! Actual Worker HTTP closes through the original DispatchedMemory observer.
use super::*;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::EndCause;
use awaken_agent_contract::thread::commit::{RunDisposition, coordinator::Coordinator as _};
use awaken_dispatch_contract::DispatchSettlement as _;
use awaken_session_run_delivery::{
    AuthorityCommits, BackgroundRuns, CommittedReads, CompletionRegistry, MemoryBindingCompiler,
    MemoryExtractions, ResourceBindingVerifiers, SessionProjections, ThreadEventHub,
    WorkerPlacedSessions, WorkerPlacement,
};

struct FrozenMemory;

#[async_trait::async_trait]
impl awaken_resource_contract::LiveResourceBindingVerifier for FrozenMemory {
    async fn verify_memory_binding(
        &self,
        workspace: &str,
        store: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        assert_eq!(
            (workspace, store, version.0),
            ("http-memory-workspace", "http-memory", 4)
        );
        Ok(())
    }
    async fn verify_repository_binding(
        &self,
        _: &str,
        _: &str,
        _: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        unreachable!("the frozen fixture contains only Memory")
    }
}

fn memory_request(id: &SessionId) -> RunDispatch {
    use awaken_session_configuration_contract::resource::{
        ResolvedInput, ResolvedInputSource, ResolvedSessionResources, SessionResourceManifest,
    };
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .model(awaken_runtime_contract::ModelBinding::parse("default", "parking-model", "default").unwrap())
        .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_owned()])
        .plugin_config([(awaken_ext_memory::MEMORY_PLUGIN_ID.to_owned(), serde_json::json!({"binding_id":"memory", "recall_enabled":false, "extraction_enabled":true}))]).build();
    let manifest = SessionResourceManifest::new(
        "http-memory-workspace",
        ResolvedSessionResources::try_new(
            vec![ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: "memory".into(),
                source: ResolvedInputSource::MemoryStore {
                    memory_store_id: "http-memory".into(),
                    config: awaken_resource_contract::MemoryStoreConfigVersion {
                        memory_store_id: "http-memory".into(),
                        version: awaken_resource_contract::ConfigVersion(4),
                        retention_policy: Default::default(),
                    },
                },
                mount_path: "/mnt/memory".into(),
                access: awaken_resource_contract::ResourceAccess::ReadWrite,
                instructions: None,
            }],
            Vec::new(),
        )
        .unwrap(),
    );
    // The historical ordinary Root shape owns its Work but carries no Session
    // activity epoch; this case isolates Memory/Work settlement, not a synthetic
    // coordinated activity receipt or another Session boundary writer.
    RunDispatch::new(awaken_runtime_contract::RunActivation::new(
        RunId(format!("memory-run-{}", id.as_str())),
        ThreadId(id.to_string()),
        snapshot,
        Vec::new(),
    ))
    .with_session_resources(
        awaken_dispatch_execution::session_envelope::resource_envelope(&manifest).unwrap(),
    )
    .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
        "http-memory-workspace".into(),
    ))
}

async fn memory_observer(
    f: &Fixture,
    entry: &Entry,
    store: Arc<awaken_store_postgres::PostgresCommitCoordinator>,
) -> (
    Arc<dyn awaken_dispatch::DispatchSettlementObserver>,
    Arc<MemoryExtractions>,
) {
    let owner = Arc::new(
        awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(f.dispatch.clone()),
    );
    let local = awaken_coordinator::postgres_local_commit(store, ThreadId(entry.id.to_string()))
        .await
        .unwrap();
    let authority = Arc::new(
        awaken_dispatch_erasure::ReferenceAuthority::with_dispatch(f.clock.clone(), owner.clone())
            .with_commit(entry.id.as_str(), local),
    );
    let projections = Arc::new(SessionProjections::new("http-memory-workspace", None));
    let reads = Arc::new(CommittedReads::new(
        Arc::new(AuthorityCommits::new(authority)),
        projections.clone(),
    ));
    let hub = Arc::new(ThreadEventHub::new());
    let completion = Arc::new(CompletionRegistry::new(hub.clone()));
    let repository = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::with_pool(f.roots.clone())
            .await
            .unwrap(),
    );
    let extractions = Arc::new(MemoryExtractions::new(
        repository,
        Arc::new(BackgroundRuns::new()),
        f.clock.clone(),
    ));
    let process = awaken_dispatch_execution::ProcessDispatch::new(
        awaken_deployment_config::ExecutionTopology {
            foreground_delivery: awaken_deployment_config::ForegroundRunDelivery::Durable,
            claim_pool: awaken_deployment_config::ClaimPool::RemoteWorkersOnly,
            lease_owner: awaken_deployment_config::LeaseOwnerId::new("http-memory-coordinator")
                .unwrap(),
            lost_wake_recovery_bound: Duration::from_millis(5),
            queue_depth: awaken_deployment_config::QueueDepthSource::External,
        },
        f.clock.clone(),
        awaken_dispatch_execution::wake::DispatchHint::new(Arc::new(
            awaken_dispatch_execution::LocalWakeSignal::new(),
        )),
    );
    let validator = Arc::new(FrozenMemory);
    let content = awaken_resource_contract::MemoryExecutionContent::new(Arc::new(
        awaken_memory_store::VolatileMemoryRepository::new(f.clock.clone()),
    ));
    let sessions = WorkerPlacedSessions::new(WorkerPlacement {
        projections,
        publication: process.publication(),
        owner,
        reads,
        hub,
        completion,
        skill_bundles: None,
        resource_bindings: ResourceBindingVerifiers {
            memory: Some(validator.clone()),
            repositories: None,
        },
        memory_bindings: MemoryBindingCompiler::new(content, Some(validator)),
        memory_extractions: extractions.clone(),
        skill_catalog: None,
        wake: None,
        clock: f.clock.clone(),
        default_model: "default/parking-model".into(),
    });
    (sessions.worker_memory_settlement_observer(), extractions)
}

#[tokio::test]
#[ignore = "live PostgreSQL cap1 actual-HTTP Memory terminal qualification"]
async fn live_postgres_http_cap_one_settles_real_memory_once_without_held_guard() {
    // H13/H14: R1 exact Ended+empty slot registers one durable original Memory
    // intent and retires Work through HTTP; replay cannot duplicate it. R2 a
    // Running prefix and R3 an occupied physical slot register zero intent and
    // cannot release Work. Both committed reads and the actual intent repository
    // use the same product cap1 pool as Dispatch. The only volatile component is
    // Memory content (no extraction Worker/provider runs in this fixture).
    for cause in [
        "ended-empty",
        "running-empty",
        "ended-occupied",
        "ended-retired-work",
    ] {
        let f = Fixture::new(1, true, true).await;
        let expected = WorkerIdentity {
            worker_id: "http-pool-worker".into(),
            incarnation_id: "http-pool-boot".into(),
            generation: 1,
        };
        let entry = f
            .entry_with_dispatch(&expected.lease_owner(), memory_request)
            .await;
        if cause == "ended-retired-work" {
            assert!(
                f.release()
                    .release(&entry.claim, &entry.attempt, true)
                    .await
                    .unwrap()
            );
        }
        if cause != "ended-occupied" {
            finish_fixture_attempt(&f, &entry).await;
        }
        let before = f.work_row(&entry.id).await;
        let store = Arc::new(
            awaken_store_postgres::PostgresCommitCoordinator::with_pool(f.d.clone())
                .await
                .unwrap(),
        );
        let disposition = if cause == "running-empty" {
            RunDisposition::running(entry.claim.run_id.clone())
        } else {
            RunDisposition::ended(entry.claim.run_id.clone(), EndCause::NaturalEnd)
        };
        store
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                ThreadId(entry.id.to_string()),
                disposition,
                true,
                vec![Message::text(
                    MessageId(format!("input-{}", entry.id.as_str())),
                    Role::User,
                    "remember this preference",
                )],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        let (observer, extractions) = memory_observer(&f, &entry, store).await;
        let http = HttpFixture::with_observer(&f, Some(observer)).await;
        assert_eq!(
            http.identity, expected,
            "the real Registry issues the fixture's claimed incarnation"
        );
        let result = tokio::time::timeout(
            PROGRESS,
            http.queue.settle(
                &entry.claim.run_id,
                entry.claim.epoch,
                awaken_dispatch::DispatchOutcome::Done,
                &[],
            ),
        )
        .await
        .expect("H13/H14 no observer may wait behind its own cap1 Dispatch guard");
        let accepted = matches!(cause, "ended-empty" | "ended-retired-work");
        if accepted {
            assert_eq!(result.unwrap(), awaken_dispatch::SettleOutcome::Applied);
        } else if cause == "running-empty" {
            assert!(matches!(
                result,
                Err(awaken_dispatch::DispatchError::Rejected(_))
            ));
        } else {
            assert_eq!(result.unwrap(), awaken_dispatch::SettleOutcome::Fenced);
        }
        let id = format!(
            "memory-extraction:{}:{}",
            entry.id.as_str(),
            entry.claim.run_id.0
        );
        let repository = extractions.repository();
        let intent = repository.get_extraction(&id).await.unwrap();
        assert_eq!(intent.is_some(), accepted, "H13/H14 {cause}");
        if accepted {
            let intent = intent.unwrap();
            assert_eq!(intent.session_id, entry.id.as_str());
            assert_eq!(intent.logical_thread_id(), entry.id.as_str());
            assert_eq!(
                intent.transcript[0].content,
                vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "remember this preference"
                )]
            );
            assert_eq!(
                http.queue
                    .settle(
                        &entry.claim.run_id,
                        entry.claim.epoch,
                        awaken_dispatch::DispatchOutcome::Done,
                        &[]
                    )
                    .await
                    .unwrap(),
                awaken_dispatch::SettleOutcome::Fenced
            );
            let replay = repository.get_extraction(&id).await.unwrap().unwrap();
            assert_eq!(replay.auxiliary_run_id(), intent.auxiliary_run_id());
            assert_eq!(replay.transcript_snapshot, intent.transcript_snapshot);
            let after = f.work_row(&entry.id).await;
            assert_eq!(after.0, "stopped");
        } else {
            let after = f.work_row(&entry.id).await;
            assert_eq!(
                (after.0, after.1, after.2),
                (before.0, before.1, before.2),
                "R2/R3 zero Work release"
            );
        }
        drop(repository);
        drop(http);
        drop(extractions);
        f.cleanup().await;
    }
}

struct PauseAfterMemory {
    inner: Arc<dyn awaken_dispatch::DispatchSettlementObserver>,
    reached: tokio::sync::Notify,
    proceed: tokio::sync::Semaphore,
}
#[async_trait::async_trait]
impl awaken_dispatch::DispatchSettlementObserver for PauseAfterMemory {
    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &awaken_agent_contract::agent::run::RunState,
        cancelled: bool,
    ) -> Result<(), awaken_dispatch::DispatchSettlementError> {
        // Decorate the actual original observer; do not replace its committed
        // reads, binding compiler, intent CAS or extraction controller.
        self.inner
            .before_settle(dispatch, claim, state, cancelled)
            .await?;
        self.reached.notify_one();
        self.proceed
            .acquire()
            .await
            .expect("fixture owns the original cut")
            .forget();
        Ok(())
    }
}

#[tokio::test]
#[ignore = "live PostgreSQL controlled actual-HTTP Memory settlement takeover"]
async fn live_postgres_http_memory_observer_release_preserves_a_successor_before_settlement_cas() {
    // H14: original Ended/empty-slot read -> released D -> REAL Memory observer
    // registers one intent -> controlled pause -> successor claims and occupies
    // its own physical slot -> old request resumes. Final exact-epoch CAS must
    // return settled:false, preserving the successor claim+slot and the one
    // original intent. An observer invoked while holding D cannot reach the
    // cut on cap1; a dropped-fence final write destroys the independent oracle.
    let f = Fixture::new(1, true, true).await;
    let expected = WorkerIdentity {
        worker_id: "http-pool-worker".into(),
        incarnation_id: "http-pool-boot".into(),
        generation: 1,
    };
    let entry = f
        .entry_with_dispatch(&expected.lease_owner(), memory_request)
        .await;
    finish_fixture_attempt(&f, &entry).await;
    let store = Arc::new(
        awaken_store_postgres::PostgresCommitCoordinator::with_pool(f.d.clone())
            .await
            .unwrap(),
    );
    store
        .commit(awaken_agent_contract::ThreadCommit::assemble(
            ThreadId(entry.id.to_string()),
            RunDisposition::ended(entry.claim.run_id.clone(), EndCause::NaturalEnd),
            true,
            vec![Message::text(
                MessageId(format!("takeover-input-{}", entry.id.as_str())),
                Role::User,
                "retain this exact terminal preference",
            )],
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let (actual, extractions) = memory_observer(&f, &entry, store).await;
    let observer = Arc::new(PauseAfterMemory {
        inner: actual,
        reached: tokio::sync::Notify::new(),
        proceed: tokio::sync::Semaphore::new(0),
    });
    let http = HttpFixture::with_observer(&f, Some(observer.clone())).await;
    assert_eq!(http.identity, expected);
    let queue = awaken_coordinator_client::HttpDispatchQueue::new(
        http.upstream.base_url(),
        http.identity.clone(),
    );
    let old_claim = entry.claim.clone();
    let settling = tokio::spawn(async move {
        queue
            .settle(
                &old_claim.run_id,
                old_claim.epoch,
                awaken_dispatch::DispatchOutcome::Done,
                &[],
            )
            .await
    });
    tokio::time::timeout(PROGRESS, observer.reached.notified())
        .await
        .expect("actual Memory intent completes after D release on cap1");
    let id = format!(
        "memory-extraction:{}:{}",
        entry.id.as_str(),
        entry.claim.run_id.0
    );
    let repository = extractions.repository();
    let intent = repository
        .get_extraction(&id)
        .await
        .unwrap()
        .expect("the original observer really registered its intent");
    let successor = tokio::time::timeout(PROGRESS, async {
        f.dispatch.cancel(&entry.claim.run_id).await.unwrap();
        f.dispatch
            .claim_run(
                &entry.claim.run_id,
                "http-memory-successor",
                60_000,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap()
    })
    .await
    .expect("takeover must complete while the observer is paused without D");
    let successor_claim = RunClaim::from(&successor.lease);
    assert!(successor_claim.epoch > entry.claim.epoch);
    assert_eq!(
        f.dispatch
            .begin_attempt(&successor_claim, None)
            .await
            .unwrap(),
        awaken_dispatch::AttemptAdmission::Applied
    );
    let successor_slot = f
        .dispatch
        .load_physical_attempt(&entry.claim.run_id)
        .await
        .unwrap()
        .expect("independent current successor slot");
    observer.proceed.add_permits(1);
    assert_eq!(
        tokio::time::timeout(PROGRESS, settling)
            .await
            .expect("old request reaches its final epoch CAS")
            .unwrap()
            .unwrap(),
        awaken_dispatch::SettleOutcome::Fenced
    );
    let guard = f
        .dispatch
        .lock_commit_epoch(&successor_claim)
        .await
        .unwrap()
        .expect("old settle preserved the successor claim");
    assert_eq!(guard.request(), &successor.request);
    guard.release().await.unwrap();
    assert_eq!(
        f.dispatch
            .load_physical_attempt(&entry.claim.run_id)
            .await
            .unwrap(),
        Some(successor_slot)
    );
    let retained = repository.get_extraction(&id).await.unwrap().unwrap();
    assert_eq!(retained.auxiliary_run_id(), intent.auxiliary_run_id());
    assert_eq!(retained.transcript_snapshot, intent.transcript_snapshot);
    assert_eq!(retained.transcript, intent.transcript);
    drop(repository);
    drop(http);
    drop(observer);
    drop(extractions);
    f.cleanup().await;
}
