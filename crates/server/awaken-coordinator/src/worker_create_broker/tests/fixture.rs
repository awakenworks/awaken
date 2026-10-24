use super::*;
use awaken_container_contract::{
    CreateReplyProfile, ISOLATION_ID_ANNOTATION, K8S_CREATE_CAPTURE_HEADER,
    K8S_CREATE_WIRE_VERSION, ORIGINAL_GUARD_FINALIZER, REQUEST_DIGEST_ANNOTATION, RequestJoin,
};
use http_body_util::BodyExt as _;

pub(super) fn empty_heartbeat(sequence: u64, ready: bool) -> WorkerHeartbeat {
    WorkerHeartbeat {
        sequence,
        ready,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
    }
}

pub(super) fn ledger_state(stored: &Mutex<Option<Value>>) -> Value {
    let object = stored.lock().unwrap();
    serde_json::from_str(object.as_ref().unwrap()["data"]["ledger"].as_str().unwrap()).unwrap()
}

pub(super) async fn quiesce_worker(directory: &dyn WorkerDirectory, identity: &WorkerIdentity) {
    assert_eq!(
        directory.begin_drain(identity, 1_100).await.unwrap(),
        awaken_dispatch::RegistryMutation::Applied,
        "real Draining precondition"
    );
    assert_eq!(
        directory.mark_quiesced(identity).await.unwrap(),
        awaken_dispatch::RegistryMutation::Applied,
        "real Quiesced precondition"
    );
}

pub(super) struct NoApiRouteFixture {
    pub(super) router: Router,
    pub(super) directory: Arc<MemoryWorkerDirectory>,
    pub(super) clock: Arc<ManualClock>,
    pub(super) api_calls: Arc<AtomicUsize>,
}

/// The original transport-rejection API spy also owns the zero-effect Seal
/// fixture. Its client never supplies a ledger, exit, or historical proof.
pub(super) fn no_api_route_fixture() -> NoApiRouteFixture {
    let api_calls = Arc::new(AtomicUsize::new(0));
    let service = tower::service_fn({
        let api_calls = api_calls.clone();
        move |_request: Request<Body>| {
            api_calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(500)
                        .body(Body::from(b"{}".to_vec()))
                        .unwrap(),
                )
            }
        }
    });
    let client = Client::new(service, "sandbox-ns");
    let ledger =
        K8sWorkerCreateLedger::new(client.clone(), "control-ns", "sandbox-ns", "provider-a")
            .unwrap();
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let clock = Arc::new(ManualClock::new(100));
    let router = Arc::new(K8sCreateBroker::new(
        Arc::new(ledger),
        "sandbox-ns",
        "provider-a",
    ))
    .router(
        directory.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
    );
    NoApiRouteFixture {
        router,
        directory,
        clock,
        api_calls,
    }
}

pub(super) async fn ready_route_fixture() -> (NoApiRouteFixture, WorkerIdentity) {
    let fixture = no_api_route_fixture();
    let now_ms = fixture.clock.now_ms();
    let registered = fixture
        .directory
        .register(
            WorkerRegistration {
                worker_id: "k8s:sandbox-ns:worker:pod-uid".into(),
                incarnation_id: "pod-uid".into(),
                manifest: WorkerManifest::default(),
                create_ledger: None,
            },
            now_ms,
            60_000,
        )
        .await
        .unwrap();
    let identity = registered.snapshot.identity;
    assert_eq!(
        fixture
            .directory
            .heartbeat(&identity, empty_heartbeat(1, true), now_ms, 60_000,)
            .await
            .unwrap(),
        awaken_dispatch::RegistryMutation::Applied,
        "S1 real Ready transition"
    );
    (fixture, identity)
}

pub(super) fn seal_body(identity: &WorkerIdentity) -> Vec<u8> {
    let old = WorkerIdentity::new("k8s:sandbox-ns:old-worker:old-uid", "old-uid", 1);
    let predecessor =
        SandboxEffectFence::new("old-effect", &old.worker_id, old.lease_owner(), 1, u64::MAX)
            .unwrap();
    serde_json::to_vec(&json!({
        "wire_version": K8S_CREATE_WIRE_VERSION,
        "identity": identity,
        "scope": "scope",
        "predecessor": predecessor,
        "expected_pod_name": "awaken-scope"
    }))
    .unwrap()
}

pub(super) fn create_body(identity: &WorkerIdentity) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "wire_version": K8S_CREATE_WIRE_VERSION,
        "identity": identity,
        "create": {
            "path":"/api/v1/namespaces/sandbox-ns/pods",
            "object":{"metadata":{"name":"pod","namespace":"sandbox-ns",
                "annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}
        }
    }))
    .unwrap()
}

pub(super) fn route_request(
    path: &str,
    body: Vec<u8>,
    transport_owner: Option<&str>,
) -> Request<AxumBody> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(owner) = transport_owner {
        request = request.header(WORKER_ID_HEADER, owner);
    }
    request.body(AxumBody::from(body)).unwrap()
}

pub(super) async fn assert_unknown_seal(response: AxumResponse, rule: &str) {
    assert_eq!(response.status(), StatusCode::OK, "{rule} Seal control");
    assert_eq!(
        response.headers().get(K8S_CREATE_CAPTURE_HEADER).unwrap(),
        CreateReplyProfile::WholeJoin.header_value(),
        "{rule} original complete response profile"
    );
    let bytes = axum::body::to_bytes(
        response.into_body(),
        CreateReplyProfile::WholeJoin.body_limit().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<RequestJoin>(&bytes).unwrap(),
        RequestJoin::Unknown,
        "{rule} missing predecessor is not closed; admission creates no closure proof"
    );
}

// Optional response fault on this same original fake API; it creates no
// second client, ledger, request lifetime or classification path.
pub(super) struct ReadbackResponse {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
    pub(super) polls: Arc<AtomicUsize>,
}

// The existing response/body adapter has one implementation for GET and POST
// fault rows. This is test input, not a second effects/ledger authority.
impl ReadbackResponse {
    fn into_http(self, capture_error: bool) -> Response<AxumBody> {
        let Self {
            status,
            body,
            polls,
        } = self;
        // A ready data frame counts every first data poll, never a Pending shortcut.
        let body = Body::from(body).map_frame(move |frame| {
            polls.fetch_add(1, Ordering::SeqCst);
            frame
        });
        let body = if capture_error {
            // Real partial data followed by an HTTP-body error, using the
            // existing library combinator rather than another body/stream impl.
            AxumBody::new(
                body.with_trailers(std::future::ready(Some(Err(kube::Error::Service(
                    Box::new(std::io::Error::other("original body interrupted")),
                ))))),
            )
        } else {
            AxumBody::new(body)
        };
        Response::builder().status(status).body(body).unwrap()
    }
}

pub(super) struct ResponsePause {
    pub(super) method: Method,
    pub(super) path: String,
    pub(super) entered: Arc<Notify>,
    pub(super) release: Arc<Notify>,
}

pub(super) struct BrokerFixture {
    pub(super) broker: K8sCreateBroker,
    pub(super) api_client: Client,
    pub(super) ledger: Arc<K8sWorkerCreateLedger>,
    pub(super) api_calls: Arc<AtomicUsize>,
    pub(super) fail_send: Arc<Mutex<Option<Method>>>,
    pub(super) capture_error: Arc<Mutex<Option<Method>>>,
    pub(super) post_response: Arc<Mutex<Option<ReadbackResponse>>>,
    pub(super) response_pause: Arc<Mutex<Option<ResponsePause>>>,
    pub(super) directory: Arc<MemoryWorkerDirectory>,
    pub(super) clock: Arc<ManualClock>,
    pub(super) identity: WorkerIdentity,
    pub(super) create: K8sCreateRequest,
    pub(super) stored: Arc<Mutex<Option<Value>>>,
    pub(super) pod_posts: Arc<AtomicUsize>,
    pub(super) created_pod: Arc<Mutex<Option<Value>>>,
    pub(super) readback_response: Arc<Mutex<Option<ReadbackResponse>>>,
    pub(super) started: Arc<Notify>,
    pub(super) release: Arc<Notify>,
    pub(super) completed: Arc<Notify>,
}

// One authoritative existing fake API for the broker's fault/replay tests.
// Every original request/UID/CAS/projection assertion remains in this helper.
pub(super) async fn broker_fixture(mutate_response: fn(&mut Value)) -> BrokerFixture {
    let api_calls = Arc::new(AtomicUsize::new(0));
    let fail_send = Arc::new(Mutex::new(None::<Method>));
    let capture_error = Arc::new(Mutex::new(None::<Method>));
    let post_response = Arc::new(Mutex::new(None::<ReadbackResponse>));
    let response_pause = Arc::new(Mutex::new(None::<ResponsePause>));
    let stored = Arc::new(Mutex::new(None::<Value>));
    let pod_posts = Arc::new(AtomicUsize::new(0));
    let created_pod = Arc::new(Mutex::new(None::<Value>));
    let readback_response = Arc::new(Mutex::new(None::<ReadbackResponse>));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let service = tower::service_fn({
        let (api_calls, fail_send, capture_error, post_response, response_pause) = (
            api_calls.clone(),
            fail_send.clone(),
            capture_error.clone(),
            post_response.clone(),
            response_pause.clone(),
        );
        let (stored, created_pod, pod_posts, started, release, completed, readback_response) = (
            stored.clone(),
            created_pod.clone(),
            pod_posts.clone(),
            started.clone(),
            release.clone(),
            completed.clone(),
            readback_response.clone(),
        );
        move |request: Request<Body>| {
            let (api_calls, fail_send, capture_error, post_response, response_pause) = (
                api_calls.clone(),
                fail_send.clone(),
                capture_error.clone(),
                post_response.clone(),
                response_pause.clone(),
            );
            let (stored, created_pod, pod_posts, started, release, completed, readback_response) = (
                stored.clone(),
                created_pod.clone(),
                pod_posts.clone(),
                started.clone(),
                release.clone(),
                completed.clone(),
                readback_response.clone(),
            );
            async move {
                api_calls.fetch_add(1, Ordering::SeqCst);
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let raw = (method == Method::POST && path == "/api/v1/namespaces/sandbox-ns/pods")
                    || (method == Method::GET
                        && path == "/api/v1/namespaces/sandbox-ns/pods/sandbox-pod");
                let fail = {
                    let mut selected = fail_send.lock().unwrap();
                    if raw && selected.as_ref() == Some(&method) {
                        selected.take().is_some()
                    } else {
                        false
                    }
                };
                if fail {
                    if method == Method::POST {
                        pod_posts.fetch_add(1, Ordering::SeqCst);
                    }
                    return Err(std::io::Error::other("original API service unavailable"));
                }
                let fail_capture = {
                    let mut selected = capture_error.lock().unwrap();
                    if raw && selected.as_ref() == Some(&method) {
                        selected.take().is_some()
                    } else {
                        false
                    }
                };
                let bytes = request.into_body().collect_bytes().await.unwrap();
                let body: Value = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                let (code, value) = match (method.as_str(), path.as_str()) {
                    ("POST", "/api/v1/namespaces/control-ns/configmaps") => {
                        let mut created = body;
                        created["apiVersion"] = "v1".into();
                        created["kind"] = "ConfigMap".into();
                        created["metadata"]["uid"] = "ledger-uid".into();
                        created["metadata"]["resourceVersion"] = "1".into();
                        *stored.lock().unwrap() = Some(created.clone());
                        (201, created)
                    }
                    ("GET", path)
                        if path.starts_with("/api/v1/namespaces/control-ns/configmaps/") =>
                    {
                        (200, stored.lock().unwrap().clone().unwrap())
                    }
                    ("PUT", path)
                        if path.starts_with("/api/v1/namespaces/control-ns/configmaps/") =>
                    {
                        let mut current = stored.lock().unwrap();
                        let old = current.as_ref().unwrap();
                        if body["metadata"]["uid"] != old["metadata"]["uid"]
                            || body["metadata"]["resourceVersion"]
                                != old["metadata"]["resourceVersion"]
                        {
                            (
                                409,
                                json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":409,"reason":"Conflict"}),
                            )
                        } else {
                            let mut updated = body;
                            let next = old["metadata"]["resourceVersion"]
                                .as_str()
                                .unwrap()
                                .parse::<u64>()
                                .unwrap()
                                + 1;
                            updated["metadata"]["resourceVersion"] = next.to_string().into();
                            let finished = updated["data"]["ledger"]
                                .as_str()
                                .unwrap()
                                .contains("finished");
                            *current = Some(updated.clone());
                            if finished {
                                completed.notify_one();
                            }
                            (200, updated)
                        }
                    }
                    ("POST", "/api/v1/namespaces/sandbox-ns/pods") => {
                        {
                            let ledger = ledger_state(&stored);
                            let ticket = ledger["tickets"]
                                .as_object()
                                .unwrap()
                                .values()
                                .next()
                                .unwrap();
                            assert_eq!(
                                body["metadata"]["annotations"][ISOLATION_ID_ANNOTATION],
                                ticket["isolation_id"],
                                "E6 exact admission identity"
                            );
                            assert_eq!(
                                body["metadata"]["annotations"][REQUEST_DIGEST_ANNOTATION],
                                ticket["request"]["digest"],
                                "E6 exact request digest"
                            );
                            assert!(
                                body["metadata"]["finalizers"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .any(|value| value == ORIGINAL_GUARD_FINALIZER),
                                "E6 original Pod cannot disappear before its ticket result"
                            );
                        }
                        pod_posts.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        release.notified().await;
                        if let Some(response) = post_response.lock().unwrap().take() {
                            return Ok(response.into_http(fail_capture));
                        }
                        let mut object = body;
                        object["apiVersion"] = "v1".into();
                        object["kind"] = "Pod".into();
                        object["metadata"]["uid"] = "pod-uid".into();
                        mutate_response(&mut object);
                        *created_pod.lock().unwrap() = Some(object.clone());
                        (201, object)
                    }
                    ("GET", "/api/v1/namespaces/sandbox-ns/pods/sandbox-pod") => {
                        if let Some(response) = readback_response.lock().unwrap().take() {
                            return Ok(response.into_http(fail_capture));
                        }
                        (200, created_pod.lock().unwrap().clone().unwrap())
                    }
                    ("GET", "/api/v1/namespaces/sandbox-ns/pods/worker") => {
                        let state = ledger_state(&stored);
                        assert_eq!(
                            state["worker_sealed"], true,
                            "original Create admission closes before Worker evidence release"
                        );
                        (
                            404,
                            json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404}),
                        )
                    }
                    _ => panic!("unexpected test Kubernetes request {method} {path}"),
                };
                // Pause only the explicitly selected original response. All ConfigMap
                // guards above have dropped before awaiting real Registry mutations.
                let pause = {
                    let mut selected = response_pause.lock().unwrap();
                    if selected
                        .as_ref()
                        .is_some_and(|pause| pause.method == method && pause.path == path)
                    {
                        selected.take()
                    } else {
                        None
                    }
                };
                if let Some(pause) = pause {
                    pause.entered.notify_one();
                    pause.release.notified().await;
                }
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(code)
                        .body(AxumBody::from(serde_json::to_vec(&value).unwrap()))
                        .unwrap(),
                )
            }
        }
    });
    let client = Client::new(service, "sandbox-ns");
    let ledger =
        K8sWorkerCreateLedger::new(client.clone(), "control-ns", "sandbox-ns", "provider-a")
            .unwrap();
    let (ledger_name, ledger_uid) = ledger
        .initialize("sandbox-ns", "worker", "pod-uid")
        .await
        .unwrap();
    let anchor = WorkerCreateLedger::new(
        "provider-a",
        "control-ns",
        ledger_name,
        ledger_uid,
        ledger.configuration_fingerprint(),
    )
    .unwrap();
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let clock = Arc::new(ManualClock::new(100));
    let registered = directory
        .register(
            WorkerRegistration {
                worker_id: "k8s:sandbox-ns:worker:pod-uid".into(),
                incarnation_id: "pod-uid".into(),
                manifest: WorkerManifest::default(),
                create_ledger: Some(anchor),
            },
            100,
            60_000,
        )
        .await
        .unwrap();
    let identity = registered.snapshot.identity;
    directory
        .heartbeat(&identity, empty_heartbeat(1, true), 100, 60_000)
        .await
        .unwrap();
    let ledger = Arc::new(ledger);
    let broker = K8sCreateBroker::new(ledger.clone(), "sandbox-ns", "provider-a");
    let create = K8sCreateRequest {
        path: "/api/v1/namespaces/sandbox-ns/pods".into(),
        object: json!({"metadata":{
            "name":"sandbox-pod", "namespace":"sandbox-ns",
            "annotations":{"awaken.dev/realization-digest":"a".repeat(64)}
        }}),
    };
    BrokerFixture {
        api_calls,
        fail_send,
        capture_error,
        post_response,
        response_pause,
        api_client: client,
        ledger,
        broker,
        directory,
        clock,
        identity,
        create,
        stored,
        pod_posts,
        created_pod,
        readback_response,
        started,
        release,
        completed,
    }
}
