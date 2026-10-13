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

#[tokio::test]
async fn relay_address_uses_the_same_exact_retained_pod_observation() {
    // Causes: R1 one running Pod has the authenticated UID, retained Worker
    // evidence and a typed address; R2 UID replacement; R3 missing retention;
    // R4 missing or malformed address. Effects: E1 one IP from one Pod GET;
    // E2 no route address and no mutation for every other cause.
    // | Rule | UID | retained | Pod IP | Effect |
    // | R1 | exact | yes | valid | E1 |
    // | R2 | wrong | yes | valid | E2 |
    // | R3 | exact | no | valid | E2 |
    // | R4 | exact | yes | missing/invalid | E2 |
    let fixture = Fixture::new(Fault::None);
    let mut pod = running();
    pod["status"]["podIP"] = json!("10.1.2.3");
    fixture.facts.lock().unwrap().pod = Some(retained(pod.clone()));
    let observer = fixture.observer();
    assert_eq!(
        observer.relay_ip_for("worker", "uid-a").await.unwrap(),
        "10.1.2.3".parse::<std::net::IpAddr>().unwrap(),
        "R1/E1"
    );
    assert_eq!(fixture.facts.lock().unwrap().calls.len(), 1, "R1 one GET");
    assert!(
        observer
            .relay_ip_for("worker", "replacement")
            .await
            .is_err(),
        "R2/E2"
    );
    fixture.facts.lock().unwrap().pod = Some(decode(pod.clone()));
    assert!(
        observer.relay_ip_for("worker", "uid-a").await.is_err(),
        "R3/E2"
    );
    for address in [Value::Null, json!("not-an-ip")] {
        let mut missing = pod.clone();
        missing["status"]["podIP"] = address;
        fixture.facts.lock().unwrap().pod = Some(retained(missing));
        assert!(
            observer.relay_ip_for("worker", "uid-a").await.is_err(),
            "R4/E2"
        );
    }
    assert!(
        fixture
            .facts
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(|(method, _)| method == "GET"),
        "E2 read-only observation"
    );
}

#[tokio::test]
async fn registration_retention_is_exact_idempotent_and_rejects_deleting_pods() {
    // T1 original live UID + absent finalizer -> exact RV CAS preserves sibling
    // metadata; T2 repeat -> no second write; T3 deletion/replacement -> no
    // retention and no inferred exit. No Pod template is required to pin a Pod
    // before registration. A fake API tests adapter decisions, not GKE health.
    let fixture = Fixture::new(Fault::None);
    fixture
        .facts
        .lock()
        .unwrap()
        .pod
        .as_mut()
        .unwrap()
        .metadata
        .finalizers = Some(vec!["other.example/keep".into()]);
    fixture
        .observer()
        .retain_pod_process_evidence("worker", "uid-a")
        .await
        .unwrap();
    assert!(evidence_is_retained(
        fixture.facts.lock().unwrap().pod.as_ref().unwrap()
    ));
    let writes = fixture
        .facts
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(m, _)| m == "PUT")
        .count();
    fixture
        .observer()
        .retain_pod_process_evidence("worker", "uid-a")
        .await
        .unwrap();
    assert_eq!(
        fixture
            .facts
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == "PUT")
            .count(),
        writes
    );
    for replacement in [false, true] {
        let fixture = Fixture::new(Fault::None);
        {
            let mut facts = fixture.facts.lock().unwrap();
            let pod = facts.pod.as_mut().unwrap();
            if replacement {
                pod.metadata.uid = Some("uid-b".into());
            } else {
                pod.metadata.deletion_timestamp =
                    Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                        "2026-09-29T00:00:00Z".parse().unwrap(),
                    ));
            }
        }
        assert!(
            fixture
                .observer()
                .retain_pod_process_evidence("worker", "uid-a")
                .await
                .is_err()
        );
        assert!(
            !fixture
                .facts
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|(m, _)| m == "PUT")
        );
    }
}

#[tokio::test]
async fn closed_idle_release_removes_only_the_original_uid_retention_metadata() {
    // I1 trusted closed/no-slot admission is supplied by Coordinator. I2 exact
    // UID + finalizer -> one existing CAS remove, no DELETE/exit receipt. I3
    // replacement UID -> no metadata mutation. This is not a physical ACK.
    let fixture = Fixture::new(Fault::None);
    fixture
        .stopper()
        .release_closed_idle_pod_evidence("worker", "uid-a")
        .await
        .unwrap();
    assert_eq!(fixture.facts.lock().unwrap().released, 1, "I2");
    assert_eq!(fixture.facts.lock().unwrap().accepted, 0, "I2 no stop");
    let fixture = Fixture::new(Fault::None);
    fixture
        .facts
        .lock()
        .unwrap()
        .pod
        .as_mut()
        .unwrap()
        .metadata
        .uid = Some("replacement".into());
    fixture
        .stopper()
        .release_closed_idle_pod_evidence("worker", "uid-a")
        .await
        .unwrap();
    assert_eq!(fixture.facts.lock().unwrap().released, 0, "I3");
}

#[tokio::test]
async fn closed_idle_retirement_closes_restartable_pods_before_releasing_exact_evidence() {
    // Cause/effect graph: C1 Coordinator proved closed admission + no physical
    // slots; C2 exact Pod UID/RV; C3 Always restart; C4 zero configured grace;
    // C5 Worker retention present/previously released; C6 deletion already begun.
    // Rules IR1=C1+C2+C3+C4+!C6 -> one conditional DELETE with positive grace,
    // then remove only C5's Worker finalizer. IR2=C6 -> no duplicate DELETE.
    // C5 is not admission: its absence must not strand an old closed-idle Pod.
    // Result is unit, not a process-exit/Dispatch/Session settlement receipt.
    for retained_evidence in [true, false] {
        let fixture = Fixture::new(Fault::None);
        let mut pod = running();
        pod["spec"]["restartPolicy"] = json!("Always");
        pod["spec"]["terminationGracePeriodSeconds"] = json!(0);
        fixture.facts.lock().unwrap().pod = Some(if retained_evidence {
            retained(pod)
        } else {
            pod["metadata"]["finalizers"] = json!(["other.example/keep"]);
            decode(pod)
        });
        fixture
            .stopper()
            .retire_closed_idle_pod("worker", "uid-a")
            .await
            .unwrap();
        {
            let facts = fixture.facts.lock().unwrap();
            assert_eq!(facts.accepted, 1, "IR1 immutable Pod domain closed");
            assert_eq!(facts.released, usize::from(retained_evidence), "IR1 C5");
            let delete = facts.calls.iter().find(|(m, _)| m == "DELETE").unwrap();
            assert_eq!(
                delete.1["preconditions"],
                json!({"uid":"uid-a", "resourceVersion":"1"})
            );
            assert_eq!(delete.1["gracePeriodSeconds"], 1, "IR1 positive grace");
            let finalizers = facts
                .pod
                .as_ref()
                .unwrap()
                .metadata
                .finalizers
                .as_ref()
                .unwrap();
            assert_eq!(finalizers, &["other.example/keep"], "IR1 sibling retention");
            if retained_evidence {
                assert!(
                    facts.calls.iter().position(|(m, _)| m == "DELETE").unwrap()
                        < facts.calls.iter().position(|(m, _)| m == "PUT").unwrap(),
                    "IR1 close before release"
                );
            }
        }
        fixture
            .stopper()
            .retire_closed_idle_pod("worker", "uid-a")
            .await
            .unwrap();
        assert_eq!(fixture.facts.lock().unwrap().accepted, 1, "IR2");
        assert_eq!(
            fixture.facts.lock().unwrap().released,
            usize::from(retained_evidence),
            "IR2"
        );
    }
}

#[tokio::test]
async fn closed_idle_retirement_retries_api_faults_without_touching_successor_uid() {
    // Causes: F1 DELETE fails before acceptance; F2 response lost after acceptance;
    // F3 exact RV conflicts; F4 UID replaced during DELETE; F5 release response
    // lost; F6 release CAS conflict. Rules RF1(F1/F3) -> error, retain metadata,
    // retry same UID. RF2(F2/F5) -> error, retry observed deletion without another
    // DELETE. RF3(F4) -> conflict then read-only completion for the old UID.
    // RF4(F6) -> existing finalizer CAS retries. No fault creates an exit fact.
    for fault in [
        Fault::BeforeDelete,
        Fault::AfterDelete,
        Fault::Conflict,
        Fault::Replacement,
        Fault::ReleaseResponseLost,
        Fault::ReleaseConflict,
    ] {
        let fixture = Fixture::new(fault);
        let result = fixture
            .stopper()
            .retire_closed_idle_pod("worker", "uid-a")
            .await;
        if matches!(fault, Fault::ReleaseConflict) {
            result.unwrap();
        } else {
            assert!(result.is_err(), "RF1/RF2/RF3");
        }
        if matches!(
            fault,
            Fault::BeforeDelete | Fault::AfterDelete | Fault::Conflict | Fault::Replacement
        ) {
            assert_eq!(
                fixture.facts.lock().unwrap().released,
                0,
                "failed delete retains evidence"
            );
        }
        fixture
            .stopper()
            .retire_closed_idle_pod("worker", "uid-a")
            .await
            .unwrap();
        let facts = fixture.facts.lock().unwrap();
        if matches!(fault, Fault::Replacement) {
            assert_eq!(facts.accepted, 0, "RF3 successor never deleted");
            assert_eq!(facts.released, 0, "RF3 successor retention untouched");
            assert!(evidence_is_retained(facts.pod.as_ref().unwrap()));
        } else {
            assert_eq!(facts.accepted, 1, "RF1/RF2/RF4 one accepted deletion");
            assert_eq!(facts.released, 1, "RF1/RF2/RF4 one release");
        }
    }
}

#[tokio::test]
async fn closed_idle_retirement_missing_or_unavailable_coordinates_never_mutate() {
    // Cause/effect table: RC1 absent Pod -> old domain complete, no mutation;
    // RC2 successor UID -> no mutation; RC3 empty UID or absent/blank RV -> error;
    // RC4 API unavailable -> retryable error. Only IR1's exact observable UID/RV
    // admits deletion. None of RC1..RC4 is a physical-exit assertion.
    for case in [
        "missing",
        "successor",
        "empty-uid",
        "missing-rv",
        "blank-rv",
        "unavailable",
    ] {
        let fixture = Fixture::new(Fault::None);
        {
            let mut facts = fixture.facts.lock().unwrap();
            match case {
                "missing" => facts.pod = None,
                "successor" => facts.pod.as_mut().unwrap().metadata.uid = Some("uid-b".into()),
                "missing-rv" => facts.pod.as_mut().unwrap().metadata.resource_version = None,
                "blank-rv" => {
                    facts.pod.as_mut().unwrap().metadata.resource_version = Some(" ".into())
                }
                "unavailable" => facts.read_status = 503,
                _ => {}
            }
        }
        let uid = if case == "empty-uid" { "" } else { "uid-a" };
        let result = fixture
            .stopper()
            .retire_closed_idle_pod("worker", uid)
            .await;
        assert_eq!(
            result.is_ok(),
            matches!(case, "missing" | "successor"),
            "{case}"
        );
        let facts = fixture.facts.lock().unwrap();
        assert!(
            facts.calls.iter().all(|(m, _)| m == "GET"),
            "{case}: no mutation"
        );
    }
}

#[tokio::test]
async fn registered_worker_replacement_retains_only_exact_delete_intent() {
    // Causes: C1 exact retained running UID; C2 API accepts DELETE, loses its
    // response, or races one resourceVersion change; C3 Pod is already deleting;
    // C4 UID/retention/process projection is foreign or unbound. Effects:
    // E1 one UID/RV-fenced intent, never an exit receipt; E2 replay observes
    // the same intent without another DELETE; E3 C4 cannot mutate any Pod.
    // Decision rules: W1=C1+C2(accept)->E1; W2=C1+C2(lost)->E1+E2;
    // W3=C1+C2(conflict)->retry E1; W4=C3->E2; W5=C4->E3.
    for fault in [Fault::None, Fault::AfterDelete, Fault::Conflict] {
        let fixture = Fixture::new(fault);
        let mut live = running();
        live["spec"]["restartPolicy"] = json!("Always");
        fixture.facts.lock().unwrap().pod = Some(retained(live));
        fixture
            .stopper()
            .request_registered_pod_replacement("worker", "uid-a")
            .await
            .unwrap();
        fixture
            .stopper()
            .request_registered_pod_replacement("worker", "uid-a")
            .await
            .unwrap();
        let facts = fixture.facts.lock().unwrap();
        assert_eq!(facts.accepted, 1, "W1..W4");
        assert!(
            facts
                .pod
                .as_ref()
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_some()
        );
        for (method, body) in &facts.calls {
            if method == "DELETE" {
                assert_eq!(body["preconditions"]["uid"], "uid-a", "E1");
            }
        }
    }
    for corrupt in ["uid", "retention", "restart"] {
        let fixture = Fixture::new(Fault::None);
        let mut live = running();
        live["spec"]["restartPolicy"] = json!("Always");
        match corrupt {
            "uid" => live["metadata"]["uid"] = json!("foreign"),
            "retention" => live["metadata"]["finalizers"] = json!([]),
            "restart" => live["status"]["containerStatuses"][0]["restartCount"] = json!(1),
            _ => unreachable!(),
        }
        fixture.facts.lock().unwrap().pod = Some(decode(live));
        assert!(
            fixture
                .stopper()
                .request_registered_pod_replacement("worker", "uid-a")
                .await
                .is_err(),
            "W5 {corrupt}"
        );
        assert_eq!(fixture.facts.lock().unwrap().accepted, 0, "E3 {corrupt}");
    }
}

#[tokio::test]
async fn deployment_stop_uses_existing_exact_delete_then_observes_original_exits() {
    // C1 frozen retained Always Pod; C2 exact UID/RV; C3 deletion accepted;
    // C4 original containers exited. P1(C1+C2) => one conditional DELETE and
    // Unknown; P2(C1+C2+C3+!C4) => read-only Unknown; P3(C1..C4) => local exit.
    // No response loss/retry may become a new stop operation or Dispatch write;
    // those faults use the same existing stopper matrix below.
    let fixture = Fixture::new(Fault::None);
    let mut live = running();
    live["spec"]["restartPolicy"] = json!("Always");
    fixture.facts.lock().unwrap().pod = Some(retained(live));
    let observer = fixture.observer();
    let binding = observer.bind_pod_process("worker", "uid-a").await.unwrap();
    let stopper = PodProcessStopper::new(observer);
    assert_eq!(
        stopper.request_stop(&binding).await.unwrap(),
        PodProcessTermination::Unknown,
        "P1"
    );
    assert_eq!(
        stopper.request_stop(&binding).await.unwrap(),
        PodProcessTermination::Unknown,
        "P2"
    );
    let mut exited = terminal();
    exited["spec"]["restartPolicy"] = json!("Always");
    exited["metadata"]["deletionTimestamp"] = json!("2026-09-08T00:00:02Z");
    fixture.facts.lock().unwrap().pod = Some(retained(exited));
    assert!(
        matches!(
            stopper.request_stop(&binding).await.unwrap(),
            PodProcessTermination::Observed(_)
        ),
        "P3"
    );
    assert_eq!(fixture.facts.lock().unwrap().accepted, 1);
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
                            // Finalizer faults belong to PUT, even when the same
                            // production operation first requests Pod deletion.
                            let fault = if matches!(
                                state.fault,
                                Fault::ReleaseConflict | Fault::ReleaseResponseLost
                            ) {
                                Fault::None
                            } else {
                                std::mem::take(&mut state.fault)
                            };
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
async fn sandbox_stop_preserves_create_and_continuation_retention_ownership() {
    // DT-RESOURCE-GUARD: C1 pre-entry Create/continuation guard + exact UID;
    // C2 current private process identity; C3 accepted DELETE; C4 exact exits.
    // G1=C1+C2 -> one UID/RV-fenced stop, still Unknown until C4. G2=G1+C4
    // -> local exit receipt and idempotent replay. G3 either resource guard
    // never grants Worker replacement/authentication or Worker guard release.
    // The existing resource lifecycle remains the sole guard/PVC harvest owner.
    for guard in [
        super::super::super::continuation::CONTINUATION_CLEANUP_FINALIZER,
        super::super::super::create_ledger::ORIGINAL_GUARD_FINALIZER,
    ] {
        let fixture = Fixture::new(Fault::None);
        fixture
            .facts
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .metadata
            .finalizers = Some(vec![guard.into()]);
        let binding = fixture.binding().await;
        assert!(binding.evidence_was_retained(), "G1 {guard}");
        assert!(!binding.worker_evidence_was_retained(), "G3 {guard}");
        assert!(
            fixture
                .stopper()
                .request_registered_pod_replacement("worker", "uid-a")
                .await
                .is_err(),
            "G3"
        );
        assert_eq!(
            fixture.facts.lock().unwrap().accepted,
            0,
            "G3 no Worker mutation"
        );
        assert_eq!(
            fixture.stopper().request_stop(&binding).await.unwrap(),
            PodProcessTermination::Unknown,
            "G1"
        );
        fixture.exit();
        for _ in 0..2 {
            assert!(
                matches!(
                    fixture.stopper().request_stop(&binding).await.unwrap(),
                    PodProcessTermination::Observed(_)
                ),
                "G2"
            );
        }
        fixture.stopper().release_evidence(&binding).await.unwrap();
        let facts = fixture.facts.lock().unwrap();
        assert_eq!(facts.accepted, 1, "G1/G2 exact single stop");
        assert_eq!(facts.released, 0, "G3 no resource-guard release");
        assert_eq!(
            facts.pod.as_ref().unwrap().metadata.finalizers.as_deref(),
            Some([guard.to_owned()].as_slice()),
            "G3 original guard retained"
        );
        assert!(
            facts.calls.iter().all(|(method, _)| method != "PUT"),
            "G3 no metadata rewrite"
        );
    }
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
