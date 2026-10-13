use super::*;

pub(super) struct BrokerFixture {
    pub(super) broker: K8sCreateBroker,
    pub(super) directory: Arc<MemoryWorkerDirectory>,
    pub(super) clock: Arc<ManualClock>,
    pub(super) identity: WorkerIdentity,
    pub(super) create: K8sCreateRequest,
    pub(super) stored: Arc<Mutex<Option<Value>>>,
    pub(super) pod_posts: Arc<AtomicUsize>,
    pub(super) created_pod: Arc<Mutex<Option<Value>>>,
    pub(super) started: Arc<Notify>,
    pub(super) release: Arc<Notify>,
    pub(super) completed: Arc<Notify>,
}

// One authoritative existing fake API for the broker's fault/replay tests.
// Every original request/UID/CAS/projection assertion remains in this helper.
pub(super) async fn broker_fixture(mutate_response: fn(&mut Value)) -> BrokerFixture {
    let stored = Arc::new(Mutex::new(None::<Value>));
    let pod_posts = Arc::new(AtomicUsize::new(0));
    let created_pod = Arc::new(Mutex::new(None::<Value>));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let service = tower::service_fn({
        let (stored, created_pod, pod_posts, started, release, completed) = (
            stored.clone(),
            created_pod.clone(),
            pod_posts.clone(),
            started.clone(),
            release.clone(),
            completed.clone(),
        );
        move |request: Request<Body>| {
            let (stored, created_pod, pod_posts, started, release, completed) = (
                stored.clone(),
                created_pod.clone(),
                pod_posts.clone(),
                started.clone(),
                release.clone(),
                completed.clone(),
            );
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
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
                            let stored = stored.lock().unwrap();
                            let ledger: Value = serde_json::from_str(
                                stored.as_ref().unwrap()["data"]["ledger"].as_str().unwrap(),
                            )
                            .unwrap();
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
                        let mut object = body;
                        object["apiVersion"] = "v1".into();
                        object["kind"] = "Pod".into();
                        object["metadata"]["uid"] = "pod-uid".into();
                        mutate_response(&mut object);
                        *created_pod.lock().unwrap() = Some(object.clone());
                        (201, object)
                    }
                    ("GET", "/api/v1/namespaces/sandbox-ns/pods/sandbox-pod") => {
                        (200, created_pod.lock().unwrap().clone().unwrap())
                    }
                    _ => panic!("unexpected test Kubernetes request {method} {path}"),
                };
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(code)
                        .body(Body::from(serde_json::to_vec(&value).unwrap()))
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
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
            100,
            60_000,
        )
        .await
        .unwrap();
    let broker = K8sCreateBroker::new(client, ledger, "sandbox-ns", "provider-a");
    let create = K8sCreateRequest {
        path: "/api/v1/namespaces/sandbox-ns/pods".into(),
        object: json!({"metadata":{
            "name":"sandbox-pod", "namespace":"sandbox-ns",
            "annotations":{"awaken.dev/realization-digest":"a".repeat(64)}
        }}),
    };
    BrokerFixture {
        broker,
        directory,
        clock,
        identity,
        create,
        stored,
        pod_posts,
        created_pod,
        started,
        release,
        completed,
    }
}
