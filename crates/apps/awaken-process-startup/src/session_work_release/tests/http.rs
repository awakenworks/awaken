//! Real Worker HTTP over the same selected PostgreSQL Session/Work/Dispatch.
use super::*;
mod races;
mod settlement;
use awaken_coordinator_client::WorkerControlClient;
use awaken_dispatch_http::{RegisteredDispatchDependencies, WorkerDispatchService};
use awaken_dispatch_testkit::worker_dispatch::worker_dispatch_dependencies;
use awaken_session_application_contract::{
    SessionWorkAuthorityReceipt, acquire_session_work_receipt,
};
use awaken_worker_contract::{WorkerHeartbeat, WorkerIdentity};
use awaken_worker_registry::MemoryWorkerDirectory;
use awaken_worker_transport_security::WorkerUpstream;

async fn finish_fixture_attempt(fixture: &Fixture, entry: &Entry) {
    use awaken_dispatch_contract::DispatchAttemptControl as _;
    // Entry creates and then closes this exact fixture-owned physical slot.
    // Ordinary ACK is fenced after close; only the existing trusted closure
    // record can retire it. No model/provider execution is started by Entry.
    let closure = awaken_dispatch_contract::AttemptClosureRecord::try_new(
        entry.attempt.clone(),
        awaken_dispatch_contract::AttemptClosureKind::ControlledExecutionClosed,
        format!("http-memory-fixture-closure-{}", entry.id.as_str()),
    )
    .unwrap();
    assert_eq!(
        fixture
            .dispatch
            .finish_attempt_closure(&closure)
            .await
            .unwrap(),
        awaken_dispatch::SettleOutcome::Applied
    );
}
struct HttpFixture {
    client: WorkerControlClient,
    identity: WorkerIdentity,
    queue: awaken_coordinator_client::HttpDispatchQueue,
    upstream: WorkerUpstream,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl HttpFixture {
    async fn new(fixture: &Fixture) -> Self {
        Self::with_observer(fixture, None).await
    }

    async fn with_observer(
        fixture: &Fixture,
        observer: Option<Arc<dyn awaken_dispatch::DispatchSettlementObserver>>,
    ) -> Self {
        Self::with_controls(fixture, observer, None).await
    }

    async fn with_cut(
        fixture: &Fixture,
        cut: Arc<crate::worker_claimed_persistence::test_cut::FinalAdmissionTestCut>,
    ) -> Self {
        Self::with_controls(fixture, None, Some(cut)).await
    }

    async fn with_controls(
        fixture: &Fixture,
        observer: Option<Arc<dyn awaken_dispatch::DispatchSettlementObserver>>,
        cut: Option<Arc<crate::worker_claimed_persistence::test_cut::FinalAdmissionTestCut>>,
    ) -> Self {
        let directory = Arc::new(MemoryWorkerDirectory::new());
        let application = fixture.managed.session_application();
        // The same capped D handle supplies recovery; the existing volatile
        // authority is only the explicit non-PG fallback required by assembly.
        // No second product pool or claim writer is opened for these requests.
        let runtime = Arc::new(awaken_dispatch_erasure::ReferenceAuthority::with_dispatch(
            fixture.clock.clone(),
            Arc::new(
                awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(
                    fixture.dispatch.clone(),
                ),
            ),
        ));
        awaken_store_postgres::PostgresCommitCoordinator::with_pool(fixture.d.clone())
            .await
            .unwrap();
        let persistence = SessionWorkReleasePersistence::Postgres {
            pool: fixture.d.clone(),
            roots: if fixture.roots_shared {
                ClaimConnectionUse::Borrowed
            } else {
                ClaimConnectionUse::Independent
            },
            work: if fixture.work_shared {
                ClaimConnectionUse::Borrowed
            } else {
                ClaimConnectionUse::Independent
            },
        };
        let mut dependencies = RegisteredDispatchDependencies {
            claim_persistence: crate::worker_claimed_persistence::select_worker_claimed_persistence(
                persistence,
                awaken_deployment_config::StoreKind::Postgres,
                runtime,
                application.clone(),
                fixture.clock.clone(),
                cut,
            ),
            realizations: application.realizations(),
            session_preparation: application.realizations(),
            terminal_cleanup: application.terminal_cleanups(),
            coordination: application.coordination(),
            session_work: fixture.assignments.clone(),
            worker_work_release: fixture.release(),
            ..worker_dispatch_dependencies(
                fixture.dispatch.clone(),
                directory,
                fixture.clock.clone(),
            )
        };
        if let Some(observer) = observer {
            dependencies.terminal_observer = observer;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = WorkerDispatchService::new(dependencies);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                awaken_dispatch_http::dispatch_transport_router_with_service(Arc::new(service)),
            )
            .await
            .unwrap();
        });
        let client = WorkerControlClient::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_id("http-pool-worker"),
        );
        let registered = client
            .register_classified(
                "http-pool-boot",
                awaken_dispatch_testkit::worker_http::current_dispatch_codec_manifest(),
            )
            .await
            .unwrap();
        let identity = registered.worker.snapshot.identity;
        client
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: Default::default(),
                    acp_capability_observations: Default::default(),
                    source_retention: Default::default(),
                },
            )
            .await
            .unwrap();
        let queue = awaken_coordinator_client::HttpDispatchQueue::new(
            format!("http://{address}"),
            identity.clone(),
        );
        let upstream = WorkerUpstream::new(format!("http://{address}"))
            .with_worker_id(identity.worker_id.clone())
            .with_worker_identity(identity.clone());
        Self {
            client,
            queue,
            upstream,
            identity,
            server,
        }
    }

    async fn receipt(&self, fixture: &Fixture, entry: &Entry) -> SessionWorkAuthorityReceipt {
        acquire_session_work_receipt(
            fixture.assignments.as_ref(),
            &entry.id,
            &self.identity.lease_owner(),
            SessionWorkAcquisition::ClaimedRun,
        )
        .await
        .unwrap()
        .expect("the existing Work owner issues the exact original receipt")
    }
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP shared-pool qualification"]
async fn live_postgres_http_cap_one_resumes_the_selected_session() {
    // H3: real Session/Work/Dispatch share cap1; a registered Ready Worker has
    // the exact claim and original Work receipt. The real HTTP Resume must
    // complete and return that Worker's directive. A guard followed by pooled
    // SessionRepository::get cannot progress, even though all direct release
    // and commit tests pass. No RecordingSessionControl or fake grant hides it.
    let fixture = Fixture::new(1, true, true).await;
    let http = HttpFixture::new(&fixture).await;
    let entry = fixture.entry_for(&http.identity.lease_owner()).await;
    finish_fixture_attempt(&fixture, &entry).await;
    let receipt = http.receipt(&fixture, &entry).await;
    let before = fixture.work_row(&entry.id).await;
    let resumed = tokio::time::timeout(
        PROGRESS,
        http.client.resume_session(
            &http.identity,
            &entry.claim,
            entry.id.as_str(),
            Some(&receipt),
            None,
        ),
    )
    .await
    .expect("H3 actual HTTP Resume must not wait for a second shared checkout")
    .expect("H3 the selected Session application accepts the exact claim")
    .expect("H3 actual Root truth returns its frozen directive");
    assert_eq!(
        resumed.authority.lease.runtime_incarnation,
        http.identity.lease_owner()
    );
    assert_eq!(resumed.authority.lease.owner, http.identity.worker_id);
    let after = fixture.work_row(&entry.id).await;
    assert_eq!(after.0, before.0, "H3 Resume does not stop the Work");
    assert_eq!(after.2, before.2, "H3 Resume retains the exact Work epoch");
    // H3 wire decision table: C1 stale continuation is a real renewal Control
    // refusal => 409 + exact StaleOwnership; C2 missing Work is a Runtime input
    // refusal => original400 one-field Boundary; C3 an invalid INITIAL claim
    // stays Runtime400, unlike lost admitted Begin authority. None authors Root.
    // Work observation before a renewal refusal remains permitted by main.
    let repository = fixture
        .managed
        .session_application()
        .session_repository_handle();
    let root_before_refusals = repository.get(&entry.id).await.unwrap();
    let mut stale = resumed.authority.lease.clone();
    stale.epoch = awaken_lease_contract::fence::Epoch::from_persisted(stale.epoch)
        .unwrap()
        .next()
        .unwrap()
        .get();
    let mut invalid_claim = entry.claim.clone();
    invalid_claim.epoch = awaken_lease_contract::fence::Epoch::from_persisted(invalid_claim.epoch)
        .unwrap()
        .next()
        .unwrap()
        .get();
    for (claim, work, continuation, expected_status, expected_control, boundary_message) in [
        (
            entry.claim.clone(),
            Some(receipt.clone()),
            Some(stale),
            axum::http::StatusCode::CONFLICT,
            true,
            "",
        ),
        (
            entry.claim.clone(),
            None,
            None,
            axum::http::StatusCode::BAD_REQUEST,
            false,
            "Session resume requires exact Work authority",
        ),
        (
            invalid_claim,
            Some(receipt.clone()),
            None,
            axum::http::StatusCode::BAD_REQUEST,
            false,
            "Session claim is stale",
        ),
    ] {
        let path = "/v1/worker/session/resume";
        let url = format!("{}{}", http.upstream.base_url(), path);
        let response = tokio::time::timeout(PROGRESS, async {
            http.upstream
                .authorize("POST", path, http.upstream.client().post(url))
                .unwrap()
                .json(&serde_json::json!({"identity":http.identity, "claim":claim,
                    "session_id":entry.id, "session_work_authority":work,
                    "continuation":continuation}))
                .send()
                .await
                .unwrap()
        })
        .await
        .expect("selected PG wire refusal progresses on cap1");
        assert_eq!(response.status(), expected_status);
        let body: serde_json::Value = response.json().await.unwrap();
        if expected_control {
            let cause: awaken_session_execution_contract::SessionRealizationControlFailure =
                serde_json::from_value(body["realization_error"].clone()).unwrap();
            assert_eq!(
                cause,
                awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership
            );
        } else {
            assert_eq!(body, serde_json::json!({"error":boundary_message}));
        }
        assert_eq!(
            repository.get(&entry.id).await.unwrap(),
            root_before_refusals
        );
    }
    drop(repository);
    drop(http);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP shared-pool qualification"]
async fn live_postgres_http_cap_two_resumes_distinct_claims() {
    // H4: two legitimate logical children borrow one physical Session/Work.
    // An independent fixture transaction first blocks both exact Dispatch
    // rows. Seeing both real PostgreSQL waiters proves that each request owns
    // one of the two product connections before release. Thus a second pooled
    // checkout is a causal deadlock, not a scheduler-dependent concurrent race.
    // The original ten-second progress bound is unchanged; no extra product
    // pool, copied SQL writer, sleep or weaker authority assertion is used.
    // Both exact children also renew the same parent Work: the ordinary writer
    // must serialize those renewals, not reject one with a stale RR snapshot.
    let fixture = Fixture::new(2, true, true).await;
    let http = HttpFixture::new(&fixture).await;
    let parent = fixture.entry_for(&http.identity.lease_owner()).await;
    finish_fixture_attempt(&fixture, &parent).await;
    let receipt = http.receipt(&fixture, &parent).await;
    let mut claims = Vec::new();
    for suffix in ["left", "right"] {
        let activation = awaken_dispatch_testkit::worker_http::activation(&format!(
            "{}-{suffix}",
            parent.id.as_str()
        ));
        let request = RunDispatch::new(activation)
            .for_session(ThreadId(parent.id.as_str().to_owned()))
            .with_session_activity_epoch(1);
        let run = request.run_id().clone();
        fixture.dispatch.enqueue(request).await.unwrap();
        let claimed = fixture
            .dispatch
            .claim_run(
                &run,
                &http.identity.lease_owner(),
                60_000,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
        claims.push(RunClaim::from(&claimed.lease));
    }
    let before = fixture.work_row(&parent.id).await;
    let mut blocker = fixture.observer.begin().await.unwrap();
    for claim in &claims {
        sqlx::query("SELECT run_id FROM runtime_dispatch WHERE run_id=$1 FOR UPDATE")
            .bind(&claim.run_id.0)
            .execute(&mut *blocker)
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for claim in claims {
        let client = http.client.clone();
        let identity = http.identity.clone();
        let session = parent.id.clone();
        let receipt = receipt.clone();
        tasks.push(tokio::spawn(async move {
            client
                .resume_session(&identity, &claim, session.as_str(), Some(&receipt), None)
                .await
        }));
    }
    tokio::time::timeout(PROGRESS, async {
        loop {
            let waiters: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock' AND query LIKE '%runtime_dispatch%'",
            )
            .bind(&fixture.tag).fetch_one(&fixture.observer).await.unwrap();
            if waiters == 2 { break; }
            assert!(tasks.iter().all(|task| !task.is_finished()), "H4 each HTTP request reaches its real claim fence");
            tokio::task::yield_now().await;
        }
    }).await.expect("H4 both actual claim-row waiters must be observable");
    blocker.rollback().await.unwrap();
    for task in tasks {
        let resumed = tokio::time::timeout(PROGRESS, task)
            .await
            .expect("H4 actual HTTP requests cannot monopolize both shared connections")
            .unwrap()
            .expect("H4 exact child Resume is admitted")
            .expect("H4 original Session directive");
        assert_eq!(
            resumed.authority.lease.runtime_incarnation,
            http.identity.lease_owner()
        );
    }
    let after = fixture.work_row(&parent.id).await;
    assert_eq!(
        after.0, before.0,
        "H4 child Resume does not stop parent Work"
    );
    assert_eq!(
        after.2, before.2,
        "H4 both children retain its original epoch"
    );
    drop(http);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP shared-pool qualification"]
async fn live_postgres_http_cap_one_checkpoint_and_recovery_keep_exact_fences() {
    // H0-H2/H6: all three checkpoint verbs and the real PostgreSQL recovery
    // reader enter through HTTP on cap1. Current D+Work returns one exact
    // original snapshot; cancellation fences writes and recovery without
    // erasing the previously committed checkpoint. No second product pool.
    use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
    use awaken_dispatch_contract::ClaimedCheckpointTransport as _;
    let fixture = Fixture::new(1, true, true).await;
    let http = HttpFixture::new(&fixture).await;
    let entry = fixture.entry_for(&http.identity.lease_owner()).await;
    let receipt = http.receipt(&fixture, &entry).await;
    let checkpoint = StreamCheckpoint {
        run_id: entry.claim.run_id.0.clone(),
        thread_id: entry.id.to_string(),
        model: "default/parking-model".into(),
        partial_text: "partial".into(),
        partial_tools: Vec::new(),
        retry_count: 0,
    };
    tokio::time::timeout(PROGRESS, async {
        assert_eq!(
            http.queue
                .put_stream_checkpoint(&entry.claim, checkpoint.clone())
                .await
                .unwrap(),
            awaken_dispatch::SettleOutcome::Applied
        );
        assert_eq!(
            http.queue
                .load_stream_checkpoint(&entry.claim)
                .await
                .unwrap(),
            Some(checkpoint.clone())
        );
        let snapshot = http
            .queue
            .load_recovery_snapshot(&entry.claim, Some(&receipt))
            .await
            .unwrap();
        assert_eq!(snapshot.thread_id.0, entry.id.as_str());
        assert_eq!(snapshot.claimed_run_id, entry.claim.run_id);
        assert_eq!(
            http.queue
                .delete_stream_checkpoint(&entry.claim)
                .await
                .unwrap(),
            awaken_dispatch::SettleOutcome::Applied
        );
        assert!(
            http.queue
                .load_stream_checkpoint(&entry.claim)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            http.queue
                .put_stream_checkpoint(&entry.claim, checkpoint.clone())
                .await
                .unwrap(),
            awaken_dispatch::SettleOutcome::Applied
        );
        fixture.dispatch.cancel(&entry.claim.run_id).await.unwrap();
        assert_eq!(
            http.queue
                .put_stream_checkpoint(&entry.claim, checkpoint.clone())
                .await
                .unwrap(),
            awaken_dispatch::SettleOutcome::Fenced
        );
        assert_eq!(
            http.queue
                .delete_stream_checkpoint(&entry.claim)
                .await
                .unwrap(),
            awaken_dispatch::SettleOutcome::Fenced
        );
        assert!(
            http.queue
                .load_stream_checkpoint(&entry.claim)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            http.queue
                .load_recovery_snapshot(&entry.claim, Some(&receipt))
                .await,
            Err(awaken_dispatch::DispatchError::Conflict(_))
        ));
        let mut connection = fixture.d.acquire().await.unwrap();
        assert_eq!(
            awaken_dispatch::PostgresStreamCheckpointStore::get_on(
                &mut connection,
                &entry.claim.run_id.0
            )
            .await,
            awaken_agent_contract::observation::Observed::Present(checkpoint)
        );
    })
    .await
    .expect("H0-H2/H6 actual HTTP verbs cannot wait for another cap1 checkout");
    drop(http);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "live PostgreSQL actual-HTTP partial-pool qualification"]
async fn live_postgres_http_resume_partial_affinity_keeps_original_owners() {
    // H3 partial 2x2: Dispatch PG always selects the composed path. Only the
    // sources sharing its actual pool borrow D's connection; the independent
    // sources use their original ports. This asserts progress and original Work
    // effects, not a cross-database rollback guarantee.
    for (roots, work) in [(true, true), (true, false), (false, true), (false, false)] {
        let fixture = Fixture::new(1, roots, work).await;
        let http = HttpFixture::new(&fixture).await;
        let entry = fixture.entry_for(&http.identity.lease_owner()).await;
        finish_fixture_attempt(&fixture, &entry).await;
        let receipt = http.receipt(&fixture, &entry).await;
        let before = fixture.work_row(&entry.id).await;
        let directive = tokio::time::timeout(
            PROGRESS,
            http.client.resume_session(
                &http.identity,
                &entry.claim,
                entry.id.as_str(),
                Some(&receipt),
                None,
            ),
        )
        .await
        .expect("each selected source affinity must progress")
        .unwrap()
        .unwrap();
        assert_eq!(
            directive.authority.lease.runtime_incarnation,
            http.identity.lease_owner()
        );
        let after = fixture.work_row(&entry.id).await;
        assert_eq!((after.0, after.2), (before.0, before.2));
        drop(http);
        fixture.cleanup().await;
    }
}
