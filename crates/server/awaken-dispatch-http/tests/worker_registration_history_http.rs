// @formal evidence worker.incarnation_generation_fences_stale_processes
//! Historical registration through the actual signed client, HTTP handler and
//! Registry adapters. The signer is re-run per request: these are valid
//! application retries, not an attempt to bypass the signature nonce cache.
mod postgres_support;

use awaken_lease_testkit::ManualClock;
use awaken_dispatch::{MemoryDispatchStore, WorkerDirectory, WorkerManifest};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory,
};
use awaken_coordinator_client::WorkerControlClient;
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, SignedWorkerAuthenticator, SignedWorkerRequestAuthorizer,
    WorkerSigningCredential, WorkerUpstream,
};
use std::sync::Arc;

async fn assert_registration_history(directory: Arc<dyn WorkerDirectory>, worker_id: &str) {
    // Cause/effect decision table: T1 valid bootstrap A -> generation 1;
    // T2 A expires, valid B -> generation 2; T3 B expires, fresh signed A retry
    // -> HTTP rejection + unchanged B; T4 same B retry -> exact B, no extension;
    // T5 fresh C -> generation 3. The independent domain table owns all Worker
    // lifecycle states; this table checks production client/route composition.
    // Kubernetes immutable-domain registration remains in its existing suite.
    let clock = Arc::new(ManualClock::new(100));
    let credential =
        WorkerSigningCredential::new(worker_id, "key", "fixture", b"history-test-only".to_vec())
            .unwrap();
    let auth = SignedWorkerAuthenticator::new(credential.clone(), clock.clone())
        .with_time_policy(60_000, 0);
    let service = WorkerDispatchService::new(
        Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
        Arc::new(auth),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(50)),
    )
    .with_worker_directory(directory.clone(), 50);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            dispatch_transport_router_with_service(Arc::new(service)),
        )
        .await
        .unwrap();
    });
    // Abort-on-drop also closes the fixture on assertion failure.
    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = Server(server);
    let upstream = WorkerUpstream::new(format!("http://{address}"))
        .with_worker_id(worker_id)
        .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(
            credential,
            clock.clone(),
        )));
    let client = WorkerControlClient::new(upstream);
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    let a = client
        .register_classified("A", manifest.clone())
        .await
        .unwrap()
        .worker;
    assert_eq!(a.snapshot.identity.generation, 1, "T1");
    clock.set(150);
    let b = client
        .register_classified("B", manifest.clone())
        .await
        .unwrap()
        .worker;
    assert_eq!(b.snapshot.identity.generation, 2, "T2");
    clock.set(200);
    let error = client
        .register_classified("A", manifest.clone())
        .await
        .expect_err("T3 historical registration fails");
    assert!(
        error.to_string().contains("historical incarnation"),
        "T3 specific authority rejection: {error}"
    );
    assert_eq!(
        directory.current(worker_id).await.unwrap(),
        Some(b.clone()),
        "T3 no write"
    );
    assert_eq!(
        client
            .register_classified("B", manifest.clone())
            .await
            .unwrap()
            .worker,
        b,
        "T4"
    );
    assert_eq!(
        client
            .register_classified("C", manifest)
            .await
            .unwrap()
            .worker
            .snapshot
            .identity
            .generation,
        3,
        "T5"
    );
}

#[tokio::test]
async fn memory_signed_registration_rejects_historical_incarnation() {
    assert_registration_history(
        Arc::new(MemoryWorkerDirectory::new()),
        "history-http-memory",
    )
    .await;
}

#[tokio::test]
async fn sqlite_signed_registration_rejects_historical_incarnation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.sqlite");
    assert_registration_history(
        Arc::new(SqliteWorkerDirectory::open(path).await.unwrap()),
        "history-http-sqlite",
    )
    .await;
}

#[tokio::test]
async fn postgres_signed_registration_rejects_historical_incarnation() {
    let Some(url) = postgres_support::database_url() else {
        return;
    };
    let directory = postgres_support::connect(PostgresWorkerDirectory::connect(&url, 3)).await;
    assert_registration_history(
        Arc::new(directory),
        &format!("history-http-pg-{}", std::process::id()),
    )
    .await;
}

#[test]
fn postgres_configuration_contract() {
    postgres_support::assert_configuration_contract(&[
        "postgres_signed_registration_rejects_historical_incarnation",
    ]);
}
