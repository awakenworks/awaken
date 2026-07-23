//! The production request identity is wired through lifecycle and dispatch
//! clients over a real socket, including the post-registration incarnation bind.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    DispatchQueue, MemoryDispatchStore, RegisteredWorker, RegistryError, RegistryMutation,
    RunDispatch, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_runtime_contract::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    FixedWorkerLeasePolicy, ManualWorkerClock, SignedWorkerAuthenticator,
    SignedWorkerRequestAuthorizer, WorkerControlClient, WorkerDispatchService,
    WorkerSigningCredential, WorkerUpstream, dispatch_transport_router_with_service,
    worker_dispatch_store_with_upstream,
};

#[derive(Default)]
struct TestWorkerDirectory(Mutex<Option<RegisteredWorker>>);

#[async_trait::async_trait]
impl WorkerDirectory for TestWorkerDirectory {
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let record = RegisteredWorker {
            snapshot: WorkerSnapshot {
                identity: WorkerIdentity::new(
                    registration.worker_id,
                    registration.incarnation_id,
                    1,
                ),
                state: WorkerState::Starting,
                capability_fingerprint: registration.manifest.fingerprint().unwrap(),
                manifest: registration.manifest,
                in_flight: 0,
                available_credentials: Default::default(),
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
            heartbeat_sequence: 0,
            registered_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            drain_deadline_ms: None,
        };
        *self.0.lock().unwrap() = Some(record.clone());
        Ok(record)
    }

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        let mut current = self.0.lock().unwrap();
        let Some(record) = current.as_mut() else {
            return Ok(RegistryMutation::NotFound);
        };
        if &record.snapshot.identity != identity {
            return Ok(RegistryMutation::StaleIncarnation);
        }
        record.snapshot.state = if heartbeat.ready {
            WorkerState::Ready
        } else {
            WorkerState::Starting
        };
        record.snapshot.in_flight = heartbeat.in_flight;
        record.snapshot.available_credentials = heartbeat.available_credentials;
        record.snapshot.expires_at_ms = now_ms.saturating_add(ttl_ms);
        record.heartbeat_sequence = heartbeat.sequence;
        record.heartbeat_at_ms = now_ms;
        Ok(RegistryMutation::Applied)
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .clone()
            .filter(|record| record.snapshot.identity.worker_id == worker_id))
    }

    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(self.0.lock().unwrap().clone().into_iter().collect())
    }

    async fn expire(&self, _now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        Ok(Vec::new())
    }
}

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("signed-run".into()),
        ThreadId("signed-thread".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("signed-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("signed-agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("signed-catalog".into()),
                instructions: String::new(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "genai"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("signed-snapshot-fingerprint".into()),
        },
        vec![Message::text(
            MessageId("signed-input".into()),
            Role::User,
            "run",
        )],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_identity_covers_register_heartbeat_and_dispatch() {
    let clock = Arc::new(ManualWorkerClock::new(10_000));
    let credential = WorkerSigningCredential::new(
        "signed-http-worker",
        "rotation-1",
        "credential-1",
        b"test-only-signing-secret".to_vec(),
    )
    .unwrap();
    let authenticator = SignedWorkerAuthenticator::new(credential.clone())
        .with_clock(clock.clone())
        .with_time_policy(60_000, 0);
    let directory = Arc::new(TestWorkerDirectory::default());
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let service = WorkerDispatchService::new(
        dispatch.clone(),
        Arc::new(authenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(30_000)),
    )
    .with_worker_directory(directory, 30_000);
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let authorizer = Arc::new(
        SignedWorkerRequestAuthorizer::new(credential)
            .with_clock(clock.clone())
            .with_assertion_ttl_ms(10_000),
    );
    let bootstrap = WorkerUpstream::new(format!("http://{address}"))
        .with_worker_id("signed-http-worker")
        .with_request_authorizer(authorizer);
    let manifest = WorkerManifest {
        build_digest: "signed-http-build".into(),
        ..WorkerManifest::default()
    };
    let registered = WorkerControlClient::new(bootstrap.clone())
        .register("signed-http-boot", manifest)
        .await
        .expect("bootstrap assertion registers the Worker");
    assert!(
        WorkerControlClient::new(bootstrap.clone())
            .heartbeat(
                &registered.snapshot.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    available_credentials: Default::default(),
                },
            )
            .await
            .is_err(),
        "a bootstrap assertion cannot act as an allocated incarnation"
    );

    let upstream = bootstrap.with_worker_identity(registered.snapshot.identity.clone());
    let heartbeat = WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                available_credentials: Default::default(),
            },
        )
        .await
        .expect("incarnation-bound assertion heartbeats");
    assert_eq!(heartbeat, RegistryMutation::Applied);

    let queue =
        worker_dispatch_store_with_upstream(&upstream, registered.snapshot.identity.clone());
    queue
        .enqueue(RunDispatch::new(activation()))
        .await
        .expect("signed dispatch enqueue");
    let claimed = queue
        .claim(&registered.snapshot.identity.lease_owner(), 30_000, 10_000)
        .await
        .expect("signed dispatch claim")
        .expect("queued run is claimable");
    assert_eq!(
        claimed.lease.owner,
        registered.snapshot.identity.lease_owner()
    );
    let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
    assert!(
        queue
            .claim_is_current(&claim, 10_000)
            .await
            .expect("signed exact-claim verification")
    );
    clock.set(20_000);
    WorkerControlClient::new(upstream)
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 2,
                ready: true,
                in_flight: 1,
                available_credentials: Default::default(),
            },
        )
        .await
        .expect("registry lease remains live while the dispatch lease expires");
    clock.set(claimed.lease.expires_ms + 1);
    assert!(
        !queue
            .claim_is_current(&claim, claimed.lease.expires_ms + 1)
            .await
            .expect("signed expired-claim verification")
    );
}
