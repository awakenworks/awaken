use super::*;
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
}

fn running() -> Value {
    json!({"metadata":{"namespace":"workers","name":"worker-a","uid":"uid-a","resourceVersion":"7"},
    "spec":{"nodeName":"node-a","restartPolicy":"Never","containers":[{
        "name":"worker","image":"fixture","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}
    }]},"status":{"phase":"Running","containerStatuses":[{
        "name":"worker","image":"fixture","imageID":"fixture","ready":true,"restartCount":0,"containerID":"containerd://exact",
        "state":{"running":{"startedAt":"2026-09-09T00:00:00Z"}}
    }]}})
}

fn policy() -> crate::config::KubernetesWorkerTransport {
    crate::config::KubernetesWorkerTransport {
        namespace: "workers".into(),
        service_account: "executor".into(),
        audience: "awaken-worker".into(),
        provider_id: "test-cluster".into(),
    }
}

fn fixture() -> (WorkerTransport, Arc<Facts>) {
    let facts = Arc::new(Facts {
        pod: Mutex::new((StatusCode::OK, running())),
        reviews: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
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
    // One actual kube Client over an in-process API transport. Unexpected Pod
    // writes/NetworkPolicy calls have no route and fail; no live cluster implied.
    let router = Router::new()
        .route("/apis/authentication.k8s.io/v1/tokenreviews", post(review))
        .route("/api/v1/namespaces/workers/pods/worker-a", get(pod))
        .with_state(facts.clone());
    let transport = assemble(&policy(), kube::Client::new(router, "workers")).unwrap();
    (transport, facts)
}

#[tokio::test]
async fn authenticated_pod_is_bound_through_the_same_client_without_a_second_identity_store() {
    // C1 exact reviewed Pod identity; C2 exact running private non-restarting
    // Pod; C3 Registry-allocated incarnation. R1 conjunction => frozen binding
    // contains that incarnation + operator provider + exact process coordinate.
    // R2 new incarnation changes only the Worker tuple, never physical identity.
    // Authentication proves the caller, not termination or replay authority.
    let (transport, facts) = fixture();
    let (auth, bindings) = transport.into_parts();
    let bindings = bindings.expect("Kubernetes selection cannot lose its binding source");
    for generation in [1, 2] {
        let worker = WorkerIdentity::new(
            "k8s:workers:worker-a:uid-a",
            format!("boot-{generation}"),
            generation,
        );
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
}

#[tokio::test]
async fn pod_replacement_and_unobservable_processes_never_degrade_to_unbound_admission() {
    // R3 old token followed by Pod name reuse => reject at the UID read, even
    // when every other process field is unchanged. R4 missing/forbidden/error
    // response, restarting/privileged/live-participant drift => Unavailable,
    // never None. R5 malformed/cross-namespace Worker => reject before Pod IO.
    // Each mutation starts from the valid R1 witness (MC/DC).
    let (transport, facts) = fixture();
    let source = transport.into_parts().1.unwrap();
    let worker = WorkerIdentity::new("k8s:workers:worker-a:uid-a", "boot", 1);
    for (pointer, value) in [
        ("/metadata/uid", json!("uid-replacement")),
        ("/spec/restartPolicy", json!("Always")),
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
