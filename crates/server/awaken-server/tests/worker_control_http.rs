use std::sync::Arc;

use awaken_run_ingress::{DispatchQueue, MemoryDispatchStore};
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
