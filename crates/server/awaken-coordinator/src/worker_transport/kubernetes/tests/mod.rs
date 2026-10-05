// @formal evidence model_operation.coverage_commit_strictly_precedes_model_entry
// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
use super::*;
use awaken_dispatch::ControlDispatchQueue;
mod live;
use axum::{
    Json, Router,
    extract::State,
    http::{Request, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Facts {
    pod: Mutex<(StatusCode, Value)>,
    reviews: AtomicUsize,
    reads: AtomicUsize,
    deletes: Mutex<Vec<Value>>,
    updates: Mutex<Vec<Value>>,
}

fn model_only_activation(tag: &str) -> awaken_runtime_contract::activation::RunActivation {
    let mut activation = awaken_dispatch_testkit::worker_http::activation(tag);
    // The generic HTTP fixture deliberately owns two opaque consistency tokens;
    // actual Runtime execution requires the canonical internally coherent hash.
    // Compile before enqueue so Dispatch and Runtime consume identical bytes.
    activation.snapshot.recompute_fingerprint().unwrap();
    activation
}

/// Generate closure history through the actual Runtime journal, shared by the
/// mock-API and live-Pod qualification. Never write a coverage/terminal pair by
/// hand: that could accidentally qualify a history production cannot produce.
async fn commit_actual_model_only_result<R>(
    recovery: Arc<R>,
    activation: awaken_runtime_contract::activation::RunActivation,
) where
    R: awaken_runtime_contract::CommitCoordinator
        + awaken_runtime_contract::CommittedThreadView
        + awaken_runtime_contract::RunRecoverySource
        + 'static,
{
    use awaken_agent_contract::agent::state::StateKey;
    use awaken_runtime_contract::RunExecutor as _;
    use awaken_runtime_contract::attempt_termination::ActiveNativeAttemptCoverage;
    use awaken_runtime_contract::model_operation::ActiveModelOperation;
    struct CompleteModel(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for CompleteModel {
        async fn infer(
            &self,
            _: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(awaken_runtime_contract::llm::ChatResponse {
                output: awaken_runtime_contract::llm::AssistantOutput::text(
                    "complete fixture response",
                ),
                usage: None,
                stop_reason: None,
            })
        }
    }
    // R1 one complete model result, no tools/plugins => actual pre-effect
    // coverage commit strictly before model entry, one invocation and a closed
    // journal. R2 replay/retirement only reads this exact durable snapshot.
    // This fixture qualifies journal wiring, not a remote cancellation/query.
    let thread = activation.thread_id.clone();
    let run = activation.run_id.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let result = awaken_runtime::Runtime::new()
        .with_llm(Arc::new(CompleteModel(calls.clone())))
        .execute(
            activation,
            awaken_runtime_contract::RuntimeRunContext::new()
                .with_reader(recovery.clone())
                .with_commit(recovery.clone()),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        awaken_agent_contract::agent::run::RunState::Ended(
            awaken_agent_contract::agent::run::EndCause::NaturalEnd
        ),
        "R1 actual Runtime result"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "R1 one invocation");
    let snapshot = recovery.recovery_snapshot(&thread, &run).await.unwrap();
    let cursor_for = |key: &str| {
        snapshot
            .state
            .iter()
            .zip(&snapshot.state_commit_cursors)
            .find_map(|(command, cursor)| (command.key.0 == key).then_some(*cursor))
            .unwrap()
    };
    assert!(
        cursor_for(ActiveNativeAttemptCoverage::KEY) < cursor_for(ActiveModelOperation::KEY),
        "R1 enrollment must actually commit first"
    );
    assert_eq!(
        classify_native_attempt_termination(&snapshot).unwrap(),
        NativeAttemptTermination::AllExecutionStopped,
        "R2 real journal is the classifier's input"
    );
}

fn running() -> Value {
    json!({"metadata":{"namespace":"workers","name":"worker-a","uid":"uid-a","resourceVersion":"7",
        "finalizers":["awaken.dev/worker-process-evidence"]},
    "spec":{"nodeName":"node-a","restartPolicy":"Never","containers":[{
        "name":"worker","image":"fixture","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}
    }]},"status":{"phase":"Running","containerStatuses":[{
        "name":"worker","image":"fixture","imageID":"fixture","ready":true,"restartCount":0,"containerID":"containerd://exact",
        "state":{"running":{"startedAt":"2026-09-09T00:00:00Z"}}
    }]}})
}

fn fixture() -> (WorkerTransport, Arc<Facts>) {
    let facts = Arc::new(Facts {
        pod: Mutex::new((StatusCode::OK, running())),
        reviews: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        deletes: Mutex::new(Vec::new()),
        updates: Mutex::new(Vec::new()),
    });
    async fn review(State(facts): State<Arc<Facts>>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["spec"]["token"], "fixture-token");
        assert_eq!(body["spec"]["audiences"], json!(["awaken-worker"]));
        facts.reviews.fetch_add(1, Ordering::SeqCst);
        Json(
            json!({"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","spec":{},"status":{
                "authenticated":true,"audiences":["awaken-worker"],"user":{
                    "username":"system:serviceaccount:workers:executor","uid":"sa-uid","extra":{
                        "authentication.kubernetes.io/pod-name":["worker-a"],"authentication.kubernetes.io/pod-uid":["uid-a"]
                    }
                }
            }}),
        )
    }
    async fn pod(State(facts): State<Arc<Facts>>) -> (StatusCode, Json<Value>) {
        facts.reads.fetch_add(1, Ordering::SeqCst);
        let (status, body) = facts.pod.lock().unwrap().clone();
        (status, Json(body))
    }
    async fn delete(State(facts): State<Arc<Facts>>, Json(body): Json<Value>) -> Json<Value> {
        facts.deletes.lock().unwrap().push(body);
        facts.pod.lock().unwrap().1["metadata"]["deletionTimestamp"] =
            json!("2026-09-09T00:01:00Z");
        Json(json!({"kind":"Status","apiVersion":"v1","status":"Success"}))
    }
    async fn update(State(facts): State<Arc<Facts>>, Json(mut body): Json<Value>) -> Json<Value> {
        let mut current = facts.pod.lock().unwrap();
        assert_eq!(body["metadata"]["uid"], current.1["metadata"]["uid"]);
        assert_eq!(
            body["metadata"]["resourceVersion"],
            current.1["metadata"]["resourceVersion"]
        );
        facts.updates.lock().unwrap().push(body.clone());
        body["metadata"]["resourceVersion"] = json!("8");
        current.1 = body.clone();
        Json(body)
    }
    // One actual kube Client over an in-process API transport. Only the exact
    // Pod stop is writable; unrelated provisioning has no route. Not k3d proof.
    let router = Router::new()
        .route("/apis/authentication.k8s.io/v1/tokenreviews", post(review))
        .route(
            "/api/v1/namespaces/workers/pods/worker-a",
            get(pod).put(update).delete(delete),
        )
        .with_state(facts.clone());
    let transport = WorkerTransport::kubernetes(
        kube::Client::new(router, "workers"),
        "workers",
        "executor",
        "awaken-worker",
        "test-cluster",
    )
    .unwrap();
    (transport, facts)
}

#[tokio::test]
async fn authenticated_pod_is_bound_through_the_same_client_without_a_second_identity_store() {
    // C1 exact reviewed Pod identity; C2 exact running private non-restarting
    // Pod; C3 Registry-allocated incarnation. R1 conjunction => frozen binding
    // contains that incarnation + operator provider + exact process coordinate.
    // R2 Registry generation remains separately checked; the Pod incarnation
    // cannot change independently of its immutable UID.
    // Authentication proves the caller, not termination or replay authority.
    let (transport, facts) = fixture();
    let auth = transport.authenticator();
    let bindings = transport.binding_source();
    let stops = transport.stop_source();
    assert!(
        stops.is_some(),
        "production selection installs independent stop control"
    );
    let bindings = bindings.expect("Kubernetes selection cannot lose its binding source");
    for generation in [1, 2] {
        let worker = WorkerIdentity::new("k8s:workers:worker-a:uid-a", "uid-a", generation);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/worker/dispatch/attempt/begin")
            .header("x-awaken-worker-id", &worker.worker_id)
            .header("authorization", "Bearer fixture-token")
            .header(
                "x-awaken-worker-incarnation",
                serde_json::to_string(&worker).unwrap(),
            )
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let caller = auth.authenticate(&request).await.unwrap();
        let binding = bindings
            .binding_for(caller.identity().unwrap())
            .await
            .unwrap();
        assert_eq!(binding.worker(), &worker);
        assert_eq!(binding.provider_id(), "test-cluster");
        let coordinate: Value = serde_json::from_str(binding.execution_ref()).unwrap();
        assert_eq!(coordinate["uid"], "uid-a");
        assert_eq!(coordinate["node"], "node-a");
        assert_eq!(
            coordinate["containers"][0]["container_id"],
            "containerd://exact"
        );
    }
    assert_eq!(facts.reviews.load(Ordering::SeqCst), 2);
    assert_eq!(facts.reads.load(Ordering::SeqCst), 2);
    assert!(
        facts.deletes.lock().unwrap().is_empty(),
        "binding is read-only"
    );
}

#[tokio::test]
async fn pod_replacement_and_unobservable_processes_never_degrade_to_unbound_admission() {
    // R3 old token followed by Pod name reuse => reject at the UID read, even
    // when every other process field is unchanged. R4 missing/forbidden/error
    // response, restarting/privileged/live-participant drift => Unavailable,
    // never None. R5 malformed/cross-namespace Worker => reject before Pod IO.
    // Each mutation starts from the valid R1 witness (MC/DC).
    let (transport, facts) = fixture();
    let source = transport.binding_source().unwrap();
    let worker = WorkerIdentity::new("k8s:workers:worker-a:uid-a", "uid-a", 1);
    for (pointer, value) in [
        ("/metadata/uid", json!("uid-replacement")),
        ("/metadata/finalizers", json!([])),
        ("/spec/restartPolicy", json!("OnFailure")),
        (
            "/spec/containers/0/securityContext/allowPrivilegeEscalation",
            json!(true),
        ),
        ("/status/containerStatuses/0/containerID", Value::Null),
        ("/status/containerStatuses/0/restartCount", json!(1)),
        ("/status/phase", json!("Failed")),
    ] {
        let mut pod = running();
        *pod.pointer_mut(pointer).unwrap() = value;
        *facts.pod.lock().unwrap() = (StatusCode::OK, pod);
        assert!(
            matches!(
                source.binding_for(&worker).await.unwrap_err().kind,
                awaken_dispatch::ApplicationErrorKind::Unavailable
            ),
            "R3/R4 {pointer}"
        );
    }
    for code in [
        StatusCode::NOT_FOUND,
        StatusCode::FORBIDDEN,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        *facts.pod.lock().unwrap() = (
            code,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"fixture-secret","code":code.as_u16()}),
        );
        let error = source.binding_for(&worker).await.unwrap_err();
        assert!(
            !error.to_string().contains("fixture-secret"),
            "R4 secret-safe"
        );
    }
    let before = facts.reads.load(Ordering::SeqCst);
    // R6 the same Pod id with a new process-proposed incarnation must fail
    // before provider I/O, just like a cross-namespace identity (MC/DC).
    assert!(
        source
            .binding_for(&WorkerIdentity::new(&worker.worker_id, "new-boot", 1))
            .await
            .is_err()
    );
    for id in ["unsigned-worker", "k8s:foreign:worker-a:uid-a"] {
        assert!(
            source
                .binding_for(&WorkerIdentity::new(id, "boot", 1))
                .await
                .is_err(),
            "R5"
        );
    }
    assert_eq!(facts.reads.load(Ordering::SeqCst), before, "R5 no API read");
}

#[tokio::test]
async fn deployment_binding_freezes_restart_policy_through_the_same_transport_owner() {
    // D1 reviewed UID + Registry incarnation + retained original Deployment
    // container => the existing binding source freezes Always. D2 removing the
    // finalizer before admission => Unavailable, not an unbound fallback.
    let (transport, facts) = fixture();
    let source = transport.binding_source().unwrap();
    let worker = WorkerIdentity::new("k8s:workers:worker-a:uid-a", "uid-a", 1);
    let mut pod = running();
    pod["spec"]["restartPolicy"] = json!("Always");
    *facts.pod.lock().unwrap() = (StatusCode::OK, pod.clone());
    let binding = source.binding_for(&worker).await.unwrap();
    let coordinate: Value = serde_json::from_str(binding.execution_ref()).unwrap();
    assert_eq!(coordinate["restartable"], true, "D1");
    assert_eq!(
        coordinate["containers"][0]["container_id"],
        "containerd://exact"
    );
    pod["metadata"]["finalizers"] = json!([]);
    *facts.pod.lock().unwrap() = (StatusCode::OK, pod);
    assert!(source.binding_for(&worker).await.is_err(), "D2");
    assert!(facts.deletes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn independent_stop_consumes_only_original_closed_binding_and_preserves_dispatch_veto() {
    use awaken_dispatch::{DispatchQueue, WorkerDirectory};
    use awaken_store_inmem::MemoryCommitCoordinator;
    // S1 current successor + persisted old binding + exact expired Registry
    // => independent UID/resourceVersion stop with Coordinator client (no token
    // from the dead Worker). S2 accepted/delete-in-progress => Unknown. S3 exact
    // local exit => Exited, STILL occupied; remote effects are not inferred.
    // S4 exact local exit is committed to Registry before one exact finalizer
    // update; complete model-only ThreadCommit evidence then lets the existing
    // exact finish transition clear only the old slot, after which a
    // fresh successor admission applies. S5 provider/Worker/opaque-coordinate
    // mismatch => no API read/write. S6 absent/name-reused Pod => Unknown; an
    // old late query never stops the replacement.
    for rule in [
        "valid",
        "foreign-provider",
        "foreign-coordinate",
        "missing",
        "replaced",
    ] {
        let (transport, facts) = fixture();
        let bindings = transport.binding_source();
        let stops = transport.stop_source();
        let directory = awaken_worker_registry::MemoryWorkerDirectory::new();
        let mut manifest = awaken_dispatch::WorkerManifest::default();
        manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
        manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
        let old = directory
            .register(
                awaken_dispatch::WorkerRegistration {
                    worker_id: "k8s:workers:worker-a:uid-a".into(),
                    incarnation_id: "uid-a".into(),
                    manifest,
                },
                100,
                50,
            )
            .await
            .unwrap();
        let worker = old.snapshot.identity.clone();
        let original = bindings.unwrap().binding_for(&worker).await.unwrap();
        let bound = if rule == "foreign-provider" {
            AttemptExecutionBinding::new(worker.clone(), "other-cluster", original.execution_ref())
                .unwrap()
        } else if rule == "foreign-coordinate" {
            let mut coordinate: Value = serde_json::from_str(original.execution_ref()).unwrap();
            coordinate["uid"] = json!("uid-foreign");
            AttemptExecutionBinding::new(
                worker.clone(),
                original.provider_id(),
                coordinate.to_string(),
            )
            .unwrap()
        } else {
            original.clone()
        };
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = awaken_dispatch::MemoryDispatchStore::new(clock.clone());
        let activation = model_only_activation("cli-stop");
        let model_activation = activation.clone();
        let request = awaken_dispatch::RunDispatch::new(activation)
            .with_placement(awaken_dispatch::PlacementRequirements::remote_required());
        store.enqueue(request).await.unwrap();
        let mut snapshot = old.snapshot;
        snapshot.state = awaken_dispatch::WorkerState::Ready;
        clock.set(100);
        let old_claim = awaken_dispatch::RunClaim::from(
            &store
                .claim_compatible(&snapshot, 50)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        clock.set(100);
        store.begin_attempt(&old_claim, Some(&bound)).await.unwrap();
        let recovery = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        if rule == "valid" {
            commit_actual_model_only_result(recovery.clone(), model_activation).await;
        }
        snapshot.identity = WorkerIdentity::new("successor", "successor-boot", 1);
        snapshot.expires_at_ms = 10_000;
        clock.set(200);
        let next = awaken_dispatch::RunClaim::from(
            &store
                .claim_compatible(&snapshot, 10_000)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        if rule == "missing" {
            *facts.pod.lock().unwrap() = (
                StatusCode::NOT_FOUND,
                json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
            );
        } else if rule == "replaced" {
            facts.pod.lock().unwrap().1["metadata"]["uid"] = json!("new-uid");
        }
        let reads = facts.reads.load(Ordering::SeqCst);
        let stop = stops.unwrap();
        let result = awaken_dispatch::stop_expired_predecessor(
            &store,
            &directory,
            stop.as_ref(),
            None,
            &next,
            200,
        )
        .await;
        if rule.starts_with("foreign") {
            assert!(result.is_err(), "S4/{rule}");
            assert_eq!(facts.reads.load(Ordering::SeqCst), reads);
        } else {
            assert_eq!(
                result.unwrap(),
                Some(WorkerProcessStopStatus::Unknown),
                "S2/S5/{rule}"
            );
        }
        if rule == "valid" {
            assert_eq!(
                facts.deletes.lock().unwrap()[0]["preconditions"],
                json!({"uid":"uid-a","resourceVersion":"7"})
            );
            assert_eq!(
                awaken_dispatch::stop_expired_predecessor(
                    &store,
                    &directory,
                    stop.as_ref(),
                    None,
                    &next,
                    200
                )
                .await
                .unwrap(),
                Some(WorkerProcessStopStatus::Unknown)
            );
            {
                let mut pod = facts.pod.lock().unwrap();
                pod.1["status"]["phase"] = json!("Failed");
                pod.1["status"]["containerStatuses"][0]["state"] = json!({"terminated":{
                    "exitCode":137,"reason":"Error","containerID":"containerd://exact",
                    "startedAt":"2026-09-09T00:00:00Z","finishedAt":"2026-09-09T00:01:00Z"
                }});
            }
            let observation = awaken_dispatch::stop_expired_predecessor(
                &store,
                &directory,
                stop.as_ref(),
                Some(recovery.as_ref()),
                &next,
                200,
            )
            .await
            .unwrap();
            let Some(WorkerProcessStopStatus::Exited(receipt)) = observation else {
                panic!("S3 exact local exit")
            };
            assert_eq!(receipt.binding(), &bound);
            assert_eq!(receipt.evidence_ref(), "7");
            let persisted = directory
                .incarnation(&worker)
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .unwrap();
            assert_eq!(persisted.evidence_ref(), "7", "S4 commit before release");
            assert_eq!(facts.updates.lock().unwrap().len(), 1, "S4 one release");
            assert!(
                !facts.pod.lock().unwrap().1["metadata"]["finalizers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == "awaken.dev/worker-process-evidence"),
                "S4 process evidence released"
            );
            assert_eq!(
                facts.deletes.lock().unwrap().len(),
                1,
                "S2 no duplicate delete while pending or exited"
            );
        } else {
            assert!(facts.deletes.lock().unwrap().is_empty());
            assert!(facts.updates.lock().unwrap().is_empty());
        }
        assert_eq!(
            facts.reviews.load(Ordering::SeqCst),
            0,
            "S1 dead Worker token is unnecessary"
        );
        if rule == "valid" {
            assert_eq!(
                store
                    .load_physical_attempt(&old_claim.run_id)
                    .await
                    .unwrap(),
                None,
                "S4 complete evidence clears exactly the old slot"
            );
            clock.set(200);
            assert_eq!(
                store.begin_attempt(&next, None).await.unwrap(),
                awaken_dispatch::AttemptAdmission::Applied,
                "S4 successor retries through fresh admission"
            );
        } else {
            assert_eq!(
                store
                    .load_physical_attempt(&old_claim.run_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .claim,
                old_claim
            );
            clock.set(200);
            assert_eq!(
                store.begin_attempt(&next, None).await.unwrap(),
                awaken_dispatch::AttemptAdmission::Blocked,
                "S3 remote veto"
            );
        }
    }
}

#[test]
fn shared_policy_validation_rejects_invalid_provider_coordinates_before_io() {
    // Decision P1: exact provider/ns/account/audience -> accepted. P2 any blank,
    // padded or malformed coordinate -> rejected before provider I/O. Existing
    // CLI tests separately own role/build-feature/signing-trust exclusivity.
    assert!(
        WorkerTransport::validate_kubernetes_policy(
            "workers",
            "executor",
            "awaken-worker",
            "cluster"
        )
        .is_ok()
    );
    for (namespace, account, audience, provider) in [
        ("../workers", "executor", "awaken-worker", "cluster"),
        ("workers", "", "awaken-worker", "cluster"),
        ("workers", "executor", " ", "cluster"),
        ("workers", "executor", "awaken-worker", ""),
        ("workers", "executor", "awaken-worker", " cluster "),
    ] {
        assert!(
            WorkerTransport::validate_kubernetes_policy(namespace, account, audience, provider)
                .is_err()
        );
    }
}
