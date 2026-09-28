// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
//! Pod authentication through the production registration/client path. The API
//! review is a fixture; Registry persistence and HTTP are real, not k3d evidence.

mod postgres_support;

use std::sync::Arc;

use awaken_dispatch::{
    MemoryDispatchStore, WorkerDirectory, WorkerManifest, WorkerRegistration, WorkerState,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory,
};
use awaken_coordinator_client::WorkerControlClient;
use awaken_worker_transport_security::kubernetes::{
    KubernetesPodIdentity, KubernetesWorkerAuthenticator, KubernetesWorkerRequestAuthorizer,
};
use awaken_worker_transport_security::{FixedWorkerLeasePolicy, ManualWorkerClock, WorkerUpstream};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

fn manifest() -> WorkerManifest {
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest
}

struct Fixture {
    address: String,
    directory: Arc<dyn WorkerDirectory>,
    clock: Arc<ManualWorkerClock>,
    files: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(directory: Arc<dyn WorkerDirectory>) -> Self {
        async fn review(Json(body): Json<Value>) -> Json<Value> {
            let uid = body["spec"]["token"].as_str().unwrap();
            assert!(uid.starts_with("uid-"));
            Json(
                json!({"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","spec":{},"status":{
                    "authenticated":true,"audiences":["awaken-worker"],"user":{
                        "username":"system:serviceaccount:workers:executor","uid":"sa-fixture","extra":{
                            "authentication.kubernetes.io/pod-name":["worker-a"],
                            "authentication.kubernetes.io/pod-uid":[uid]
                        }
                    }
                }}),
            )
        }
        let api = Router::new().route("/apis/authentication.k8s.io/v1/tokenreviews", post(review));
        let auth = KubernetesWorkerAuthenticator::new(
            kube::Client::new(api, "workers"),
            "workers",
            "executor",
            "awaken-worker",
            std::time::Duration::from_secs(2),
        )
        .unwrap();
        let clock = Arc::new(ManualWorkerClock::new(100));
        let service = WorkerDispatchService::new(
            Arc::new(MemoryDispatchStore::new(
                awaken_lease_contract::system_clock(),
            )),
            Arc::new(auth),
            clock.clone(),
            Arc::new(FixedWorkerLeasePolicy::new(50)),
        )
        .with_worker_directory(directory.clone(), 50);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let router = dispatch_transport_router_with_service(Arc::new(service));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            address,
            directory,
            clock,
            files: tempfile::tempdir().unwrap(),
            server,
        }
    }

    fn client(&self, uid: &str) -> WorkerControlClient {
        WorkerControlClient::new(self.upstream(uid))
    }

    fn upstream(&self, uid: &str) -> WorkerUpstream {
        let pod = KubernetesPodIdentity::new("workers", "worker-a", uid).unwrap();
        let token = self.files.path().join(uid);
        std::fs::write(&token, uid).unwrap();
        WorkerUpstream::new(&self.address)
            .with_worker_id(pod.worker_id())
            .with_request_authorizer(Arc::new(
                KubernetesWorkerRequestAuthorizer::new(pod, token).unwrap(),
            ))
    }
}

async fn assert_immutable_registration(directory: Arc<dyn WorkerDirectory>, backend: &str) {
    // Decision table: P1 first and concurrent same-Pod registrations with
    // different process nonces => one UID incarnation, one generation. P2
    // response-loss retry after expiry/closure => exact record, no refresh or
    // replacement. P3 fresh Pod UID at same name => independent generation 1.
    // P4 historical arbitrary incarnation (live OR expired) => reject unchanged,
    // never backfill provider authority. P5 changed manifest => existing reject.
    // Registry serializes P1/P2: every authenticated writer chooses the same
    // UID, and same-incarnation register is a stutter in all states. This proves
    // registration behavior, not Pod confinement or termination of remote work.
    let f = Fixture::new(directory).await;
    let uid = format!("uid-{backend}-{}", std::process::id());
    let a = f.client(&uid);
    let b = f.client(&uid);
    let (first, retry) = tokio::join!(
        a.register_classified("random-a", manifest()),
        b.register_classified("random-b", manifest())
    );
    let first = first.unwrap().worker;
    assert_eq!(retry.unwrap().worker, first, "P1");
    assert_eq!(first.snapshot.identity.incarnation_id, uid);
    assert_eq!(first.snapshot.identity.generation, 1);
    let mut changed = manifest();
    changed.build_digest = "changed".into();
    assert!(
        a.register_classified("random-c", changed).await.is_err(),
        "P5"
    );
    for close in [false, true] {
        f.clock.set(151);
        if close {
            assert_eq!(
                f.directory
                    .expire_incarnation(&first.snapshot.identity, 151)
                    .await
                    .unwrap(),
                awaken_dispatch::RegistryMutation::Applied
            );
        }
        let before = f
            .directory
            .current(&first.snapshot.identity.worker_id)
            .await
            .unwrap()
            .unwrap();
        let retry = a
            .register_classified("another-random-boot", manifest())
            .await
            .unwrap()
            .worker;
        assert_eq!(retry, before, "P2");
        assert_eq!(retry.snapshot.identity.generation, 1);
        assert_eq!(
            retry.snapshot.state,
            if close {
                WorkerState::Dead
            } else {
                WorkerState::Starting
            }
        );
        assert_eq!(retry.snapshot.expires_at_ms, 150, "P2 no renewal");
        let bound = WorkerControlClient::new(
            f.upstream(&uid)
                .with_worker_identity(retry.snapshot.identity.clone()),
        );
        let heartbeat = bound
            .heartbeat(
                &retry.snapshot.identity,
                awaken_dispatch::WorkerHeartbeat {
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
        assert_eq!(
            heartbeat.mutation,
            // Expiry is checked before lifecycle state in the canonical
            // heartbeat admission table, for both Starting and Dead records.
            awaken_dispatch::RegistryMutation::StaleIncarnation,
            "P2 returned identity cannot renew closed/expired authority"
        );
        assert_eq!(heartbeat.lease_ttl_ms, None);
    }
    let successor_uid = format!("{uid}-next");
    let next = f
        .client(&successor_uid)
        .register_classified("random-next", manifest())
        .await
        .unwrap()
        .worker;
    assert_eq!(next.snapshot.identity.incarnation_id, successor_uid, "P3");
    assert_eq!(next.snapshot.identity.generation, 1);
    assert_ne!(
        next.snapshot.identity.worker_id,
        first.snapshot.identity.worker_id
    );
    for now in [100, 151] {
        let legacy_uid = format!("{uid}-legacy-{now}");
        let id = KubernetesPodIdentity::new("workers", "worker-a", &legacy_uid)
            .unwrap()
            .worker_id();
        let historical = f
            .directory
            .register(
                WorkerRegistration {
                    worker_id: id.clone(),
                    incarnation_id: "historical-boot".into(),
                    manifest: manifest(),
                },
                100,
                50,
            )
            .await
            .unwrap();
        f.clock.set(now);
        assert!(
            f.client(&legacy_uid)
                .register_classified("new-boot", manifest())
                .await
                .is_err(),
            "P4"
        );
        assert_eq!(
            f.directory.current(&id).await.unwrap(),
            Some(historical),
            "P4 no mutation"
        );
    }
}

#[tokio::test]
async fn immutable_pod_registration_memory() {
    assert_immutable_registration(Arc::new(MemoryWorkerDirectory::new()), "memory").await;
}

#[tokio::test]
async fn immutable_pod_registration_sqlite() {
    let temp = tempfile::tempdir().unwrap();
    assert_immutable_registration(
        Arc::new(SqliteWorkerDirectory::open(temp.path().join("registry.sqlite")).unwrap()),
        "sqlite",
    )
    .await;
}

#[tokio::test]
async fn immutable_pod_registration_postgres_when_configured() {
    let Some(url) = postgres_support::database_url() else {
        return;
    };
    assert_immutable_registration(
        Arc::new(postgres_support::connect(PostgresWorkerDirectory::connect(&url, 5)).await),
        "postgres",
    )
    .await;
}

#[test]
fn postgres_configuration_contract() {
    postgres_support::assert_configuration_contract(&[
        "immutable_pod_registration_postgres_when_configured",
    ]);
}
