// @formal evidence model_operation.coverage_commit_strictly_precedes_model_entry
// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
//! Actual TokenReview + HTTP + reopened SQLite + independently assembled stop.
//! The shared k3d harness owns the isolated cluster; no fake provider receipts.
use super::*;
use awaken_container_contract::K8sCreateRequest;
use awaken_container_contract::{K8sLedgerBinding, RequestJoin};
use awaken_lease_testkit::ManualClock;
use awaken_worker_contract::{WorkerCurrentSource, WorkerIncarnationSource};

use awaken_dispatch::{
    AttemptAdmission, RunClaim, RunDispatch, SqliteDispatchStore, WorkerHeartbeat,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_sandbox_kubernetes::{K8sCreateTransport, K8sWorkerCreateLedger, PodProcessObserver};
use awaken_store_sqlite::SqliteCommitCoordinator;
use awaken_time::WallClockExt;
use awaken_worker::WorkerK8sCreateClient;
use awaken_worker_registry::SqliteWorkerDirectory;
use awaken_coordinator_client::{WorkerControlClient, dispatch_transport_with_upstream};
use awaken_worker_transport_security::kubernetes::KubernetesWorkerRequestAuthorizer;
use awaken_worker_transport_security::{FixedWorkerLeasePolicy, WorkerUpstream};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::authentication::v1::TokenRequest;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Pod, ServiceAccount};
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use std::time::Duration;

const TTL: u64 = 60_000;
const FINALIZER: &str = awaken_sandbox_kubernetes::WORKER_PROCESS_EVIDENCE_FINALIZER;

async fn ready_deployment_pod(pods: &kube::Api<Pod>, previous_uid: Option<&str>) -> Pod {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let observed = pods
                .list(&ListParams::default().labels("app=closed-idle-worker"))
                .await
                .unwrap();
            if let Some(pod) = observed.items.into_iter().find(|pod| {
                pod.metadata.deletion_timestamp.is_none()
                    && pod.metadata.uid.as_deref() != previous_uid
                    && pod
                        .status
                        .as_ref()
                        .and_then(|s| s.container_statuses.as_ref())
                        .is_some_and(|statuses| {
                            !statuses.is_empty() && statuses.iter().all(|s| s.ready)
                        })
            }) {
                return pod;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("actual Ready Deployment Pod with a fresh UID")
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
    clock: Arc<ManualClock>,
) -> (String, Arc<SqliteDispatchStore>, Server) {
    let store = Arc::new(
        // The test clock advances only while the first server is down. Freeze
        // the same phase time at both existing authority ports on each reopen.
        SqliteDispatchStore::open(
            files.join("dispatch.sqlite").to_str().unwrap(),
            Arc::new(awaken_lease_testkit::ManualClock::new(clock.now_ms())),
        )
        .await
        .unwrap(),
    );
    let directory = Arc::new(
        SqliteWorkerDirectory::open(files.join("registry.sqlite"))
            .await
            .unwrap(),
    );
    let create_broker = transport.create_broker_router(directory.clone(), clock.clone());
    let recovery = Arc::new(
        SqliteCommitCoordinator::open(files.join("runtime.sqlite").to_str().unwrap())
            .await
            .unwrap(),
    );
    let service = WorkerDispatchService::new(
        store.clone(),
        transport.authenticator(),
        clock,
        Arc::new(FixedWorkerLeasePolicy::new(TTL)),
    )
    .with_worker_directory(directory, TTL)
    .with_recovery_source(recovery)
    .with_execution_binding_source(transport.binding_source().unwrap())
    .with_execution_stop_source(transport.stop_source().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let router = match create_broker {
        Some(broker) => router.merge(broker),
        None => router,
    };
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
async fn real_pod_identity_survives_reopen_and_complete_evidence_retires_exact_attempt() {
    // Causes: C1 actual Pod-bound token / wrong audience / substituted UID;
    // C2 nonce retry / exact registry expiry; C3 binding committed before stop;
    // C4 Coordinator restart; C5 local exit plus a persisted model-only coverage
    // barrier and terminal model operation; C6 exact successor retry.
    // Rules: L1 exact token => same immutable UID incarnation despite new nonce;
    // L2 wrong audience or UID => reject, zero new binding; L3 C3+C4 => exact
    // frozen binding survives database reopen; L4 expiry + current successor =>
    // stop OLD Pod using independent Coordinator credentials, no old token;
    // L5 C5 => the original physical slot is retired through the canonical exact
    // finish transition; L6 C6 => successor enters only on a fresh admission.
    // This covers real identity/retention/stop/retirement composition, NOT an
    // actual Native Worker process crash; the distributed k3d scenario owns that.
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
    let client = awaken_sandbox_kubernetes::connect_control_client()
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
    let control_namespace = namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({
                "metadata":{"generateName":"worker-create-control-"}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let control_name = control_namespace.metadata.name.as_deref().unwrap();
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
    for pod_name in ["old", "next", "idle"] {
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
    let audience = "awaken-worker";
    let transport = WorkerTransport::kubernetes(
        client.clone(),
        name,
        "executor",
        audience,
        &cluster,
        control_name,
    )
    .unwrap();
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
    let idle_token = files.path().join("idle-token");
    let clock = Arc::new(ManualClock::new(100));
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
    std::fs::write(&old_token, token(&accounts, &created[0], audience).await).unwrap();
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
    // L1a actual Pod UID + successful provider Create -> the Registry freezes
    // one ledger UID before Ready. L3a Coordinator restart and old Pod loss ->
    // that same historical UID remains reobservable in both owners; a stable
    // ConfigMap name alone would not satisfy either comparison.
    let ledger = worker
        .create_ledger
        .clone()
        .expect("Kubernetes registration ledger");
    assert_eq!(
        ledger.namespace(),
        control_name,
        "L1a separate control domain"
    );
    assert_eq!(ledger.provider_id(), cluster, "L1a exact provider");
    let ledgers: kube::Api<ConfigMap> = kube::Api::namespaced(client.clone(), control_name);
    assert_eq!(
        ledgers
            .get(ledger.name())
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(ledger.uid()),
        "L1a exact API UID"
    );
    let old_upstream = old_upstream.with_worker_identity(worker.snapshot.identity.clone());
    ready(
        &WorkerControlClient::new(old_upstream.clone()),
        &worker.snapshot.identity,
        1,
    )
    .await;
    // C7 the executor ServiceAccount cannot Create directly, but its exact
    // registered, signed Worker request reaches the independent broker. E7
    // the first API UID is retained and a response-loss replay becomes 409 for
    // the existing typed GET verifier, with no second Kubernetes object.
    let denied = std::process::Command::new("kubectl")
        .args([
            "auth",
            "can-i",
            "create",
            "configmaps",
            "--as",
            &format!("system:serviceaccount:{name}:executor"),
            "-n",
            name,
            "--quiet",
        ])
        .status()
        .unwrap();
    assert!(
        !denied.success(),
        "C7 Worker Pod has no direct ConfigMap Create"
    );
    let create_client = WorkerK8sCreateClient::new(old_upstream.clone());
    create_client
        .bind_identity(worker.snapshot.identity.clone())
        .unwrap();
    let request = K8sCreateRequest {
        path: format!("/api/v1/namespaces/{name}/configmaps"),
        object: json!({
            "apiVersion":"v1", "kind":"ConfigMap",
            "metadata":{
                "name":"broker-probe", "namespace":name,
                "annotations":{"awaken.dev/realization-digest":"a".repeat(64)}
            },
            "data":{"probe":"ok"}
        }),
    };
    let first_create = create_client.create(request.clone()).await.unwrap();
    assert_eq!(
        first_create.status, 201,
        "E7 independent API Create succeeded"
    );
    let first_uid = first_create.object["metadata"]["uid"]
        .as_str()
        .unwrap()
        .to_owned();
    let first_version = first_create.object["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .to_owned();
    let cms: kube::Api<ConfigMap> = kube::Api::namespaced(client.clone(), name);
    assert_eq!(
        cms.get("broker-probe")
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(first_uid.as_str()),
        "E7 exact API readback"
    );
    let replay = create_client.create(request).await.unwrap();
    assert_eq!(replay.status, 200, "E7 exact admitted Create readback");
    assert_eq!(
        replay.object["metadata"]["uid"], first_uid,
        "E7 same object"
    );
    assert_eq!(
        replay.object["metadata"]["resourceVersion"], first_version,
        "E7 readback does not mutate the original object"
    );
    assert_eq!(
        cms.get("broker-probe")
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(first_uid.as_str()),
        "E7 no replacement UID"
    );
    cms.delete(
        "broker-probe",
        &DeleteParams::default().preconditions(kube::api::Preconditions {
            uid: Some(first_uid),
            resource_version: None,
        }),
    )
    .await
    .unwrap();
    // Copying a genuine token does not grant another Pod's coordinate.
    let impostor = WorkerControlClient::new(upstream(&address, &created[1], &old_token));
    assert!(
        impostor
            .register_classified("copied-token", manifest())
            .await
            .is_err(),
        "L2 UID"
    );
    let activation = model_only_activation("live-pod-stop");
    let model_activation = activation.clone();
    store
        .enqueue(
            RunDispatch::new(activation)
                .with_placement(awaken_dispatch::PlacementRequirements::remote_required()),
        )
        .await
        .unwrap();
    let old_dispatch =
        dispatch_transport_with_upstream(&old_upstream, worker.snapshot.identity.clone());
    clock.set(100);
    let claimed = old_dispatch
        .claim(
            &worker.snapshot.identity.lease_owner(),
            TTL,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let old_claim = RunClaim::from(&claimed.lease);
    clock.set(100);
    assert_eq!(
        old_dispatch.begin_attempt(&old_claim, None).await.unwrap(),
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
    let recovery = Arc::new(
        SqliteCommitCoordinator::open(files.path().join("runtime.sqlite").to_str().unwrap())
            .await
            .unwrap(),
    );
    commit_actual_model_only_result(recovery.clone(), model_activation).await;
    drop(recovery);
    // C10 a second Worker never owns a Dispatch slot. Register it just before
    // the old Worker's deadline, so the old attempt expires during restart
    // while this independent idle registration retains its original lease.
    // Its process binding is retained by the old Coordinator before Ready;
    // only the reopened Coordinator may observe and commit its later exit.
    clock.set(100 + TTL - 1);
    std::fs::write(&idle_token, token(&accounts, &created[2], audience).await).unwrap();
    let idle_upstream = upstream(&address, &created[2], &idle_token);
    let idle_identity = WorkerControlClient::new(idle_upstream)
        .register_classified("idle-boot", manifest())
        .await
        .unwrap()
        .worker
        .snapshot
        .identity;
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
    let transport = WorkerTransport::kubernetes(
        awaken_sandbox_kubernetes::connect_control_client()
            .await
            .unwrap(),
        name,
        "executor",
        audience,
        &cluster,
        control_name,
    )
    .unwrap();
    let (address, store, server) = coordinator(files.path(), &transport, clock.clone()).await;
    let reopened = SqliteWorkerDirectory::open(files.path().join("registry.sqlite"))
        .await
        .unwrap();
    assert_eq!(
        reopened
            .incarnation(&worker.snapshot.identity)
            .await
            .unwrap()
            .unwrap()
            .create_ledger,
        Some(ledger.clone()),
        "L3a historical Registry anchor survives Coordinator restart"
    );
    assert_eq!(
        store
            .load_physical_attempt(&old_claim.run_id)
            .await
            .unwrap(),
        Some(original.clone()),
        "L3"
    );
    std::fs::write(&next_token, token(&accounts, &created[1], audience).await).unwrap();
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
            .claim(&successor.lease_owner(), TTL, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    assert!(next.epoch > old_claim.epoch);
    assert_eq!(
        next_dispatch.begin_attempt(&next, None).await.unwrap(),
        AttemptAdmission::Blocked
    );
    let directory = SqliteWorkerDirectory::open(files.path().join("registry.sqlite"))
        .await
        .unwrap();
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
    let recovery =
        SqliteCommitCoordinator::open(files.path().join("runtime.sqlite").to_str().unwrap())
            .await
            .unwrap();
    let receipt = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(WorkerProcessStopStatus::Exited(receipt)) =
                awaken_dispatch::stop_expired_predecessor(
                    store.as_ref(),
                    &directory,
                    stop.as_ref(),
                    Some(&awaken_dispatch::PhysicalAttemptRecoveryRead::new(
                        &recovery,
                    )),
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
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match pods.get(&created[0].metadata.name.clone().unwrap()).await {
                Err(kube::Error::Api(error)) if error.code == 404 => break,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .expect("durable receipt allows exact old Pod evidence release");
    assert_eq!(
        store
            .load_physical_attempt(&old_claim.run_id)
            .await
            .unwrap(),
        None,
        "L5"
    );
    assert_eq!(
        ledgers
            .get(ledger.name())
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(ledger.uid()),
        "L3a old Pod deletion cannot garbage-collect the independent ledger"
    );
    let process_ledger =
        K8sWorkerCreateLedger::new(client.clone(), control_name, name, &cluster).unwrap();
    let retained_process = process_ledger
        .read_process_binding(K8sLedgerBinding {
            worker_namespace: name,
            worker_name: created[0].metadata.name.as_deref().unwrap(),
            worker_uid: created[0].metadata.uid.as_deref().unwrap(),
            ledger_name: ledger.name(),
            ledger_uid: ledger.uid(),
            configuration_fingerprint: ledger.configuration_fingerprint(),
        })
        .await
        .unwrap()
        .expect("original process coordinates survive Pod deletion");
    assert!(
        retained_process.matches_pod(
            name,
            created[0].metadata.name.as_deref().unwrap(),
            created[0].metadata.uid.as_deref().unwrap(),
        ) && retained_process.worker_evidence_was_retained(),
        "L3a provider process binding remains exact"
    );
    // C8 successor is a different registered Pod after Coordinator restart;
    // C9 the old exact process-exit receipt and one completed Create ticket
    // survive. E8 successor seals the old Worker ledger and joins that request
    // without recreating its ConfigMap or borrowing the successor's UID. This
    // is request closure only, not a Session terminal cleanup receipt.
    let successor_create = WorkerK8sCreateClient::new(next_upstream.clone());
    successor_create.bind_identity(successor.clone()).unwrap();
    let old_effect = awaken_provisioning_contract::SandboxEffectFence::new(
        "old-create-effect",
        &worker.snapshot.identity.worker_id,
        worker.snapshot.identity.lease_owner(),
        1,
        u64::MAX,
    )
    .unwrap();
    assert!(
        matches!(
            successor_create.seal_predecessor("old-session", &old_effect, "awaken-old-session").await.unwrap(),
            RequestJoin::Complete(tickets)
                if tickets.len() == 1 && tickets[0].request.resource == "configmaps"
        ),
        "E8 exact old Create request joined across both process boundaries"
    );
    assert_eq!(
        next_dispatch.begin_attempt(&next, None).await.unwrap(),
        AttemptAdmission::Applied,
        "L6"
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
    // E10 observes a retained, independently stopped idle Pod. L7 below
    // separately proves Deployment UID replacement after idle retirement.
    std::fs::write(&idle_token, token(&accounts, &created[2], audience).await).unwrap();
    let idle_control = WorkerControlClient::new(
        upstream(&address, &created[2], &idle_token).with_worker_identity(idle_identity.clone()),
    );
    ready(&idle_control, &idle_identity, 1).await;
    let idle_record = directory
        .incarnation(&idle_identity)
        .await
        .unwrap()
        .unwrap();
    assert!(
        idle_record.snapshot.expires_at_ms > clock.now_ms(),
        "E10 graceful idle Worker still has original Registry lease time"
    );
    let idle_anchor = idle_record.create_ledger.unwrap();
    let idle_process = process_ledger
        .read_process_binding(K8sLedgerBinding {
            worker_namespace: name,
            worker_name: "idle",
            worker_uid: created[2].metadata.uid.as_deref().unwrap(),
            ledger_name: idle_anchor.name(),
            ledger_uid: idle_anchor.uid(),
            configuration_fingerprint: idle_anchor.configuration_fingerprint(),
        })
        .await
        .unwrap()
        .unwrap();
    let running = PodProcessObserver::new(client.clone(), name)
        .unwrap()
        .bind_pod_process("idle", created[2].metadata.uid.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        idle_process, running,
        "E10 registered process binding still names the running Pod"
    );
    assert_eq!(
        idle_control.deregister(&idle_identity).await.unwrap(),
        awaken_dispatch::RegistryMutation::Applied,
        "E10 close exact idle Registry admission after Coordinator restart"
    );
    assert!(
        directory
            .incarnation(&idle_identity)
            .await
            .unwrap()
            .unwrap()
            .process_exit
            .is_none(),
        "E10 a running Quiesced Worker has no premature exit receipt"
    );
    assert!(
        pods.get("idle")
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "E10 graceful deregistration did not delete the running Pod"
    );
    pods.delete("idle", &DeleteParams::default()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            awaken_dispatch::release_closed_idle_worker_evidence(
                store.as_ref(),
                &directory,
                stop.as_ref(),
                &idle_identity,
                clock.now_ms(),
            )
            .await
            .unwrap();
            if directory
                .incarnation(&idle_identity)
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("E10 retained process exits and Registry commits exact receipt");
    assert_eq!(
        directory
            .incarnation(&idle_identity)
            .await
            .unwrap()
            .unwrap()
            .process_exit
            .unwrap()
            .provider_id(),
        cluster,
        "E10 provider-backed receipt before release"
    );
    // Cause/effect extension: L7 real Always Deployment + real Pod-bound
    // registration + exact idle withdrawal inside the original Registry lease
    // => no premature DELETE or exit receipt. L7b lease expiry + zero owned
    // physical slots + original UID => the existing Coordinator release owner
    // stops OLD, commits its observed exit and lets the controller produce a
    // Ready NEW UID. L8 source-side retry for OLD => no successor mutation;
    // L9 NEW registers a distinct incarnation. A foreign physical slot stays
    // occupied. This is live API/controller evidence, not a full Worker crash
    // or Hosted product acceptance. Readiness settles on persisted Pod status.
    let deployments: kube::Api<Deployment> = kube::Api::namespaced(client.clone(), name);
    deployments.create(&PostParams::default(), &serde_json::from_value(json!({
        "metadata":{"name":"closed-idle-worker"},
        "spec":{"replicas":1,"selector":{"matchLabels":{"app":"closed-idle-worker"}},
            "template":{"metadata":{"labels":{"app":"closed-idle-worker"}},
                "spec":{"serviceAccountName":"executor","automountServiceAccountToken":false,
                    "terminationGracePeriodSeconds":1,
                    "containers":[{"name":"worker","image":image,"imagePullPolicy":"Never",
                        "command":["sleep","600"],"securityContext":{"allowPrivilegeEscalation":false,
                            "capabilities":{"drop":["ALL"]}}}]}}}
    })).unwrap()).await.unwrap();
    let idle = ready_deployment_pod(&pods, None).await;
    let idle_token = files.path().join("idle-token");
    std::fs::write(&idle_token, token(&accounts, &idle, audience).await).unwrap();
    let idle_upstream = upstream(&address, &idle, &idle_token);
    let idle_registration = WorkerControlClient::new(idle_upstream.clone())
        .register_classified("idle-process-nonce", manifest())
        .await
        .unwrap()
        .worker;
    let idle_identity = idle_registration.snapshot.identity.clone();
    let idle_control =
        WorkerControlClient::new(idle_upstream.with_worker_identity(idle_identity.clone()));
    assert_eq!(
        idle_control.deregister(&idle_identity).await.unwrap(),
        awaken_dispatch::RegistryMutation::Applied,
        "L7"
    );
    let directory = SqliteWorkerDirectory::open(files.path().join("registry.sqlite"))
        .await
        .unwrap();
    let closed = directory
        .incarnation(&idle_identity)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        closed.snapshot.state,
        awaken_dispatch::WorkerState::Dead,
        "L7"
    );
    assert!(
        closed.process_exit.is_none(),
        "L7 a closed but still-running Worker has no exit evidence"
    );
    assert!(
        pods.get(idle.metadata.name.as_deref().unwrap())
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "L7 the original live lease does not authorize Pod deletion"
    );
    clock.set(closed.snapshot.expires_at_ms.checked_add(1).unwrap());
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            awaken_dispatch::release_closed_idle_worker_evidence(
                store.as_ref(),
                &directory,
                transport.stop_source().unwrap().as_ref(),
                &idle_identity,
                clock.now_ms(),
            )
            .await
            .unwrap();
            if directory
                .incarnation(&idle_identity)
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("L7b exact original process exit is retained before successor admission");
    let successor = ready_deployment_pod(&pods, idle.metadata.uid.as_deref()).await;
    awaken_dispatch::release_closed_idle_worker_evidence(
        store.as_ref(),
        &directory,
        transport.stop_source().unwrap().as_ref(),
        &idle_identity,
        clock.now_ms(),
    )
    .await
    .unwrap();
    assert!(
        pods.get(successor.metadata.name.as_deref().unwrap())
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "L8"
    );
    let successor_token = files.path().join("idle-successor-token");
    std::fs::write(
        &successor_token,
        token(&accounts, &successor, audience).await,
    )
    .unwrap();
    let registered_successor =
        WorkerControlClient::new(upstream(&address, &successor, &successor_token))
            .register_classified("successor-process-nonce", manifest())
            .await
            .unwrap()
            .worker;
    assert_eq!(
        registered_successor.snapshot.state,
        awaken_dispatch::WorkerState::Starting,
        "L9"
    );
    assert_ne!(
        registered_successor.snapshot.identity.worker_id, idle_identity.worker_id,
        "L9"
    );
    assert_eq!(
        registered_successor.snapshot.identity.incarnation_id,
        successor.metadata.uid.as_deref().unwrap(),
        "L9"
    );
    assert!(
        store
            .load_physical_attempt(&next.run_id)
            .await
            .unwrap()
            .is_some(),
        "L7 foreign physical slot preserved"
    );
    created.push(successor);
    server.stop().await;
    // Only these test-owned retention markers are released. The enclosing
    // cluster harness is also the cleanup authority if any assertion panics.
    for pod in created.into_iter().skip(1) {
        let pod_name = pod.metadata.name.unwrap();
        let current = match pods.get(&pod_name).await {
            Ok(pod) => pod,
            Err(kube::Error::Api(error)) if error.code == 404 => continue,
            Err(error) => panic!("test-owned Pod read failed: {error}"),
        };
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
    namespaces
        .delete(
            control_name,
            &DeleteParams::default().preconditions(kube::api::Preconditions {
                uid: control_namespace.metadata.uid,
                resource_version: None,
            }),
        )
        .await
        .unwrap();
}

use awaken_dispatch::{
    DispatchClaims as _, DispatchControlReads as _, DispatchEnqueue as _, DispatchExecution as _,
};
