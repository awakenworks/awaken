use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_run_ingress::{
    DispatchQueue, HttpDispatchQueue, MemoryDispatchStore, PlacementRequirements, RunDispatch,
};
use awaken_runtime::memory::MemoryStreamCheckpointStore;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock, WorkerControlClient,
    WorkerDispatchService, WorkerUpstream, dispatch_transport_router_with_service,
};
use awaken_worker_registry::{
    MemoryWorkerDirectory, RegistryMutation, WorkerDirectory, WorkerHeartbeat, WorkerManifest,
    WorkerState,
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
    assert_eq!(registered.snapshot.state, WorkerState::Starting);
    let identity = registered.snapshot.identity;

    clock.set(110);
    assert_eq!(
        client
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 1,
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
            root_agent_id: AgentId("agent".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("catalog".to_string()),
                instructions: String::new(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: ModelBinding::new("provider", "model", "native"),
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
            },
        )
        .await
        .unwrap();

    let client = HttpDispatchQueue::new(format!("http://{address}"))
        .with_worker_identity(registered.snapshot.identity.clone());
    let claimed = client
        .claim("ignored-local-owner", 99, 99)
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
        client.claim("ignored", 99, 99).await.is_err(),
        "a draining incarnation cannot receive new work"
    );
    assert!(checkpoints.get(&claim.run_id.0).await.is_some());
}
