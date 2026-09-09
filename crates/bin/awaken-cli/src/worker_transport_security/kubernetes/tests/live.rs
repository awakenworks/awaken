//! Actual TokenReview + HTTP + reopened SQLite + independently assembled stop.
//! The shared k3d harness owns the isolated cluster; no fake provider receipts.
use super::*;
use awaken_dispatch::{
    AttemptAdmission, DispatchQueue, RunClaim, RunDispatch, SqliteDispatchStore, WorkerDirectory,
    WorkerHeartbeat, WorkerManifest,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_worker_registry::SqliteWorkerDirectory;
use awaken_coordinator_client::{WorkerControlClient, dispatch_transport_with_upstream};
use awaken_worker_transport_security::kubernetes::KubernetesWorkerRequestAuthorizer;
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, ManualWorkerClock, WorkerClock, WorkerUpstream,
};
use k8s_openapi::api::authentication::v1::TokenRequest;
use k8s_openapi::api::core::v1::{Namespace, Pod, ServiceAccount};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use std::time::Duration;

const TTL: u64 = 60_000;
const FINALIZER: &str = awaken_sandbox_container::k8s::WORKER_PROCESS_EVIDENCE_FINALIZER;

fn manifest() -> WorkerManifest {
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest
}

async fn token(accounts: &kube::Api<ServiceAccount>, pod: &Pod, audience: &str) -> String {
    let request: TokenRequest = serde_json::from_value(json!({"spec": {
        "audiences": [audience], "expirationSeconds": 600,
        "boundObjectRef": {"apiVersion":"v1", "kind":"Pod",
            "name": pod.metadata.name, "uid": pod.metadata.uid}
    }}))
    .unwrap();
    accounts
        .create_token_request("executor", &PostParams::default(), &request)
        .await
        .expect("real API-server-issued Pod-bound token")
        .status
        .unwrap()
        .token
}

fn upstream(address: &str, pod: &Pod, path: &std::path::Path) -> WorkerUpstream {
    let coordinate = KubernetesPodIdentity::new(
        pod.metadata.namespace.as_deref().unwrap(),
        pod.metadata.name.as_deref().unwrap(),
        pod.metadata.uid.as_deref().unwrap(),
    )
    .unwrap();
    // Loopback fixture transport only; production bootstrap's HTTPS requirement
    // is unchanged. The actual kube client still authenticates to the API server.
    WorkerUpstream::new(address)
        .with_worker_id(coordinate.worker_id())
        .with_request_authorizer(Arc::new(
            KubernetesWorkerRequestAuthorizer::new(coordinate, path.into()).unwrap(),
        ))
}

async fn ready(client: &WorkerControlClient, identity: &WorkerIdentity, sequence: u64) {
    let receipt = client
        .heartbeat(
            identity,
            WorkerHeartbeat {
                sequence,
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
        receipt.mutation,
        awaken_dispatch::RegistryMutation::Applied
    );
}

struct Server {
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("drain all HTTP connection tasks")
            .unwrap();
    }
}

async fn coordinator(
    files: &std::path::Path,
    transport: &WorkerTransport,
    clock: Arc<ManualWorkerClock>,
) -> (String, Arc<SqliteDispatchStore>, Server) {
    let store = Arc::new(
        // The test clock advances only while the first server is down. Freeze
        // the same phase time at both existing authority ports on each reopen.
        SqliteDispatchStore::open(files.join("dispatch.sqlite").to_str().unwrap())
            .unwrap()
            .with_clock(Arc::new(awaken_dispatch::ManualClock::new(
                clock.now_ms(),
            ))),
    );
    let directory = Arc::new(SqliteWorkerDirectory::open(files.join("registry.sqlite")).unwrap());
    let service = WorkerDispatchService::new(
        store.clone(),
        transport.authenticator(),
        clock,
        Arc::new(FixedWorkerLeasePolicy::new(TTL)),
    )
    .with_worker_directory(directory, TTL)
    .with_execution_binding_source(transport.binding_source().unwrap())
    .with_execution_stop_source(transport.stop_source().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    (address, store, Server { shutdown, task })
}

#[tokio::test]
async fn real_pod_identity_survives_reopen_and_independent_stop_without_false_retirement() {
    // Causes: C1 actual Pod-bound token / wrong audience / substituted UID;
    // C2 nonce retry / exact registry expiry; C3 binding committed before stop;
    // C4 Coordinator restart; C5 local exit, remote obligations still unknown.
    // Rules: L1 exact token => same immutable UID incarnation despite new nonce;
    // L2 wrong audience or UID => reject, zero new binding; L3 C3+C4 => exact
    // frozen binding survives database reopen; L4 expiry + current successor =>
    // stop OLD Pod using independent Coordinator credentials, no old token;
    // L5 C5 => original physical slot persists, successor still Blocked.
    // This covers real identity/retention/stop composition, NOT Native execution,
    // provider computation termination, or a complete crash takeover claim.
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping live Worker identity: AWAKEN_K8S_E2E=1 is required");
        return;
    }
    let cluster = std::env::var("AWAKEN_K3D_CLUSTER").expect("isolated k3d cluster required");
    let context = std::process::Command::new("kubectl")
        .args(["config", "current-context"])
        .output()
        .unwrap();
    assert!(context.status.success());
    assert_eq!(
        String::from_utf8(context.stdout).unwrap().trim(),
        format!("k3d-{cluster}")
    );
    let client = awaken_sandbox_container::k8s::connect_control_client()
        .await
        .unwrap();
    let namespaces: kube::Api<Namespace> = kube::Api::all(client.clone());
    let namespace = namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({
                "metadata":{"generateName":"worker-stop-"}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let name = namespace.metadata.name.as_deref().unwrap();
    let accounts: kube::Api<ServiceAccount> = kube::Api::namespaced(client.clone(), name);
    accounts
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({
                "metadata":{"name":"executor"}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let pods: kube::Api<Pod> = kube::Api::namespaced(client.clone(), name);
    let image = std::env::var("AWAKEN_K8S_FIXTURE_IMAGE").unwrap_or_else(|_| "awaken-bb:1".into());
    let mut created = Vec::new();
    for pod_name in ["old", "next"] {
        let pod = pods
            .create(
                &PostParams::default(),
                &serde_json::from_value(json!({
                    "metadata":{"name":pod_name,"finalizers":[FINALIZER]},
                    "spec":{"restartPolicy":"Never","serviceAccountName":"executor",
                        "automountServiceAccountToken":false,
                        "containers":[{"name":"worker","image":image,"imagePullPolicy":"Never",
                            "command":["sleep","600"],
                            "securityContext":{"allowPrivilegeEscalation":false,
                                "capabilities":{"drop":["ALL"]}}}]}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        created.push(pod);
    }
    let policy = crate::config::KubernetesWorkerTransport {
        namespace: name.into(),
        service_account: "executor".into(),
        audience: "awaken-worker".into(),
        provider_id: cluster,
    };
    let transport = assemble(&policy, client.clone()).unwrap();
    let bindings = transport.binding_source().unwrap();
    // Readiness uses the actual admissible process shape, not Pod phase alone.
    for pod in &created {
        let identity = WorkerIdentity::new(
            KubernetesPodIdentity::new(
                name,
                pod.metadata.name.as_deref().unwrap(),
                pod.metadata.uid.as_deref().unwrap(),
            )
            .unwrap()
            .worker_id(),
            pod.metadata.uid.as_deref().unwrap(),
            1,
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if bindings.binding_for(&identity).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("exact running retained Pod process");
    }
    let files = tempfile::tempdir().unwrap();
    let old_token = files.path().join("old-token");
    let next_token = files.path().join("next-token");
    let clock = Arc::new(ManualWorkerClock::new(100));
    let (address, store, server) = coordinator(files.path(), &transport, clock.clone()).await;
    std::fs::write(
        &old_token,
        token(&accounts, &created[0], "wrong-audience").await,
    )
    .unwrap();
    let old_upstream = upstream(&address, &created[0], &old_token);
    let control = WorkerControlClient::new(old_upstream.clone());
    assert!(
        control
            .register_classified("nonce-a", manifest())
            .await
            .is_err(),
        "L2 audience"
    );
    std::fs::write(
        &old_token,
        token(&accounts, &created[0], &policy.audience).await,
    )
    .unwrap();
    let worker = control
        .register_classified("nonce-a", manifest())
        .await
        .unwrap()
        .worker;
    let retry = control
        .register_classified("nonce-b", manifest())
        .await
        .unwrap()
        .worker;
    assert_eq!(worker, retry, "L1 immutable real-token identity");
    assert_eq!(
        worker.snapshot.identity.incarnation_id,
        *created[0].metadata.uid.as_ref().unwrap()
    );
    let old_upstream = old_upstream.with_worker_identity(worker.snapshot.identity.clone());
    ready(
        &WorkerControlClient::new(old_upstream.clone()),
        &worker.snapshot.identity,
        1,
    )
    .await;
    // Copying a genuine token does not grant another Pod's coordinate.
    let impostor = WorkerControlClient::new(upstream(&address, &created[1], &old_token));
    assert!(
        impostor
            .register_classified("copied-token", manifest())
            .await
            .is_err(),
        "L2 UID"
    );
    store
        .enqueue(
            RunDispatch::new(awaken_dispatch_testkit::worker_http::activation(
                "live-pod-stop",
            ))
            .with_placement(awaken_dispatch::PlacementRequirements::remote_required()),
        )
        .await
        .unwrap();
    let old_dispatch =
        dispatch_transport_with_upstream(&old_upstream, worker.snapshot.identity.clone());
    let claimed = old_dispatch
        .claim(
            &worker.snapshot.identity.lease_owner(),
            TTL,
            100,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let old_claim = RunClaim::from(&claimed.lease);
    assert_eq!(
        old_dispatch
            .begin_attempt(&old_claim, 100, None)
            .await
            .unwrap(),
        AttemptAdmission::Applied
    );
    let original = store
        .load_physical_attempt(&old_claim.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        original.binding.as_ref().unwrap().worker(),
        &worker.snapshot.identity
    );
    // L3 must drop the old authority, not just its listening task while pooled
    // HTTP connections retain the old service/store. Drain connections first.
    server.stop().await;
    assert_eq!(
        Arc::strong_count(&store),
        1,
        "L3 no retained HTTP store owner"
    );
    drop(store);
    drop(old_dispatch);
    drop(transport);
    drop(bindings);
    // Independent instance, reopened durable stores, no old token on disk.
    std::fs::remove_file(&old_token).unwrap();
    clock.set(100 + TTL + 1);
    let transport = assemble(
        &policy,
        awaken_sandbox_container::k8s::connect_control_client()
            .await
            .unwrap(),
    )
    .unwrap();
    let (address, store, server) = coordinator(files.path(), &transport, clock.clone()).await;
    assert_eq!(
        store
            .load_physical_attempt(&old_claim.run_id)
            .await
            .unwrap(),
        Some(original.clone()),
        "L3"
    );
    std::fs::write(
        &next_token,
        token(&accounts, &created[1], &policy.audience).await,
    )
    .unwrap();
    let next_upstream = upstream(&address, &created[1], &next_token);
    let successor = WorkerControlClient::new(next_upstream.clone())
        .register_classified("next-boot", manifest())
        .await
        .unwrap()
        .worker
        .snapshot
        .identity;
    let next_upstream = next_upstream.with_worker_identity(successor.clone());
    ready(
        &WorkerControlClient::new(next_upstream.clone()),
        &successor,
        1,
    )
    .await;
    let next_dispatch = dispatch_transport_with_upstream(&next_upstream, successor.clone());
    let next = RunClaim::from(
        &next_dispatch
            .claim(
                &successor.lease_owner(),
                TTL,
                100 + TTL + 1,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    assert!(next.epoch > old_claim.epoch);
    assert_eq!(
        next_dispatch
            .begin_attempt(&next, 100 + TTL + 1, None)
            .await
            .unwrap(),
        AttemptAdmission::Blocked
    );
    let directory = SqliteWorkerDirectory::open(files.path().join("registry.sqlite")).unwrap();
    assert_eq!(
        directory
            .current(&worker.snapshot.identity.worker_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        awaken_dispatch::WorkerState::Dead,
        "L4 authority closed before stop"
    );
    let stop = transport.stop_source().unwrap();
    let receipt = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(WorkerProcessStopStatus::Exited(receipt)) =
                awaken_dispatch::stop_expired_predecessor(
                    store.as_ref(),
                    &directory,
                    stop.as_ref(),
                    &next,
                    100 + TTL + 1,
                )
                .await
                .unwrap()
            {
                break receipt;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("independent exact local exit");
    assert_eq!(receipt.binding(), original.binding.as_ref().unwrap());
    assert!(!receipt.evidence_ref().is_empty());
    assert_eq!(
        store
            .load_physical_attempt(&old_claim.run_id)
            .await
            .unwrap(),
        Some(original),
        "L5"
    );
    assert_eq!(
        next_dispatch
            .begin_attempt(&next, 100 + TTL + 1, None)
            .await
            .unwrap(),
        AttemptAdmission::Blocked,
        "L5"
    );
    assert!(
        pods.get("next")
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "L4 never stop successor"
    );
    server.stop().await;
    // Only these test-owned retention markers are released. The enclosing
    // cluster harness is also the cleanup authority if any assertion panics.
    for pod in created {
        let pod_name = pod.metadata.name.unwrap();
        let current = pods.get(&pod_name).await.unwrap();
        assert_eq!(current.metadata.uid, pod.metadata.uid);
        let retained: Vec<_> = current
            .metadata
            .finalizers
            .unwrap_or_default()
            .into_iter()
            .filter(|value| value != FINALIZER)
            .collect();
        pods.patch(
            &pod_name,
            &PatchParams::default(),
            &Patch::Merge(json!({"metadata":{
                "resourceVersion":current.metadata.resource_version,"finalizers":retained
            }})),
        )
        .await
        .unwrap();
    }
    namespaces
        .delete(
            name,
            &DeleteParams::default().preconditions(kube::api::Preconditions {
                uid: namespace.metadata.uid,
                resource_version: None,
            }),
        )
        .await
        .unwrap();
}
