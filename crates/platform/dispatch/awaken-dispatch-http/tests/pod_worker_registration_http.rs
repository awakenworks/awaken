// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
//! Pod authentication through the production registration/client path. The API
//! review and the Pod's observed container are fixtures; Registry persistence
//! and HTTP are real, not k3d evidence.

mod postgres_support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_coordinator_client::WorkerControlClient;
use awaken_dispatch::{
    AttemptExecutionBinding, MemoryDispatchStore, WorkerDirectory, WorkerExecutionBindingSource,
    WorkerIdentity, WorkerManifest, WorkerRegistration, WorkerState,
};
use awaken_dispatch_http::{
    RegisteredDispatchDependencies, WorkerDispatchService, dispatch_transport_router_with_service,
};
use awaken_dispatch_testkit::worker_dispatch::worker_dispatch_dependencies;
use awaken_lease_testkit::ManualClock;
use awaken_session_execution_contract::ApplicationError;
use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory,
};
use awaken_worker_transport_security::kubernetes::{
    KubernetesPodIdentity, KubernetesWorkerAuthenticator, KubernetesWorkerRequestAuthorizer,
};
use awaken_worker_transport_security::{FixedWorkerLeasePolicy, WorkerUpstream};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

fn manifest() -> WorkerManifest {
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest
}

/// The Coordinator's view of each authenticated Pod's running container, as
/// the Kubernetes binding source reads it for that Pod UID and container name.
/// A Pod with no observed container yields no provider evidence.
#[derive(Default)]
struct PodContainers(Mutex<HashMap<String, String>>);

impl PodContainers {
    fn observe(&self, pod_uid: &str, container: &str) {
        self.0
            .lock()
            .unwrap()
            .insert(pod_uid.to_owned(), container.to_owned());
    }
}

#[async_trait::async_trait]
impl WorkerExecutionBindingSource for PodContainers {
    async fn registration_incarnation(
        &self,
        registration: &WorkerRegistration,
    ) -> Result<Option<String>, ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&registration.worker_id)
            .map_err(ApplicationError::invalid)?;
        Ok(self.0.lock().unwrap().get(pod.uid()).cloned())
    }

    async fn binding_for(
        &self,
        _: &WorkerIdentity,
    ) -> Result<AttemptExecutionBinding, ApplicationError> {
        Err(ApplicationError::unavailable(
            "registration fixture binds no attempts",
        ))
    }
}

struct Fixture {
    address: String,
    directory: Arc<dyn WorkerDirectory>,
    clock: Arc<ManualClock>,
    files: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(
        directory: Arc<dyn WorkerDirectory>,
        containers: Option<Arc<PodContainers>>,
    ) -> Self {
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
        let clock = Arc::new(ManualClock::new(100));
        let service = WorkerDispatchService::new(RegisteredDispatchDependencies {
            authenticator: Arc::new(auth),
            lease_policy: Arc::new(FixedWorkerLeasePolicy::new(50)),
            registry_ttl_ms: 50,
            execution_bindings: containers.map(|containers| {
                containers as Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>
            }),
            ..worker_dispatch_dependencies(
                Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
                directory.clone(),
                clock.clone(),
            )
        });
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
    // Decision table (the Worker ID is the Pod; an incarnation is one running
    // container instance the Coordinator observes for that Pod UID):
    // P1 first and concurrent same-Pod registrations with different process
    // nonces => the one observed container incarnation, one generation.
    // P2 response-loss retry after expiry/closure => exact record, no refresh
    // or replacement. P3 fresh Pod UID at same name => its own container,
    // independent generation 1. P4 a foreign incarnation holding a live slot,
    // or a historical container instance observed again => reject unchanged,
    // never backfill provider authority. P5 changed manifest => existing
    // reject. P6 a process-proposed incarnation without binding-source
    // evidence (no binding source, or no observed container) => reject, no
    // Registry record. P7 restarted container after the previous lease lapsed
    // => the newly observed container as the next generation.
    // Registry serializes P1/P2: every authenticated writer receives the same
    // observed container, and same-incarnation register is a stutter in all
    // states. This proves registration behavior, not Pod confinement or
    // termination of remote work.
    let containers = Arc::new(PodContainers::default());
    let f = Fixture::new(directory.clone(), Some(containers.clone())).await;
    let uid = format!("uid-{backend}-{}", std::process::id());
    let running = "0f3a9c01";
    containers.observe(&uid, running);
    let a = f.client(&uid);
    let b = f.client(&uid);
    let (first, retry) = tokio::join!(
        a.register_classified("random-a", manifest()),
        b.register_classified("random-b", manifest())
    );
    let first = first.unwrap().worker;
    assert_eq!(retry.unwrap().worker, first, "P1");
    assert_eq!(first.snapshot.identity.incarnation_id, running, "P1");
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
                    source_retention: Default::default(),
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
    containers.observe(&successor_uid, "0f3a9c02");
    let next = f
        .client(&successor_uid)
        .register_classified("random-next", manifest())
        .await
        .unwrap()
        .worker;
    assert_eq!(next.snapshot.identity.incarnation_id, "0f3a9c02", "P3");
    assert_eq!(next.snapshot.identity.generation, 1);
    assert_ne!(
        next.snapshot.identity.worker_id,
        first.snapshot.identity.worker_id
    );

    let restart_uid = format!("{uid}-restart");
    let restart_id = KubernetesPodIdentity::new("workers", "worker-a", &restart_uid)
        .unwrap()
        .worker_id();
    let foreign = f
        .directory
        .register(
            WorkerRegistration {
                worker_id: restart_id.clone(),
                incarnation_id: "0f3a9c10".into(),
                manifest: manifest(),
                create_ledger: None,
            },
            100,
            50,
        )
        .await
        .unwrap();
    containers.observe(&restart_uid, "0f3a9c11");
    f.clock.set(100);
    assert!(
        f.client(&restart_uid)
            .register_classified("0f3a9c11", manifest())
            .await
            .is_err(),
        "P4 live foreign incarnation"
    );
    assert_eq!(
        f.directory.current(&restart_id).await.unwrap(),
        Some(foreign),
        "P4 no mutation"
    );
    f.clock.set(151);
    let restarted = f
        .client(&restart_uid)
        .register_classified("new-boot", manifest())
        .await
        .unwrap()
        .worker;
    assert_eq!(restarted.snapshot.identity.incarnation_id, "0f3a9c11", "P7");
    assert_eq!(restarted.snapshot.identity.generation, 2, "P7");
    containers.observe(&restart_uid, "0f3a9c10");
    f.clock.set(300);
    assert!(
        f.client(&restart_uid)
            .register_classified("0f3a9c10", manifest())
            .await
            .is_err(),
        "P4 historical container instance"
    );
    assert_eq!(
        f.directory.current(&restart_id).await.unwrap(),
        Some(restarted),
        "P4 no mutation"
    );

    let unobserved_uid = format!("{uid}-unobserved");
    let unobserved_id = KubernetesPodIdentity::new("workers", "worker-a", &unobserved_uid)
        .unwrap()
        .worker_id();
    assert!(
        f.client(&unobserved_uid)
            .register_classified("0f3a9c20", manifest())
            .await
            .is_err(),
        "P6 no observed container"
    );
    let unbound = Fixture::new(directory, None).await;
    assert!(
        unbound
            .client(&unobserved_uid)
            .register_classified("0f3a9c20", manifest())
            .await
            .is_err(),
        "P6 no binding source"
    );
    assert_eq!(
        f.directory.current(&unobserved_id).await.unwrap(),
        None,
        "P6 no Registry record"
    );
}

#[tokio::test]
async fn immutable_pod_registration_memory() {
    assert_immutable_registration(Arc::new(MemoryWorkerDirectory::new()), "memory").await;
}

#[tokio::test]
async fn immutable_pod_registration_sqlite() {
    let temp = tempfile::tempdir().unwrap();
    assert_immutable_registration(
        Arc::new(
            SqliteWorkerDirectory::open(temp.path().join("registry.sqlite"))
                .await
                .unwrap(),
        ),
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
