// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
use super::super::tests::{decode, running, terminal};
use super::*;
use http::{Request, Response};
use kube::client::Body;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    BeforeDelete,
    AfterDelete,
    Conflict,
    Replacement,
    RetentionLost,
    PauseAfterDelete,
    ReleaseConflict,
    ReleaseResponseLost,
}

struct Facts {
    pod: Option<Pod>,
    read_status: u16,
    calls: Vec<(String, Value)>,
    accepted: usize,
    released: usize,
    fault: Fault,
}

struct Fixture {
    client: kube::Client,
    facts: Arc<Mutex<Facts>>,
    accepted: Arc<Notify>,
    release: Arc<Semaphore>,
}

fn retained(mut pod: Value) -> Pod {
    pod["metadata"]["finalizers"] =
        json!([WORKER_PROCESS_EVIDENCE_FINALIZER, "other.example/keep"]);
    decode(pod)
}

fn status(code: u16) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::from(
            serde_json::to_vec(&json!({
                "apiVersion":"v1", "kind":"Status", "status":"Failure", "code":code,
                "message":"fixture API response", "reason":"fixture"
            }))
            .unwrap(),
        ))
        .unwrap()
}

fn disconnected() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fixture response lost")
}

impl Fixture {
    fn new(fault: Fault) -> Self {
        let facts = Arc::new(Mutex::new(Facts {
            pod: Some(retained(running())),
            read_status: 200,
            calls: Vec::new(),
            accepted: 0,
            released: 0,
            fault,
        }));
        let accepted = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let (serving, signal, resume) = (facts.clone(), accepted.clone(), release.clone());
        // Use kube's supported service transport; no second hand-written HTTP
        // parser/server or Sandbox cleanup state machine is needed for faults.
        let service = tower::service_fn(move |request: Request<Body>| {
            let (facts, signal, resume) = (serving.clone(), signal.clone(), resume.clone());
            async move {
                assert_eq!(request.uri().path(), "/api/v1/namespaces/ns/pods/worker");
                let method = request.method().to_string();
                let bytes = request.into_body().collect_bytes().await.unwrap();
                let body = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                let (reply, pause) = {
                    let mut state = facts.lock().unwrap();
                    state.calls.push((method.clone(), body.clone()));
                    match method.as_str() {
                        "GET" => {
                            if state.read_status != 200 {
                                return Ok(status(state.read_status));
                            }
                            let Some(pod) = &state.pod else {
                                return Ok(status(404));
                            };
                            (
                                Response::new(Body::from(serde_json::to_vec(pod).unwrap())),
                                false,
                            )
                        }
                        "DELETE" => {
                            let fault = std::mem::take(&mut state.fault);
                            if matches!(fault, Fault::BeforeDelete) {
                                return Err(disconnected());
                            }
                            let pod = state.pod.as_mut().unwrap();
                            match fault {
                                Fault::Conflict => pod.metadata.resource_version = Some("2".into()),
                                Fault::Replacement => pod.metadata.uid = Some("uid-b".into()),
                                Fault::RetentionLost => {
                                    pod.metadata.finalizers =
                                        Some(vec!["other.example/keep".into()]);
                                    pod.metadata.resource_version = Some("2".into());
                                }
                                _ => {}
                            }
                            if body["preconditions"]["uid"] != json!(pod.metadata.uid)
                                || body["preconditions"]["resourceVersion"]
                                    != json!(pod.metadata.resource_version)
                            {
                                return Ok(status(409));
                            }
                            pod.metadata.deletion_timestamp = Some(
                                serde_json::from_value(json!("2026-09-08T00:00:02Z")).unwrap(),
                            );
                            pod.metadata.resource_version = Some("2".into());
                            state.accepted += 1;
                            if matches!(fault, Fault::AfterDelete) {
                                return Err(disconnected());
                            }
                            (Response::new(Body::from(serde_json::to_vec(&json!({"apiVersion":"v1", "kind":"Status", "status":"Success"})).unwrap())), matches!(fault, Fault::PauseAfterDelete))
                        }
                        "PUT" => {
                            let fault = std::mem::take(&mut state.fault);
                            let response = {
                                let pod = state.pod.as_mut().unwrap();
                                if matches!(fault, Fault::ReleaseConflict) {
                                    pod.metadata.resource_version = Some("release-conflict".into());
                                    return Ok(status(409));
                                }
                                if body["metadata"]["uid"] != json!(pod.metadata.uid)
                                    || body["metadata"]["resourceVersion"]
                                        != json!(pod.metadata.resource_version)
                                {
                                    return Ok(status(409));
                                }
                                let mut replacement: Pod = decode(body);
                                replacement.metadata.resource_version = Some("released".into());
                                *pod = replacement;
                                serde_json::to_vec(pod).unwrap()
                            };
                            state.released += 1;
                            if matches!(fault, Fault::ReleaseResponseLost) {
                                return Err(disconnected());
                            }
                            (Response::new(Body::from(response)), false)
                        }
                        _ => panic!("unexpected mutation: {method}"),
                    }
                };
                if pause {
                    signal.notify_one();
                    let _permit = resume.acquire().await.unwrap();
                }
                Ok::<_, std::io::Error>(reply)
            }
        });
        Self {
            client: kube::Client::new(service, "ns"),
            facts,
            accepted,
            release,
        }
    }

    fn observer(&self) -> PodProcessObserver {
        PodProcessObserver::new(self.client.clone(), "ns").unwrap()
    }

    fn stopper(&self) -> PodProcessStopper {
        PodProcessStopper::new(self.observer())
    }

    async fn binding(&self) -> PodProcessBinding {
        self.observer()
            .bind_pod_process("worker", "uid-a")
            .await
            .unwrap()
    }

    fn exit(&self) {
        let mut facts = self.facts.lock().unwrap();
        let pod = facts.pod.as_mut().unwrap();
        // A kubelet status update preserves the API-owned deletion intent and
        // retention; replacing the whole Pod with a template would erase them.
        pod.status = decode(terminal()).status;
        pod.metadata.resource_version = Some("exited".into());
    }
}

#[tokio::test]
async fn durable_exit_allows_only_exact_idempotent_process_evidence_release() {
    // Evidence-release cause/effect graph: C1 retained binding provenance; C2
    // current Pod UID exact/replaced/absent; C3 target finalizer present/absent;
    // C4 replace succeeds/conflicts/response is lost. Effects: E1 invalid C1
    // performs no IO; E2 exact presence removes only the process finalizer; E3
    // conflict rereads and retries; E4 lost success converges by observation;
    // E5 absent/replaced/already released is complete because the caller has
    // already committed the exit receipt. Rules F1=C1+exact+present+success=>E2;
    // F2=F1+conflict=>E3+E2; F3=F1+lost reply=>E4; F4=C1+(absent|replaced|
    // !present)=>E5; F5=!C1=>E1. The shared finalizer CAS owns all PUTs.
    for fault in [Fault::None, Fault::ReleaseConflict] {
        let fixture = Fixture::new(fault);
        let binding = fixture.binding().await;
        fixture.stopper().release_evidence(&binding).await.unwrap();
        {
            let state = fixture.facts.lock().unwrap();
            let finalizers = state
                .pod
                .as_ref()
                .unwrap()
                .metadata
                .finalizers
                .as_ref()
                .unwrap();
            assert!(
                !finalizers
                    .iter()
                    .any(|v| v == WORKER_PROCESS_EVIDENCE_FINALIZER)
            );
            assert!(finalizers.iter().any(|v| v == "other.example/keep"));
            assert_eq!(state.released, 1, "F1/F2");
        }
        fixture.stopper().release_evidence(&binding).await.unwrap();
        assert_eq!(fixture.facts.lock().unwrap().released, 1, "F4 idempotent");
    }

    let fixture = Fixture::new(Fault::ReleaseResponseLost);
    let binding = fixture.binding().await;
    assert!(fixture.stopper().release_evidence(&binding).await.is_err());
    fixture.stopper().release_evidence(&binding).await.unwrap();
    assert_eq!(fixture.facts.lock().unwrap().released, 1, "F3");

    let fixture = Fixture::new(Fault::None);
    let binding = fixture.binding().await;
    fixture
        .facts
        .lock()
        .unwrap()
        .pod
        .as_mut()
        .unwrap()
        .metadata
        .uid = Some("replacement".into());
    fixture.stopper().release_evidence(&binding).await.unwrap();
    assert_eq!(fixture.facts.lock().unwrap().released, 0, "F4 replacement");
    fixture.facts.lock().unwrap().pod = None;
    fixture.stopper().release_evidence(&binding).await.unwrap();
    assert_eq!(fixture.facts.lock().unwrap().released, 0, "F4 absent");

    let fixture = Fixture::new(Fault::None);
    let mut legacy = fixture.binding().await;
    legacy.retained = false;
    let before = fixture.facts.lock().unwrap().calls.len();
    assert!(fixture.stopper().release_evidence(&legacy).await.is_err());
    assert_eq!(fixture.facts.lock().unwrap().calls.len(), before, "F5");
}

#[tokio::test]
async fn an_already_exited_pod_must_be_closed_before_a_stop_receipt() {
    // S12 causes: exact retained binding + every process already exited, but
    // no API deletion intent. Effects: exact DELETE, Unknown; only a fresh read
    // of the closed Pod can return the local stop receipt. Cross successful,
    // failing and OOM exits: restartPolicy Never is not an active closure fact.
    // S13 independent adapter/repeated observation => same receipt, one DELETE.
    for reason in ["Completed", "Error", "OOMKilled"] {
        let fixture = Fixture::new(Fault::None);
        let binding = fixture.binding().await;
        fixture.exit();
        fixture
            .facts
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .status
            .as_mut()
            .unwrap()
            .container_statuses
            .as_mut()
            .unwrap()[0]
            .state
            .as_mut()
            .unwrap()
            .terminated
            .as_mut()
            .unwrap()
            .reason = Some(reason.into());
        assert_eq!(
            fixture.stopper().request_stop(&binding).await.unwrap(),
            PodProcessTermination::Unknown,
            "S12 {reason}: exit is not closure"
        );
        assert_eq!(
            fixture.facts.lock().unwrap().accepted,
            1,
            "S12 exact DELETE"
        );
        for _ in 0..2 {
            let PodProcessTermination::Observed(receipt) =
                fixture.stopper().request_stop(&binding).await.unwrap()
            else {
                panic!("S13 closed exact exit: {reason}")
            };
            assert_eq!(receipt.binding(), &binding);
        }
        assert_eq!(
            fixture.facts.lock().unwrap().accepted,
            1,
            "S13 no duplicate DELETE"
        );
    }
}

#[tokio::test]
async fn stop_requires_prebound_retention_and_returns_only_independent_exact_exit() {
    // Causes: C1 pre-binding retention; C2 current retention; C3 exact process
    // identity/profile; C4 deleting; C5 complete runtime exit; C6 API read status.
    // Effects: E1 no IO for invalid binding; E2 GET only, no proof; E3 one exact
    // DELETE, still Unknown; E4 local exit receipt, no DELETE. Rules: S1 all
    // prerequisites + live => E3; S2 deleting+live => E2; S3 deleting+complete=>E4;
    // S4 !C1=>E1; S5 !C2 / read error=>error without writes; S6 !C3/404=>E2.
    // All negative causes are changed independently from the S1 witness.
    let fixture = Fixture::new(Fault::None);
    let binding = fixture.binding().await;
    for _ in 0..2 {
        assert_eq!(
            fixture.stopper().request_stop(&binding).await.unwrap(),
            PodProcessTermination::Unknown,
            "S1/S2"
        );
    }
    assert_eq!(fixture.facts.lock().unwrap().accepted, 1);
    fixture.exit();
    for _ in 0..2 {
        let PodProcessTermination::Observed(receipt) =
            fixture.stopper().request_stop(&binding).await.unwrap()
        else {
            panic!("S3");
        };
        assert_eq!(receipt.binding(), &binding);
    }
    assert_eq!(fixture.facts.lock().unwrap().accepted, 1);
    let legacy = super::super::bind("ns", "worker", "uid-a", &decode(running())).unwrap();
    // Omission stays byte-compatible with historical opaque bindings; adding
    // retention later never upgrades their original pre-effect provenance.
    let mut encoded = serde_json::to_value(&binding).unwrap();
    encoded.as_object_mut().unwrap().remove("retained");
    let decoded: PodProcessBinding = serde_json::from_value(encoded).unwrap();
    assert_eq!(legacy, decoded);
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("retained")
            .is_none()
    );
    assert_eq!(
        serde_json::to_string(&legacy).unwrap(),
        r#"{"namespace":"ns","name":"worker","uid":"uid-a","node":"node-a","containers":[{"name":"worker","container_id":"containerd://a","started_at":"2026-09-08T00:00:00Z"}]}"#,
        "S4 exact historical wire bytes cannot conflict on a response-loss retry"
    );
    let before = fixture.facts.lock().unwrap().calls.len();
    assert!(fixture.stopper().request_stop(&legacy).await.is_err(), "S4");
    let mut foreign = binding.clone();
    foreign.namespace = "elsewhere".into();
    assert!(
        fixture.stopper().request_stop(&foreign).await.is_err(),
        "S4 foreign namespace"
    );
    assert_eq!(fixture.facts.lock().unwrap().calls.len(), before);
    for (pointer, value) in [
        ("/metadata/uid", json!("uid-b")),
        ("/spec/nodeName", json!("node-b")),
        ("/spec/restartPolicy", json!("Always")),
        (
            "/status/containerStatuses/0/containerID",
            json!("containerd://b"),
        ),
        (
            "/status/containerStatuses/0/state/running/startedAt",
            json!("2026-09-08T00:00:01Z"),
        ),
    ] {
        let mut pod = running();
        *pod.pointer_mut(pointer).unwrap() = value;
        fixture.facts.lock().unwrap().pod = Some(retained(pod));
        assert_eq!(
            fixture.stopper().request_stop(&binding).await.unwrap(),
            PodProcessTermination::Unknown,
            "S6 {pointer}"
        );
    }
    fixture.facts.lock().unwrap().pod = Some(decode(running()));
    assert!(
        fixture.stopper().request_stop(&binding).await.is_err(),
        "S5 retention lost"
    );
    for code in [403, 404, 429, 500] {
        fixture.facts.lock().unwrap().read_status = code;
        let observed = fixture.stopper().request_stop(&binding).await;
        if code == 404 {
            assert_eq!(observed.unwrap(), PodProcessTermination::Unknown);
        } else {
            assert!(observed.is_err(), "S5 {code}");
        }
    }
    assert_eq!(
        fixture.facts.lock().unwrap().accepted,
        1,
        "S4..S6 zero further writes"
    );

    // S11 zero grace => positive grace; absent/positive grace => preserve the
    // Pod policy. No branch requests force deletion or removes any finalizer.
    for grace in [None, Some(0), Some(15)] {
        let fixture = Fixture::new(Fault::None);
        fixture
            .facts
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .spec
            .as_mut()
            .unwrap()
            .termination_grace_period_seconds = grace;
        let binding = fixture.binding().await;
        assert_eq!(
            fixture.stopper().request_stop(&binding).await.unwrap(),
            PodProcessTermination::Unknown
        );
        let state = fixture.facts.lock().unwrap();
        let body = &state.calls.last().unwrap().1;
        assert_eq!(
            body.get("gracePeriodSeconds").and_then(Value::as_u64),
            if grace == Some(0) { Some(1) } else { None }
        );
    }
}

#[tokio::test]
async fn stop_response_loss_and_cas_conflicts_never_retarget_or_forge_completion() {
    // S7 response lost before/after acceptance => retry same subject; exactly
    // one accepted DELETE, Unknown until runtime exit. S8 revision conflict =>
    // re-read then exact retry. S9 replacement/retention loss between GET and
    // DELETE => conflict and no accepted write; subsequent read cannot redirect.
    // S14 cross every fault with exit-before-stop: a previously finished process
    // cannot bypass any CAS, retention or lost-response branch. After an exact
    // accepted deletion, an independent retry may expose that already-known exit.
    for already_exited in [false, true] {
        for fault in [
            Fault::BeforeDelete,
            Fault::AfterDelete,
            Fault::Conflict,
            Fault::Replacement,
            Fault::RetentionLost,
        ] {
            let fixture = Fixture::new(fault);
            let binding = fixture.binding().await;
            if already_exited {
                fixture.exit();
            }
            assert!(fixture.stopper().request_stop(&binding).await.is_err());
            let retry = fixture.stopper().request_stop(&binding).await;
            if matches!(fault, Fault::RetentionLost) {
                assert!(retry.is_err());
            } else if already_exited && matches!(fault, Fault::AfterDelete) {
                assert!(
                    matches!(retry.unwrap(), PodProcessTermination::Observed(_)),
                    "S14 lost accepted response: fresh read proves closure and exit"
                );
            } else {
                assert_eq!(retry.unwrap(), PodProcessTermination::Unknown);
            }
            if already_exited && !matches!(fault, Fault::Replacement | Fault::RetentionLost) {
                assert!(
                    matches!(
                        fixture.stopper().request_stop(&binding).await.unwrap(),
                        PodProcessTermination::Observed(_)
                    ),
                    "S14 exact post-acceptance retry"
                );
            }
            let state = fixture.facts.lock().unwrap();
            let foreign = matches!(fault, Fault::Replacement | Fault::RetentionLost);
            assert_eq!(state.accepted, usize::from(!foreign));
            for (_, request) in state.calls.iter().filter(|(method, _)| method == "DELETE") {
                assert_eq!(request["preconditions"]["uid"], "uid-a");
                assert!(request["preconditions"]["resourceVersion"].is_string());
            }
            assert!(
                state
                    .pod
                    .as_ref()
                    .unwrap()
                    .metadata
                    .finalizers
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|v| v == "other.example/keep")
            );
        }
    }
}

#[tokio::test]
async fn independent_retry_survives_caller_loss_after_stop_acceptance() {
    // S10 caller disappears while the accepted DELETE response is withheld:
    // fresh adapter reads the retained deleting domain, emits no second DELETE,
    // and waits for actual exit. Dropping the request is not stop evidence.
    let fixture = Fixture::new(Fault::PauseAfterDelete);
    let binding = fixture.binding().await;
    let stopper = fixture.stopper();
    let mut stop = Box::pin(stopper.request_stop(&binding));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            result = &mut stop => panic!("response barrier not reached: {result:?}"),
            _ = fixture.accepted.notified() => {}
        }
    })
    .await
    .expect("accepted DELETE barrier");
    drop(stop);
    assert_eq!(
        fixture.stopper().request_stop(&binding).await.unwrap(),
        PodProcessTermination::Unknown
    );
    fixture.exit();
    assert!(matches!(
        fixture.stopper().request_stop(&binding).await.unwrap(),
        PodProcessTermination::Observed(_)
    ));
    assert_eq!(fixture.facts.lock().unwrap().accepted, 1);
    fixture.release.add_permits(1);
}
