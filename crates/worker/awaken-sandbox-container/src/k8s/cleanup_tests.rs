// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Real kube HTTP boundaries, with one deterministic API response-loss scheduler
//! shared by cleanup and checkpoint participant tests.
use super::*;
use awaken_provisioning_contract::test_support::FenceTamper as _;
use pc::test_support::EffectAuthority;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};

struct ApiState {
    server_addr: std::net::SocketAddr,
    pod: Option<Pod>,
    claim: Option<PersistentVolumeClaim>,
    pod_posted: Arc<Semaphore>,
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
            ("POST", "pods") => {
                if self.pod.is_some() {
                    return (
                        409,
                        json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"AlreadyExists", "message":"occupied", "code":409}),
                    );
                }
                let mut pod: Pod = serde_json::from_value(body).unwrap();
                pod.metadata.uid = Some("delayed-create-uid".into());
                pod.metadata.resource_version = Some("1".into());
                self.pod = Some(pod.clone());
                self.writes.push(call);
                self.pod_posted.add_permits(1);
                (201, json!(pod))
            }
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
                if replacement.metadata.uid != old.metadata.uid
                    || replacement.metadata.resource_version != old.metadata.resource_version
                {
                    return (
                        409,
                        json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"Conflict", "message":"stale UID or resourceVersion", "code":409}),
                    );
                }
                let releasing = continuation::pod_cleanup_gate_held(old)
                    && !continuation::pod_cleanup_gate_held(&replacement);
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

async fn request(stream: &mut TcpStream) -> Option<(String, String, Value)> {
    let mut bytes = Vec::new();
    let (header, start) = loop {
        let mut buf = [0; 4096];
        let read = stream.read(&mut buf).await.unwrap();
        // A cancelled kube request may close after TCP connect but before the
        // API receives a complete HTTP request. It has no cleanup effect.
        if read == 0 {
            return None;
        }
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
        if read == 0 {
            return None;
        }
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
    Some((method, path, body))
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
    fixture_with_cut(cut, conflict, false).await
}

async fn fixture_with_cut(
    cut: usize,
    conflict: bool,
    pause_before_effect: bool,
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
        server_addr: addr,
        pod: Some(pod),
        claim: Some(claim),
        pod_posted: Arc::new(Semaphore::new(0)),
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
            let Some((method, path, body)) = request(&mut stream).await else {
                continue;
            };
            if pause_before_effect && serving.lock().unwrap().calls.len() + 1 == cut {
                notify.notify_one();
                let _permit = resume.acquire().await.unwrap();
            }
            let (status, response, count) = {
                let mut state = serving.lock().unwrap();
                let (status, response) = state.response(&method, &path, body);
                (status, response.to_string(), state.calls.len())
            };
            if !pause_before_effect && count == cut {
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
async fn cancelled_connections_before_http_acceptance_leave_cleanup_available() {
    // Cause/effect table: C1 TCP closes before headers, during headers, or
    // during a declared body; C2 a later exact cleanup has live authority.
    // R1=C1 => no API call or mutation is accepted; R2=C1+C2 => the same
    // fixture serves the complete Pod/PVC/projected-resource cleanup. The
    // cancelled transport is not a Kubernetes response or an effect receipt.
    let (runtime, state, _, _, server) = fixture(0, false).await;
    let address = state.lock().unwrap().server_addr;
    for partial in [
        &b""[..],
        &b"GET /api HTTP/1.1\r\n"[..],
        &b"POST /api HTTP/1.1\r\nContent-Length: 2\r\n\r\n{"[..],
    ] {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(partial).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    let authorization = crate::tests::disposal_authorization(
        &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
    );
    let authority = EffectAuthority::new(authorization.effect_fence());
    let handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap()).unwrap();
    runtime
        .remove_exact_incarnation(
            "awaken-cleanup",
            handle.as_ref(),
            &authorization,
            &authority,
        )
        .await
        .unwrap();
    {
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls.len(),
            18,
            "R1/R2 only complete HTTP requests count"
        );
        assert_eq!(state.writes.len(), 8, "R2 exact cleanup effects");
        assert!(state.pod.is_none() && state.claim.is_none() && state.projected.is_empty());
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn delayed_pod_create_cannot_be_closed_by_a_replacement_runtime() {
    // Cause/effect table for the real kube HTTP edge. C1 the original runtime
    // sends Pod POST under its exact attempt gate; C2 the API pauses before
    // accepting POST or after accepting it but before returning the response;
    // C3 the Worker process dies without persisting a handle; C4 a replacement
    // runtime observes the same scope/fence. E1 C1+C2+C4 cannot certify
    // absence; E2 C1+C2+C3 later permits the old POST to materialize despite
    // the replacement's earlier observation; E3 no cleanup write is forged.
    // The existing durable-handle and rich-topology tests own their separate
    // completion rules. The child executes the production K8sRuntime request
    // in another OS process against this test's controlled kube HTTP edge.
    // This establishes the fail-closed cut, not a durable closure witness.
    if let Ok(addr) = std::env::var("AWAKEN_K8S_CREATE_CHILD_ADDR") {
        let addr: std::net::SocketAddr = addr.parse().unwrap();
        let mut runtime = K8sRuntime::for_test(addr);
        runtime.clients.control =
            Client::try_from(kube::Config::new(format!("http://{addr}").parse().unwrap())).unwrap();
        let fingerprint: pc::SandboxRealizationFingerprint =
            serde_json::from_value(json!("delayed-create-fingerprint")).unwrap();
        let fence = pc::SandboxEffectFence::new(
            "create-operation",
            "worker-owner",
            "worker-boot",
            7,
            u64::MAX,
        )
        .unwrap();
        let intent = ContainerRealizationIntent::Create;
        let attempt = crate::fresh_create_attempt();
        let context = ContainerRealizationContext::new(
            "scope",
            &fingerprint,
            Some(&fence),
            &intent,
            &attempt,
        );
        runtime
            .prepare_for_effect(&context, &ephemeral_plan(), &fingerprint)
            .await
            .expect("child API request");
        panic!("the child unexpectedly completed before its forced process death");
    }
    for pause_before_effect in [true, false] {
        let (runtime, state, reached, release, server) =
            fixture_with_cut(2, false, pause_before_effect).await;
        let (posted, server_addr) = {
            let mut state = state.lock().unwrap();
            state.pod = None;
            state.claim = None;
            state.projected.clear();
            (state.pod_posted.clone(), state.server_addr)
        };
        let mut replacement = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        replacement.clients.control = runtime.clients.control.clone();
        drop(runtime);
        let fence = pc::SandboxEffectFence::new(
            "create-operation",
            "worker-owner",
            "worker-boot",
            7,
            u64::MAX,
        )
        .unwrap();
        let mut creating = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "k8s::cleanup_tests::delayed_pod_create_cannot_be_closed_by_a_replacement_runtime",
                "--nocapture",
            ])
            .env("AWAKEN_K8S_CREATE_CHILD_ADDR", server_addr.to_string())
            .spawn()
            .expect("C3 start one disposable Worker process");
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .expect("C1 actual Pod POST reached the selected API cut");
        assert_eq!(
            state.lock().unwrap().pod.is_some(),
            !pause_before_effect,
            "C2 API acceptance and response delivery are separate cuts"
        );
        assert!(
            !replacement
                .prove_unmaterialized_absence("scope", &ephemeral_plan(), Some(&fence))
                .await
                .unwrap(),
            "E1 replacement has no observed-attempt closure witness"
        );
        creating.kill().expect("C3 force-stop Worker process");
        assert!(
            !creating.wait().unwrap().success(),
            "C3 Worker process died"
        );
        release.add_permits(1);
        let permit = tokio::time::timeout(Duration::from_secs(5), posted.acquire())
            .await
            .expect("E2 delayed API POST eventually applies")
            .unwrap();
        drop(permit);
        {
            let state = state.lock().unwrap();
            assert!(state.pod.is_some(), "E2 late Pod exists after Worker loss");
            assert_eq!(state.writes, vec!["POST pods"], "E3 no cleanup was forged");
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[derive(Clone)]
struct CheckpointEvidence {
    source: pc::SandboxEffectFence,
    fingerprint: pc::SandboxRealizationFingerprint,
    handle: crate::ContainerContinuationHandle,
}

impl CheckpointEvidence {
    fn initialize(state: &Mutex<ApiState>) -> Self {
        let source =
            pc::SandboxEffectFence::new("suspend", "owner", "boot", 2, u64::MAX - 1).unwrap();
        let fingerprint: pc::SandboxRealizationFingerprint =
            serde_json::from_value(json!("checkpoint-fixture-realization")).unwrap();
        let mut state = state.lock().unwrap();
        let pod = state.pod.as_mut().unwrap();
        let annotations = pod.metadata.annotations.as_mut().unwrap();
        annotations.insert(SANDBOX_ADOPTION_ANNOTATION.into(), fingerprint.to_string());
        annotations.insert(
            SANDBOX_REALIZATION_ANNOTATION.into(),
            fingerprint.to_string(),
        );
        stamp_effect_fence(
            pod,
            &pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        pod.status = Some(serde_json::from_value(json!({"phase":"Running", "containerStatuses":[{
            "name":"agent", "ready":true, "restartCount":0, "image":"fixture", "imageID":"fixture", "state":{"running":{}}
        }]})).unwrap());
        let handle = continuation::handle_extra(pod).unwrap().unwrap();
        Self {
            source,
            fingerprint,
            handle,
        }
    }

    async fn apply(
        &self,
        runtime: &K8sRuntime,
        active: &pc::SandboxEffectFence,
        authority: &EffectAuthority,
        step: usize,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, RuntimeError> {
        let pending =
            pc::SandboxCheckpointUpload::pending("suspend".into(), "request", "digest", 4);
        let reference = crate::checkpoint::tests::receipt();
        let mutation = match step {
            0 => crate::ContainerCheckpointMutation::Acquire,
            1 => crate::ContainerCheckpointMutation::Begin(&pending),
            2 => crate::ContainerCheckpointMutation::Complete(&reference),
            _ => unreachable!(),
        };
        runtime
            .checkpoint_upload(
                ContainerObservationExpectation {
                    purpose: ContainerObservationPurpose::Recovery,
                    container_id: "awaken-cleanup",
                    adoption_fingerprint: Some(&self.fingerprint),
                    realization_fingerprint: Some(&self.fingerprint),
                    runtime_handle: Some(&self.handle),
                    effect_fence: Some(active),
                    retained_attempt: None,
                },
                crate::ContainerCheckpointAccess {
                    source_effect_fence: &self.source,
                    effect_fence: active,
                    request_fingerprint: "request",
                    authority,
                },
                mutation,
            )
            .await
    }
}

#[tokio::test]
async fn checkpoint_http_replay_and_terminal_takeover_share_one_pod_record() {
    // KH1 acquire/begin/complete -> three UID+RV writes to the SAME Pod;
    // KH2 fresh runtime replays each exact mutation -> reads only, same receipt;
    // KH3 terminal takeover at SAME epoch -> one fence+participant CAS;
    // KH4 old Suspend (even with a locally live grant) -> no write or success.
    // Neither path acquires a cleanup finalizer, writes PVC, creates resources,
    // uploads bytes, deletes objects, or claims Session completion.
    let (runtime, state, _, _, server) = fixture(0, false).await;
    let evidence = CheckpointEvidence::initialize(&state);
    let authority = EffectAuthority::new(&evidence.source);
    for step in 0..3 {
        evidence
            .apply(&runtime, &evidence.source, &authority, step)
            .await
            .unwrap();
    }
    assert_eq!(state.lock().unwrap().writes, vec!["PUT pods"; 3]);
    let mut restarted = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    restarted.clients.control = runtime.clients.control.clone();
    drop(runtime);
    let expected = crate::checkpoint::tests::receipt();
    for step in 0..3 {
        let replay = evidence
            .apply(&restarted, &evidence.source, &authority, step)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.reference(), Some(&expected), "KH2");
    }
    assert_eq!(
        state.lock().unwrap().writes.len(),
        3,
        "KH2 no duplicate writes"
    );
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "boot", 2, u64::MAX - 1).unwrap();
    let terminal_authority = EffectAuthority::new(&terminal);
    for step in 0..3 {
        evidence
            .apply(&restarted, &terminal, &terminal_authority, step)
            .await
            .unwrap();
    }
    assert_eq!(state.lock().unwrap().writes.len(), 4, "KH3");
    for step in 0..3 {
        assert!(
            evidence
                .apply(&restarted, &evidence.source, &authority, step)
                .await
                .is_err(),
            "KH4"
        );
    }
    {
        let state = state.lock().unwrap();
        assert_eq!(state.writes.len(), 4);
        assert!(state.claim.is_some() && state.projected.len() == 4);
        assert!(!continuation::pod_cleanup_gate_held(
            state.pod.as_ref().unwrap()
        ));
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn checkpoint_current_authority_is_reentered_after_each_http_response() {
    // KA graph: acquire/begin/complete -> GET Pod -> GET PVC -> Pod CAS;
    // C1 pause after each actual response; C2 renew/revoke/replace/expire/cancel.
    // KA1 renewal preserves the admitted effect and completes; KA2 all other
    // rows return no success and send no later request. A CAS already accepted
    // before the pause remains durable. KA3 a new successor grant replays the
    // original input over that prefix, never allocates a second participant.
    for step in 0..3 {
        for cut in 1..=3 {
            for change in ['r', 'v', 'g', 'e', 'c'] {
                let (runtime, state, reached, release, server) =
                    fixture(step * 3 + cut, false).await;
                let evidence = CheckpointEvidence::initialize(&state);
                let authority = Arc::new(EffectAuthority::new(&evidence.source));
                for seed in 0..step {
                    evidence
                        .apply(&runtime, &evidence.source, &authority, seed)
                        .await
                        .unwrap();
                }
                let runtime = Arc::new(runtime);
                let (executing, proof, inputs) =
                    (runtime.clone(), authority.clone(), evidence.clone());
                let mut task = tokio::spawn(async move {
                    inputs.apply(&executing, &inputs.source, &proof, step).await
                });
                tokio::time::timeout(Duration::from_secs(5), reached.notified())
                    .await
                    .expect("KA selected response reached");
                let (calls, writes) = {
                    let state = state.lock().unwrap();
                    (state.calls.len(), state.writes.clone())
                };
                if change == 'c' {
                    task.abort();
                } else {
                    let mut current = authority.current.lock().unwrap();
                    match change {
                        'r' => current.as_mut().unwrap().0.renew(),
                        'v' => *current = None,
                        'g' => current.as_mut().unwrap().0.reassign(),
                        'e' => current.as_mut().unwrap().1 = Duration::ZERO,
                        _ => unreachable!(),
                    }
                }
                release.add_permits(1);
                let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .expect("KA bounded result");
                if change == 'r' {
                    result.unwrap().unwrap();
                } else {
                    if change == 'c' {
                        assert!(result.unwrap_err().is_cancelled());
                    } else {
                        assert!(result.unwrap().is_err());
                    }
                    let state = state.lock().unwrap();
                    assert_eq!(state.calls.len(), calls, "KA2/{step}/{cut}/{change}");
                    assert_eq!(state.writes, writes, "KA2 retains committed prefix");
                }
                let mut successor = evidence.source.clone();
                successor.reassign();
                let fresh = EffectAuthority::new(&successor);
                evidence
                    .apply(&runtime, &successor, &fresh, step)
                    .await
                    .unwrap();
                {
                    let state = state.lock().unwrap();
                    assert!(state.pod.is_some() && state.claim.is_some());
                    assert!(state.writes.iter().all(|write| write == "PUT pods"));
                }
                server.abort();
                assert!(server.await.unwrap_err().is_cancelled());
            }
        }
    }
}

#[tokio::test]
async fn checkpoint_cas_cannot_overwrite_a_concurrent_pod_revision_or_incarnation() {
    // KC1 read exact Pod then concurrent metadata change/replacement -> stale
    // CAS rejects; KC2 no participant is published over either newer object.
    // This uses the real kube request serializer and HTTP 409, not a pure mock
    // result. The shared fixture checks BOTH UID and resourceVersion.
    for replaced in [false, true] {
        let (runtime, state, reached, release, server) = fixture(1, false).await;
        let evidence = CheckpointEvidence::initialize(&state);
        let task = tokio::spawn(async move {
            evidence
                .apply(
                    &runtime,
                    &evidence.source,
                    &EffectAuthority::new(&evidence.source),
                    0,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        let expected = {
            let mut state = state.lock().unwrap();
            let pod = state.pod.as_mut().unwrap();
            if replaced {
                pod.metadata.uid = Some("replacement".into());
            } else {
                pod.metadata.resource_version = Some("2".into());
            }
            pod.clone()
        };
        release.add_permits(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(state.lock().unwrap().pod.as_ref(), Some(&expected), "KC2");
        assert!(state.lock().unwrap().writes.is_empty());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn checkpoint_identity_and_source_state_denials_have_zero_writes() {
    // KI graph: current grant AND complete matching physical evidence AND
    // ungated live source AND valid participant => admission. KH is the positive
    // row; KI1 negates each independent prerequisite -> no write. KI2 denied
    // current authority -> no HTTP at all. KI3 a vanished Pod/PVC is an error,
    // never invented upload completion. Corrupt record bytes are not authority.
    for row in [
        "denied",
        "legacy",
        "pod-missing",
        "pod-uid",
        "pod-version",
        "adoption",
        "realization",
        "fence-missing",
        "fence-newer",
        "pod-deleting",
        "cleanup-gate",
        "claim-missing",
        "claim-uid",
        "claim-deleting",
        "corrupt",
    ] {
        let (runtime, state, _, _, server) = fixture(0, false).await;
        let mut evidence = CheckpointEvidence::initialize(&state);
        let authority = if row == "denied" {
            EffectAuthority::denied()
        } else {
            EffectAuthority::new(&evidence.source)
        };
        {
            let mut state = state.lock().unwrap();
            let deleting = || serde_json::from_value(json!("2026-09-08T00:00:00Z")).unwrap();
            match row {
                "legacy" => {
                    evidence.handle = crate::ContainerContinuationHandle::KubernetesContinuation {
                        claim_uid: "claim-uid".into(),
                    }
                }
                "pod-missing" => state.pod = None,
                "pod-uid" => state.pod.as_mut().unwrap().metadata.uid = Some("foreign".into()),
                "pod-version" => state.pod.as_mut().unwrap().metadata.resource_version = None,
                "adoption" | "realization" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .remove(if row == "adoption" {
                            SANDBOX_ADOPTION_ANNOTATION
                        } else {
                            SANDBOX_REALIZATION_ANNOTATION
                        });
                }
                "fence-missing" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .remove(SANDBOX_EFFECT_ANNOTATION);
                }
                "fence-newer" => {
                    let mut newer = evidence.source.clone();
                    newer.reassign();
                    stamp_effect_fence(state.pod.as_mut().unwrap(), &newer);
                }
                "pod-deleting" => {
                    state.pod.as_mut().unwrap().metadata.deletion_timestamp = Some(deleting())
                }
                "cleanup-gate" => {
                    // Inconsistent gate evidence must also reject; the canonical
                    // continuation validator owns both annotation/finalizer bits.
                    state.pod.as_mut().unwrap().metadata.finalizers =
                        Some(vec![continuation::CONTINUATION_CLEANUP_FINALIZER.into()]);
                }
                "claim-missing" => state.claim = None,
                "claim-uid" => state.claim.as_mut().unwrap().metadata.uid = Some("foreign".into()),
                "claim-deleting" => {
                    state.claim.as_mut().unwrap().metadata.deletion_timestamp = Some(deleting())
                }
                "corrupt" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .insert("awaken.dev/checkpoint-upload".into(), "{}".into());
                }
                "denied" => {}
                _ => unreachable!(),
            }
        }
        let before = {
            let state = state.lock().unwrap();
            (state.pod.clone(), state.claim.clone())
        };
        assert!(
            evidence
                .apply(&runtime, &evidence.source, &authority, 0)
                .await
                .is_err(),
            "KI/{row}"
        );
        {
            let state = state.lock().unwrap();
            assert_eq!(
                (state.pod.clone(), state.claim.clone()),
                before,
                "KI1/{row}"
            );
            assert!(state.writes.is_empty());
            if row == "denied" {
                assert!(state.calls.is_empty(), "KI2");
            }
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
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
            Some(
                crate::ContainerContinuationHandle::KubernetesContinuationV2 {
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
                },
            )
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
                            'r' => current.as_mut().unwrap().0.renew(),
                            'v' => *current = None,
                            'g' => current.as_mut().unwrap().0.reassign(),
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
                    successor.reassign();
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
