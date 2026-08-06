use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_run_ingress::{
    DispatchQueue, MemoryDispatchStore, PlacementRequirements, RunDispatch,
    WORKER_LOCAL_CREDENTIALS_CAPABILITY,
};
use awaken_run_ingress_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryStreamCheckpointStore;
use awaken_worker_registry::{
    MemoryWorkerDirectory, RegistryMutation, WorkerCredentialObservation, WorkerCredentialRevision,
    WorkerDirectory, WorkerHeartbeat, WorkerManifest, WorkerState,
};
use awaken_worker_runtime::HttpDispatchQueue;
use awaken_worker_runtime::{WorkerControlClient, WorkerRegistrationError};
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock, WorkerUpstream,
};

#[tokio::test]
async fn authenticated_client_drives_the_registry_lifecycle_over_real_http() {
    let clock = Arc::new(ManualWorkerClock::new(100));
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let service = WorkerDispatchService::new(
        Arc::new(MemoryDispatchStore::new()) as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory.clone(), 30);
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-http"),
    );
    let registered = client
        .register("boot-http", WorkerManifest::default())
        .await
        .unwrap();
    // Registration cause/effect graph: C1 same Worker id; C2 incarnation differs;
    // C3 prior lease is live. R1 C1+C2+C3 => HTTP conflict classified as
    // SlotOccupied, preserving the old authority. Expired replacement and the
    // resulting generation advance are covered by the crash-recovery E2E.
    assert!(matches!(
        client
            .register_classified("boot-http-conflict", WorkerManifest::default())
            .await,
        Err(WorkerRegistrationError::SlotOccupied(_))
    ));
    assert_eq!(registered.snapshot.state, WorkerState::Starting);
    let identity = registered.snapshot.identity;

    clock.set(110);
    let observed = WorkerCredentialRevision {
        id: "cred:worker".into(),
        revision: 9,
    };
    let observation = WorkerCredentialObservation::available(observed, 110, 140);
    assert_eq!(
        client
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 1,
                    warm_environment_shapes: Default::default(),
                    credential_observations: std::collections::BTreeSet::from([
                        observation.clone(),
                    ]),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        directory
            .current("worker-http")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        WorkerState::Ready
    );
    assert_eq!(
        directory
            .current("worker-http")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .credential_observations,
        std::collections::BTreeSet::from([observation])
    );

    assert_eq!(
        client.begin_drain(&identity, Some(200)).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        client.mark_quiesced(&identity).await.unwrap(),
        RegistryMutation::InvalidTransition
    );
    client
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 2,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        client.mark_quiesced(&identity).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        client.deregister(&identity).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        directory
            .current("worker-http")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        WorkerState::Dead
    );
}

#[tokio::test]
async fn authenticated_header_cannot_register_a_different_worker_id() {
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let service = WorkerDispatchService::new(
        Arc::new(MemoryDispatchStore::new()) as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        Arc::new(ManualWorkerClock::new(0)),
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory, 30);
    let router = dispatch_transport_router_with_service(Arc::new(service));
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/worker/register")
        .header("content-type", "application/json")
        .header("x-awaken-worker-id", "authenticated")
        .body(Body::from(
            serde_json::json!({
                "registration": {
                    "worker_id": "impersonated",
                    "incarnation_id": "boot",
                    "manifest": WorkerManifest::default()
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

fn dispatch_with_capability(run: &str, capability: &str) -> RunDispatch {
    let activation = RunActivation::new(
        RunId(run.to_string()),
        ThreadId(format!("thread-{run}")),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("catalog".to_string()),
                instructions: String::new(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "native"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("catalog".to_string()),
        },
        vec![Message::text(
            MessageId("message".to_string()),
            Role::User,
            "run",
        )],
    );
    let mut placement = PlacementRequirements::remote_required();
    placement
        .required_capabilities
        .insert(capability.to_string());
    RunDispatch::new(activation).with_placement(placement)
}

#[tokio::test]
async fn registered_http_claim_skips_incompatible_work_and_uses_incarnation_owner() {
    let clock = Arc::new(ManualWorkerClock::new(100));
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let checkpoints = Arc::new(MemoryStreamCheckpointStore::new());
    dispatch
        .enqueue(dispatch_with_capability("gpu", "gpu"))
        .await
        .unwrap();
    dispatch
        .enqueue(dispatch_with_capability("cpu", "cpu"))
        .await
        .unwrap();
    let service = WorkerDispatchService::new(
        dispatch as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        clock,
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory, 1_000)
    .with_checkpoint_store(checkpoints.clone());
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let upstream = WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-cpu");
    let control = WorkerControlClient::new(upstream);
    let mut manifest = WorkerManifest::default();
    manifest.capabilities.insert("cpu".to_string());
    let registered = control.register("boot-cpu", manifest).await.unwrap();
    control
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .unwrap();

    let client = HttpDispatchQueue::new(
        format!("http://{address}"),
        registered.snapshot.identity.clone(),
    );
    let claimed = client
        .claim("ignored-local-owner", 99, 99, &Default::default())
        .await
        .unwrap()
        .expect("compatible work");
    assert_eq!(claimed.request.run_id().0, "cpu");
    assert_eq!(
        claimed.lease.owner,
        registered.snapshot.identity.lease_owner()
    );
    assert_eq!(
        claimed.assignment.unwrap().identity,
        registered.snapshot.identity
    );
    let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
    assert!(
        client
            .bind_sandbox(&claim, "sandbox-cpu")
            .await
            .unwrap()
            .applied()
    );
    let partial = StreamCheckpoint {
        run_id: claimed.lease.run_id.0.clone(),
        thread_id: "thread-cpu".to_string(),
        model: "model".to_string(),
        partial_text: "partial".to_string(),
        partial_tools: Vec::new(),
    };
    assert!(
        client
            .put_stream_checkpoint(&claim, partial.clone())
            .await
            .unwrap()
            .applied()
    );
    assert_eq!(
        client.load_stream_checkpoint(&claim).await.unwrap(),
        Some(partial)
    );
    assert_eq!(
        control
            .begin_drain(&registered.snapshot.identity, Some(1_000))
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    assert!(
        client
            .claim("ignored", 99, 99, &Default::default())
            .await
            .is_err(),
        "a draining incarnation cannot receive new work"
    );
    assert!(checkpoints.get(&claim.run_id.0).await.is_some());
}

#[tokio::test]
async fn http_claim_requires_the_exact_worker_private_credential_revision() {
    let clock = Arc::new(ManualWorkerClock::new(100));
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let required = WorkerCredentialRevision {
        id: "credential-source-worker-private".to_string(),
        revision: 12,
    };
    let mut placement = PlacementRequirements::remote_required();
    placement
        .required_capabilities
        .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    placement.required_credentials.insert(required.clone());
    dispatch
        .enqueue(
            dispatch_with_capability("worker-private", WORKER_LOCAL_CREDENTIALS_CAPABILITY)
                .with_placement(placement),
        )
        .await
        .unwrap();

    let service = WorkerDispatchService::new(
        dispatch as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory, 1_000);
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    async fn ready_worker(
        address: std::net::SocketAddr,
        worker_id: &str,
        credential: WorkerCredentialRevision,
    ) -> awaken_worker_registry::WorkerIdentity {
        let control = WorkerControlClient::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_id(worker_id),
        );
        let mut manifest = WorkerManifest::default();
        manifest
            .capabilities
            .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
        let registered = control
            .register(format!("boot-{worker_id}"), manifest)
            .await
            .unwrap();
        control
            .heartbeat(
                &registered.snapshot.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: [WorkerCredentialObservation::available(
                        credential, 100, 130,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap();
        registered.snapshot.identity
    }

    let wrong = ready_worker(
        address,
        "worker-wrong-revision",
        WorkerCredentialRevision {
            id: required.id.clone(),
            revision: required.revision - 1,
        },
    )
    .await;
    let wrong_queue = HttpDispatchQueue::new(format!("http://{address}"), wrong);
    assert!(
        wrong_queue
            .claim("ignored", 1_000, 100, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a nearby credential revision is not equivalent to the published revision"
    );

    let exact = ready_worker(address, "worker-exact-revision", required).await;
    let exact_queue = HttpDispatchQueue::new(format!("http://{address}"), exact.clone());
    clock.set(130);
    assert!(
        exact_queue
            .claim("ignored", 1_000, 130, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "an expired credential observation cannot claim while the worker lease is live"
    );
    let control = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-exact-revision"),
    );
    assert_eq!(
        control
            .heartbeat(
                &exact,
                WorkerHeartbeat {
                    sequence: 2,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: [WorkerCredentialObservation::available(
                        WorkerCredentialRevision {
                            id: "credential-source-worker-private".to_string(),
                            revision: 12,
                        },
                        130,
                        160,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    let claimed = exact_queue
        .claim("ignored", 1_000, 130, &Default::default())
        .await
        .unwrap()
        .expect("the worker reporting the exact revision can claim the run");
    assert_eq!(claimed.request.run_id().0, "worker-private");
    assert_eq!(claimed.assignment.unwrap().identity, exact);
}
