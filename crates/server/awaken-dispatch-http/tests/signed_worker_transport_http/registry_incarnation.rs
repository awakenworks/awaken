//! Registry-incarnation fixtures and terminal-cleanup admission tests.
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn signed_draining_cleanup_keeps_its_original_renewal_authority() {
    // Causes: the exact existing cleanup Actor is Ready/Draining; Registry
    // heartbeats stay live while run-grace time passes; the root's own lease
    // remains live. L1 preserve the original Registry-bounded renewal law for
    // ongoing cleanup, including after the run-grace deadline. L2 terminal
    // polling still works. A safe forced stop needs separate Native source
    // retention and actual exit/Create evidence; a timer is not that evidence.
    use awaken_worker_contract::WorkerCurrentSource as _;
    let clock = Arc::new(ManualClock::new(10_000));
    let credential = WorkerSigningCredential::new(
        "drain-horizon-worker",
        "rotation",
        "credential",
        b"fixture-signing-secret".to_vec(),
    )
    .unwrap();
    let authenticator: Arc<dyn WorkerRequestAuthenticator> = Arc::new(
        SignedWorkerAuthenticator::new(credential.clone(), clock.clone())
            .with_time_policy(60_000, 0),
    );
    let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
    let control = Arc::new(RecordingSessionControl {
        clock: Some(clock.clone()),
        ..Default::default()
    });
    let work = Arc::new(RecordingSessionWorkAuthority::default());
    let dispatch = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let service = WorkerDispatchService::new(
        dispatch,
        authenticator,
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(30_000)),
    )
    .with_worker_directory(directory.clone(), 30_000)
    .with_session_control(control.clone())
    .with_terminal_cleanup_control(control.clone())
    .with_session_work_authority(work.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let bootstrap = WorkerUpstream::new(format!("http://{address}"))
        .with_worker_id("drain-horizon-worker")
        .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(
            credential,
            clock.clone(),
        )));
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = 1;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    let registered = WorkerControlClient::new(bootstrap.clone())
        .register_classified("drain-boot", manifest)
        .await
        .unwrap()
        .worker;
    let identity = registered.snapshot.identity;
    let upstream = bootstrap.with_worker_identity(identity.clone());
    let client = WorkerControlClient::new(upstream);
    let heartbeat = |sequence| WorkerHeartbeat {
        sequence,
        ready: true,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        source_retention: Default::default(),
    };
    client.heartbeat(&identity, heartbeat(1)).await.unwrap();
    *work.owner.lock().unwrap() = Some(identity.lease_owner());
    let mut command = awaken_session_realization_contract::RenewSessionRealization {
        session_id: "draining-session".into(),
        asserted_lease: awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 1,
            expires_at_unix_ms: 11_000,
        },
        requested_expires_at_unix_ms: 50_000,
    };
    assert_eq!(
        client
            .renew_session_realization(&identity, command.clone())
            .await
            .unwrap()
            .lease
            .expires_at_unix_ms,
        40_000,
        "H1 Registry cap"
    );
    assert_eq!(
        client.begin_drain(&identity, Some(12_000)).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        client
            .renew_session_realization(&identity, command.clone())
            .await
            .unwrap()
            .lease
            .expires_at_unix_ms,
        40_000,
        "L1 drain does not truncate an owned cleanup lease"
    );
    let admitted = control.renewals.lock().unwrap().len();
    // This is an existing legacy grant, not a claim minted by the test's new
    // drain path. Its assertion stays live on both sides of the drain deadline.
    command.asserted_lease.expires_at_unix_ms = 14_000;
    for (index, now) in [12_000, 12_001].into_iter().enumerate() {
        clock.set(now);
        client.heartbeat(&identity, heartbeat(now)).await.unwrap();
        let observed = directory
            .current(&identity.worker_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.snapshot.state, WorkerState::Draining);
        assert!(
            observed.snapshot.expires_at_ms > 14_000,
            "C2 current Registry liveness"
        );
        let renewed = client
            .renew_session_realization(&identity, command.clone())
            .await
            .unwrap();
        assert_eq!(
            renewed.lease.expires_at_unix_ms, observed.snapshot.expires_at_ms,
            "L1 current Registry remains the parent of ongoing cleanup"
        );
        assert_eq!(control.renewals.lock().unwrap().len(), admitted + index + 1);
        assert!(
            client
                .terminal_cleanup_work(&identity, "draining-session", &command.asserted_lease)
                .await
                .unwrap()
                .is_none(),
            "H4 existing terminal poll survives grant expiry"
        );
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerObservationSource for TestWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(self.0.lock().unwrap().clone().into_iter().collect())
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for TestWorkerDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .clone()
            .filter(|record| record.snapshot.identity.worker_id == worker_id))
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for TestWorkerDirectory {
    async fn incarnation(
        &self,
        identity: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<
        Option<awaken_worker_contract::RegisteredWorker>,
        awaken_worker_contract::RegistryError,
    > {
        awaken_worker_contract::read_current_incarnation(self, identity).await
    }
}

/// Cause/effect design: C1 the cleanup claim route has authenticated Worker-id
/// evidence but no Coordinator Worker Directory; C2 the request supplies a
/// syntactically complete identity, target, and cursor protocol. Effect E1: fail closed before
/// Session Control, because C1 cannot prove the current registry incarnation.
/// Decision rule D1: C1+C2 => HTTP 500 and zero claim calls. A local header is
/// deliberately insufficient incarnation authority; no compatibility bypass is
/// permitted for this recovery-only claim. Constraint/Invariant: cleanup claims
/// require the Coordinator's current registry incarnation, never header identity.
#[tokio::test]
async fn terminal_cleanup_claim_requires_current_registry_incarnation() {
    // The dispatch port reads "now" from its store; mirror the service clock.
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    let session_control = Arc::new(RecordingSessionControl::default());
    *session_control.projection.lock().unwrap() = Some(frozen_projection());
    let service = WorkerDispatchService::local(dispatch, awaken_time::system_clock())
        .with_session_control(session_control.clone());
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let identity = WorkerIdentity::new("local-worker", "local-incarnation", 1);
    let response = reqwest::Client::new()
        .post(format!(
            "http://{address}/v1/worker/session/cleanup/claim-next"
        ))
        .header("x-awaken-worker-id", "local-worker")
        .json(&serde_json::json!({
            "identity": identity,
            "target": awaken_session_realization_contract::SessionRealizationTarget {
                owner: "local-worker".into(),
                runtime_incarnation: identity.lease_owner(),
                lease_expires_at_unix_ms: u64::MAX,
                reassign_existing_lease: false,
            },
            "sweep_cursor_version": awaken_session_realization_contract::TERMINAL_CLEANUP_SWEEP_CURSOR_VERSION,
            "after_session_id": null,
        }))
        .send()
        .await
        .expect("D1 request reaches the authenticated route");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "D1/E1"
    );
    assert!(
        session_control.cleanup_claims.lock().unwrap().is_empty(),
        "D1/E1"
    );
}
