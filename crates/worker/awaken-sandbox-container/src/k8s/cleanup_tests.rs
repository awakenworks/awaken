//! Real kube HTTP boundaries, with a deterministic API response-loss scheduler.
use super::*;
use pc::test_support::EffectAuthority;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};

struct ApiState {
    pod: Option<Pod>,
    claim: Option<PersistentVolumeClaim>,
    calls: Vec<String>,
    writes: Vec<String>,
    projected: std::collections::HashSet<String>,
    release_conflict: bool,
}

impl ApiState {
    fn response(&mut self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let resource = path.split('/').nth(5).unwrap();
        let call = format!("{method} {resource}");
        self.calls.push(call.clone());
        let missing = || {
            (
                404,
                json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"NotFound", "message":"gone", "code":404}),
            )
        };
        match (method, resource) {
            ("GET", "pods") => self
                .pod
                .as_ref()
                .map_or_else(missing, |pod| (200, json!(pod))),
            ("GET", "persistentvolumeclaims") => self
                .claim
                .as_ref()
                .map_or_else(missing, |claim| (200, json!(claim))),
            ("PUT", "pods") => {
                let mut replacement: Pod = serde_json::from_value(body).unwrap();
                let old = self.pod.as_ref().unwrap();
                assert_eq!(replacement.metadata.uid, old.metadata.uid);
                assert_eq!(
                    replacement.metadata.resource_version,
                    old.metadata.resource_version
                );
                let releasing = !continuation::pod_cleanup_gate_held(&replacement);
                if releasing && self.release_conflict {
                    self.release_conflict = false;
                    return (
                        409,
                        json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"Conflict", "message":"retry CAS", "code":409}),
                    );
                }
                self.writes.push(call);
                let version: u64 = old
                    .metadata
                    .resource_version
                    .as_ref()
                    .unwrap()
                    .parse()
                    .unwrap();
                replacement.metadata.resource_version = Some((version + 1).to_string());
                self.pod = if releasing {
                    None
                } else {
                    Some(replacement.clone())
                };
                if releasing {
                    self.claim = None;
                }
                (200, json!(replacement))
            }
            ("DELETE", "pods" | "persistentvolumeclaims") => {
                let metadata = if resource == "pods" {
                    &mut self.pod.as_mut().unwrap().metadata
                } else {
                    &mut self.claim.as_mut().unwrap().metadata
                };
                assert_eq!(body["preconditions"]["uid"], json!(metadata.uid));
                assert_eq!(
                    body["preconditions"]["resourceVersion"],
                    json!(metadata.resource_version)
                );
                metadata.deletion_timestamp =
                    Some(serde_json::from_value(json!("2026-09-07T00:00:00Z")).unwrap());
                self.writes.push(call);
                (
                    200,
                    json!({"kind":"Status", "apiVersion":"v1", "status":"Success"}),
                )
            }
            ("GET", "configmaps" | "secrets") => {
                let kind = if resource == "configmaps" {
                    "ConfigMap"
                } else {
                    "Secret"
                };
                let items: Vec<_> = (0..2).filter(|index| self.projected.contains(&format!("{resource}-{index}"))).map(|index| json!({
                    "kind":kind, "apiVersion":"v1", "metadata": {
                        "name":format!("{resource}-{index}"), "uid":format!("{resource}-{index}-uid"), "resourceVersion":"1",
                        "annotations":{SANDBOX_ATTEMPT_ANNOTATION:"attempt"},
                        "labels":{crate::SANDBOX_ATTEMPT_LABEL:k8s_attempt_label("attempt")},
                        "ownerReferences":[{"apiVersion":"v1", "kind":"Pod", "name":"awaken-cleanup", "uid":"pod-uid", "controller":true}]
                    }
                })).collect();
                (
                    200,
                    json!({"kind":format!("{kind}List"), "apiVersion":"v1", "metadata":{}, "items":items}),
                )
            }
            ("DELETE", "configmaps" | "secrets") => {
                let name = path.split('/').nth(6).unwrap();
                assert_eq!(body["preconditions"]["uid"], format!("{name}-uid"));
                assert_eq!(body["preconditions"]["resourceVersion"], "1");
                assert!(self.projected.remove(name));
                self.writes.push(call);
                (
                    200,
                    json!({"kind":"Status", "apiVersion":"v1", "status":"Success"}),
                )
            }
            _ => panic!("unexpected cleanup request {method} {path}: {body}"),
        }
    }
}

async fn request(stream: &mut TcpStream) -> (String, String, Value) {
    let mut bytes = Vec::new();
    let (header, start) = loop {
        let mut buf = [0; 4096];
        let read = stream.read(&mut buf).await.unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&buf[..read]);
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break (String::from_utf8(bytes[..end].to_vec()).unwrap(), end + 4);
        }
    };
    let length: usize = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.trim().parse().unwrap());
    while bytes.len() < start + length {
        let mut buf = [0; 4096];
        let read = stream.read(&mut buf).await.unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&buf[..read]);
    }
    let mut parts = header.lines().next().unwrap().split_whitespace();
    let method = parts.next().unwrap().to_owned();
    let path = parts.next().unwrap().split('?').next().unwrap().to_owned();
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[start..start + length]).unwrap()
    };
    (method, path, body)
}

async fn fixture(
    cut: usize,
    conflict: bool,
) -> (
    K8sRuntime,
    Arc<Mutex<ApiState>>,
    Arc<Notify>,
    Arc<Semaphore>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut runtime = K8sRuntime::for_test(addr);
    runtime.clients.control =
        Client::try_from(kube::Config::new(format!("http://{addr}").parse().unwrap())).unwrap();
    let volume = crate::K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    };
    let mut claim = continuation::build_claim("cleanup", &volume).unwrap();
    claim.metadata.uid = Some("claim-uid".into());
    claim.metadata.resource_version = Some("1".into());
    let mut pod: Pod = serde_json::from_value(json!({"metadata": {
        "name":"awaken-cleanup", "uid":"pod-uid", "resourceVersion":"1", "annotations": {SANDBOX_ATTEMPT_ANNOTATION:"attempt"}
    }, "spec":{"containers":[], "volumes":[{"name":pod_projection::CONTINUATION_VOLUME, "persistentVolumeClaim":{"claimName":"awc-cleanup"}}]}})).unwrap();
    continuation::bind_claim_uid(&mut pod, "claim-uid");
    runtime.continuation_volume = Some(volume);
    let state = Arc::new(Mutex::new(ApiState {
        pod: Some(pod),
        claim: Some(claim),
        calls: vec![],
        writes: vec![],
        projected: ["configmaps-0", "configmaps-1", "secrets-0", "secrets-1"]
            .map(str::to_owned)
            .into_iter()
            .collect(),
        release_conflict: conflict,
    }));
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let (serving, notify, resume) = (state.clone(), reached.clone(), release.clone());
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (method, path, body) = request(&mut stream).await;
            let (status, response, count) = {
                let mut state = serving.lock().unwrap();
                let (status, response) = state.response(&method, &path, body);
                (status, response.to_string(), state.calls.len())
            };
            if count == cut {
                notify.notify_one();
                let _permit = resume.acquire().await.unwrap();
            }
            let reply = format!(
                "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            // Cancellation may close the response socket after API acceptance.
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
    (runtime, state, reached, release, server)
}

#[tokio::test]
async fn cleanup_requires_both_current_authority_and_exact_incarnation() {
    // KCE causes: C1 current/denied authority; C2 exact/missing/foreign Pod UID;
    // C3 exact/foreign PVC UID. KCE1 denied -> zero HTTP even for exact identity;
    // KCE2 current + missing/foreign identity -> one read, no writes. Positive
    // C1+C2+C3 is KCA1. Identity never substitutes for current authority, nor
    // may an authority grant bypass the existing immutable-incarnation gates.
    for row in ["denied", "missing", "foreign-pod", "foreign-claim"] {
        let authorization = crate::tests::disposal_authorization(
            &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        let (runtime, state, _, _, server) = fixture(0, false).await;
        let authority = if row == "denied" {
            EffectAuthority::denied()
        } else {
            EffectAuthority::new(authorization.effect_fence())
        };
        let handle = if row == "missing" {
            None
        } else {
            Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: if row == "foreign-pod" {
                    "foreign"
                } else {
                    "pod-uid"
                }
                .into(),
                claim_uid: Some(
                    if row == "foreign-claim" {
                        "foreign"
                    } else {
                        "claim-uid"
                    }
                    .into(),
                ),
            })
        };
        assert!(
            runtime
                .remove_exact_incarnation(
                    "awaken-cleanup",
                    handle.as_ref(),
                    &authorization,
                    &authority
                )
                .await
                .is_err(),
            "KCE {row}"
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let state = state.lock().unwrap();
        assert_eq!(state.calls.len(), usize::from(row != "denied"));
        assert!(state.writes.is_empty());
        assert!(!continuation::pod_cleanup_gate_held(
            state.pod.as_ref().unwrap()
        ));
        assert!(
            state
                .claim
                .as_ref()
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_none()
        );
        assert_eq!(state.projected.len(), 4);
    }
}

#[tokio::test]
async fn current_authority_is_required_after_every_kubernetes_cleanup_response() {
    /* KCA cause/effect graph: exact Pod+PVC+projected resources -> GET/CAS/
     * DELETE/wait -> delayed response -> authority stable/renew/revoke/replace/
     * expire or caller cancellation -> continue or preserve the committed prefix.
     * Cross every actual HTTP response with all changes, and cross the finalizer
     * release 409 retry. KCA1 stable/renew reaches exact total absence; KCA2 lost
     * authority returns error and emits NO subsequent request or success; KCA3
     * cancellation emits no subsequent request. A mutation accepted before the
     * cut remains committed; neither cancellation nor revocation may be claimed
     * to undo it. UID+resourceVersion preconditions are asserted at every write.
     * The successful path must include both projected resource kinds, both
     * entries of each deletion loop, Pod/PVC deletion and finalizer acquire/release.
     * KCA4 retry uses a NEW successor grant over the original preparation and
     * persisted handle, never revives the rejected proof or recreates resources.
     * This is adapter wiring proof, not a real cluster/controller qualification.
     */
    for conflict in [false, true] {
        let mut boundaries = 1;
        for cut in 0..40 {
            if cut > boundaries {
                break;
            }
            for change in if cut == 0 {
                &['s'][..]
            } else {
                &['r', 'v', 'g', 'e', 'c'][..]
            } {
                let authorization = crate::tests::disposal_authorization(
                    &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1)
                        .unwrap(),
                );
                let authority = Arc::new(EffectAuthority::new(authorization.effect_fence()));
                let (runtime, state, reached, release, server) = fixture(cut, conflict).await;
                let runtime = Arc::new(runtime);
                let handle =
                    continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap())
                        .unwrap();
                let current = authority.clone();
                let (executing, authorized, bound) =
                    (runtime.clone(), authorization.clone(), handle.clone());
                let task = tokio::spawn(async move {
                    executing
                        .remove_exact_incarnation(
                            "awaken-cleanup",
                            bound.as_ref(),
                            &authorized,
                            current.as_ref(),
                        )
                        .await
                });
                let mut task = task;
                let mut writes_at_cut = vec![];
                if cut > 0 {
                    tokio::time::timeout(Duration::from_secs(5), reached.notified())
                        .await
                        .expect("KCA selected HTTP cut must be reached");
                    writes_at_cut = state.lock().unwrap().writes.clone();
                    if *change == 'c' {
                        task.abort();
                    } else {
                        let mut current = authority.current.lock().unwrap();
                        match change {
                            'r' => current.as_mut().unwrap().0.expires_at_unix_ms += 1,
                            'v' => *current = None,
                            'g' => current.as_mut().unwrap().0.epoch += 1,
                            'e' => current.as_mut().unwrap().1 = Duration::ZERO,
                            _ => unreachable!(),
                        }
                    }
                    release.add_permits(1);
                }
                let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .expect("KCA cleanup bounded");
                {
                    let state = state.lock().unwrap();
                    if cut == 0 || *change == 'r' {
                        result.unwrap().unwrap();
                        assert!(state.pod.is_none() && state.claim.is_none());
                        assert!(state.projected.is_empty());
                        assert_eq!(
                            state.writes,
                            [
                                "PUT pods",
                                "DELETE persistentvolumeclaims",
                                "DELETE pods",
                                "DELETE configmaps",
                                "DELETE configmaps",
                                "DELETE secrets",
                                "DELETE secrets",
                                "PUT pods"
                            ]
                        );
                        boundaries = state.calls.len();
                        assert_eq!(boundaries, if conflict { 20 } else { 18 });
                    } else {
                        if *change == 'c' {
                            assert!(result.unwrap_err().is_cancelled());
                        } else {
                            assert!(result.unwrap().is_err(), "KCA2 {cut}/{change}");
                        }
                        assert_eq!(
                            state.calls.len(),
                            cut,
                            "KCA no next HTTP request: {cut}/{change}"
                        );
                        assert_eq!(state.writes, writes_at_cut, "KCA no next mutation");
                    }
                }
                if cut > 0 && *change != 'r' {
                    let mut successor = authorization.effect_fence().clone();
                    successor.epoch += 1;
                    let retry = authorization.preparation().authorize(successor).unwrap();
                    let fresh = EffectAuthority::new(retry.effect_fence());
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        runtime.remove_exact_incarnation(
                            "awaken-cleanup",
                            handle.as_ref(),
                            &retry,
                            &fresh,
                        ),
                    )
                    .await
                    .expect("KCA4 exact retry bounded")
                    .unwrap();
                    let state = state.lock().unwrap();
                    assert!(
                        state.pod.is_none() && state.claim.is_none() && state.projected.is_empty(),
                        "KCA4 terminal outcome {cut}/{change}"
                    );
                }
                server.abort();
                assert!(
                    server.await.unwrap_err().is_cancelled(),
                    "fixture server must not panic"
                );
            }
        }
    }
}
