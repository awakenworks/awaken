use super::*;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;

fn pod() -> KubernetesPodIdentity {
    KubernetesPodIdentity::new("workers", "worker-1", "uid-a").unwrap()
}

fn accepted() -> Value {
    json!({"apiVersion":"authentication.k8s.io/v1", "kind":"TokenReview", "metadata":{}, "spec":{}, "status":{
        "authenticated":true, "audiences":["awaken-worker"],
        "user":{"username":"system:serviceaccount:workers:executor", "uid":"sa-uid", "extra":{
            "authentication.kubernetes.io/pod-name":["worker-1"],
            "authentication.kubernetes.io/pod-uid":["uid-a"]
        }}
    }})
}

struct ApiState {
    reply: Mutex<(StatusCode, Value)>,
    calls: AtomicUsize,
    tokens: Mutex<Vec<String>>,
    delay: Duration,
}

struct MockApi {
    state: Arc<ApiState>,
    auth: KubernetesWorkerAuthenticator,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl MockApi {
    async fn new(timeout: Duration, delay: Duration) -> Self {
        let state = Arc::new(ApiState {
            reply: Mutex::new((StatusCode::OK, accepted())),
            calls: AtomicUsize::new(0),
            tokens: Mutex::new(Vec::new()),
            delay,
        });
        async fn review(
            State(state): State<Arc<ApiState>>,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            state.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(body["spec"]["audiences"], json!(["awaken-worker"]));
            state
                .tokens
                .lock()
                .unwrap()
                .push(body["spec"]["token"].as_str().unwrap().into());
            tokio::time::sleep(state.delay).await;
            let (status, body) = state.reply.lock().unwrap().clone();
            (status, Json(body))
        }
        let router = axum::Router::new()
            .route(
                "/apis/authentication.k8s.io/v1/tokenreviews",
                axum::routing::post(review),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = Client::try_from(kube::Config::new(
            format!("http://{address}").parse().unwrap(),
        ))
        .unwrap();
        let auth = KubernetesWorkerAuthenticator::new(
            client,
            "workers",
            "executor",
            "awaken-worker",
            timeout,
        )
        .unwrap();
        Self {
            state,
            auth,
            server,
        }
    }

    fn set(&self, status: StatusCode, body: Value) {
        *self.state.reply.lock().unwrap() = (status, body);
    }
}

fn parts(identity: Option<&WorkerIdentity>) -> Parts {
    let path = if identity.is_some() {
        "/v1/worker/dispatch/attempt/begin"
    } else {
        REGISTER_PATH
    };
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header(WORKER_ID_HEADER, pod().worker_id())
        .header("authorization", "Bearer fixture-pod-token");
    if let Some(identity) = identity {
        request = request.header(IDENTITY_HEADER, serde_json::to_string(identity).unwrap());
    }
    request.body(()).unwrap().into_parts().0
}

#[test]
fn pod_coordinate_codec_is_exact_and_cannot_alias_a_replacement_or_path() {
    // P1 valid names/UID => bijective Worker-id coordinate. P2 changing any
    // coordinate changes identity. P3 empty, path/query injection, malformed
    // labels, extra separators and overlong input => reject before API lookup.
    let original = pod();
    assert_eq!(
        KubernetesPodIdentity::from_worker_id(&original.worker_id()).unwrap(),
        original
    );
    for (ns, name, uid) in [
        ("other", "worker-1", "uid-a"),
        ("workers", "worker-2", "uid-a"),
        ("workers", "worker-1", "uid-b"),
    ] {
        assert_ne!(
            KubernetesPodIdentity::new(ns, name, uid)
                .unwrap()
                .worker_id(),
            original.worker_id(),
            "P2"
        );
    }
    for value in [
        "",
        "k8s:workers:worker-1",
        "k8s:workers:worker-1:uid-a:extra",
        "k8s:workers:../worker:uid-a",
        "k8s:Workers:worker:uid",
        "k8s:workers:worker?:uid",
        "k8s:workers:worker:uid/a",
        "k8s:workers:worker:uid a",
        "k8s:workers:-worker:uid",
        "k8s:workers:worker.:uid",
    ] {
        assert!(
            KubernetesPodIdentity::from_worker_id(value).is_err(),
            "P3 {value}"
        );
    }
    assert!(KubernetesPodIdentity::new(&"a".repeat(64), "worker", "uid").is_err());
    assert!(KubernetesPodIdentity::new("ns", &"a".repeat(254), "uid").is_err());
    assert!(KubernetesPodIdentity::new("ns", "worker", &"a".repeat(129)).is_err());
    assert!(KubernetesPodIdentity::from_worker_id(&":".repeat(513)).is_err());
}

#[test]
fn projected_identity_and_remote_constructor_fail_closed() {
    // P4 all three bounded metadata files + token + HTTPS => exact Pod Worker.
    // P5 one missing/invalid/oversized metadata file => no construction. P6
    // invalid endpoint, token or CA => fail, never downgrade to local/HMAC.
    let directory = tempfile::tempdir().unwrap();
    let projected =
        || KubernetesWorkerRequestAuthorizer::from_projected_directory(directory.path());
    let fields = [
        ("namespace", "workers"),
        ("name", "worker-1"),
        ("uid", "uid-a"),
    ];
    for (name, value) in fields {
        std::fs::write(directory.path().join(name), format!("{value}\n")).unwrap();
    }
    let token = directory.path().join("token");
    std::fs::write(&token, "fixture-token").unwrap();
    let authorizer = projected().unwrap();
    assert_eq!(authorizer.pod(), &pod(), "P4");
    let upstream = authorizer
        .into_remote_upstream("https://coordinator.invalid", None)
        .unwrap();
    assert_eq!(upstream.worker_id(), pod().worker_id());
    for endpoint in [
        "http://coordinator",
        "ftp://coordinator",
        "not-a-url",
        "https://user:password@coordinator",
        "https://coordinator?token=x",
        "https://coordinator#fragment",
    ] {
        assert!(
            projected()
                .unwrap()
                .into_remote_upstream(endpoint, None)
                .is_err(),
            "P6 {endpoint}"
        );
    }
    assert!(
        projected()
            .unwrap()
            .into_remote_upstream("https://coordinator", Some(b"not a certificate"))
            .is_err(),
        "P6 CA"
    );
    std::fs::remove_file(&token).unwrap();
    assert!(
        projected()
            .unwrap()
            .into_remote_upstream("https://coordinator", None)
            .is_err(),
        "P6 token"
    );
    for (name, value) in fields {
        let path = directory.path().join(name);
        for invalid in [String::new(), "../invalid".into(), "a".repeat(1025)] {
            std::fs::write(&path, invalid).unwrap();
            assert!(projected().is_err(), "P5 {name}");
        }
        std::fs::remove_file(&path).unwrap();
        assert!(projected().is_err(), "P5 missing {name}");
        std::fs::write(&path, value).unwrap();
    }
}

#[tokio::test]
async fn reviewed_pod_requires_all_identity_audience_and_service_account_conjuncts() {
    // Cause/effect graph: API authenticated AND no error AND explicit expected
    // audience AND exact configured ServiceAccount AND unique Pod name/UID
    // AND caller coordinate equality => authenticated bootstrap/current identity.
    // R1 all => exact context; R2 each missing conjunct => denial. Each mutation
    // starts from R1 (MC/DC). Kubernetes API honesty is TCB, not mocked proof.
    let api = MockApi::new(Duration::from_secs(2), Duration::ZERO).await;
    let identity = WorkerIdentity::new(pod().worker_id(), "boot-a", 7);
    for registered in [false, true] {
        let request = parts(registered.then_some(&identity));
        api.set(StatusCode::OK, accepted());
        let verified = api.auth.authenticate(&request).await.unwrap();
        assert_eq!(verified.worker_id(), pod().worker_id());
        assert_eq!(verified.identity(), registered.then_some(&identity));
        assert!(verified.credential_id().is_some());
        let cases = [
            ("/status/authenticated", json!(false)),
            ("/status/authenticated", Value::Null),
            ("/status/audiences", json!([])),
            ("/status/audiences", json!(["kubernetes-api"])),
            ("/status/audiences", Value::Null),
            (
                "/status/user/username",
                json!("system:serviceaccount:other:executor"),
            ),
            (
                "/status/user/username",
                json!("system:serviceaccount:workers:other"),
            ),
            ("/status/user/uid", json!("")),
            ("/status/user", Value::Null),
            ("/status/user/extra", Value::Null),
            (
                "/status/user/extra/authentication.kubernetes.io~1pod-name",
                json!(["other"]),
            ),
            (
                "/status/user/extra/authentication.kubernetes.io~1pod-name",
                json!([]),
            ),
            (
                "/status/user/extra/authentication.kubernetes.io~1pod-uid",
                json!(["uid-b"]),
            ),
            (
                "/status/user/extra/authentication.kubernetes.io~1pod-uid",
                json!(["uid-a", "uid-a"]),
            ),
            (
                "/status/user/extra/authentication.kubernetes.io~1pod-uid",
                json!([""]),
            ),
            (
                "/status/user/extra/authentication.kubernetes.io~1pod-uid",
                Value::Null,
            ),
            ("/status", Value::Null),
        ];
        for (pointer, value) in cases {
            let mut response = accepted();
            *response.pointer_mut(pointer).unwrap() = value;
            api.set(StatusCode::OK, response);
            assert!(
                api.auth.authenticate(&request).await.is_err(),
                "R2 {pointer}"
            );
        }
        let mut error = accepted();
        error["status"]["error"] = json!("fixture-token-must-never-appear-in-error");
        api.set(StatusCode::OK, error);
        let denied = api.auth.authenticate(&request).await.unwrap_err();
        assert!(
            !denied.to_string().contains("fixture-token"),
            "R2 secret-safe error"
        );
    }
}

#[tokio::test]
async fn malformed_or_unbound_requests_never_spend_a_token_review() {
    // R3 malformed/duplicate headers, mismatched incarnation, missing binding
    // outside POST register => local denial and zero API calls. A token cannot
    // turn a bootstrap identity into a claimed execution or foreign Worker.
    let api = MockApi::new(Duration::from_secs(2), Duration::ZERO).await;
    let identity = WorkerIdentity::new(pod().worker_id(), "boot-a", 7);
    for header in [WORKER_ID_HEADER, "authorization", IDENTITY_HEADER] {
        let mut request = parts(Some(&identity));
        let value = request.headers[header].clone();
        request.headers.append(header, value);
        assert!(
            api.auth.authenticate(&request).await.is_err(),
            "R3 duplicate {header}"
        );
    }
    for bad in [
        WorkerIdentity::new("foreign", "boot", 7),
        WorkerIdentity::new(pod().worker_id(), " ", 7),
        WorkerIdentity::new(pod().worker_id(), "boot", 0),
    ] {
        assert!(
            api.auth.authenticate(&parts(Some(&bad))).await.is_err(),
            "R3 identity"
        );
    }
    for header in [WORKER_ID_HEADER, "authorization"] {
        let mut request = parts(None);
        request.headers.remove(header);
        assert!(
            api.auth.authenticate(&request).await.is_err(),
            "R3 missing {header}"
        );
    }
    for (header, value) in [
        (IDENTITY_HEADER, "not-json".into()),
        (IDENTITY_HEADER, "a".repeat(4097)),
        (WORKER_ID_HEADER, "k8s:other:worker-1:uid-a".into()),
        ("authorization", "Bearer ".into()),
        ("authorization", "Bearer token token".into()),
        (
            "authorization",
            format!("Bearer {}", "a".repeat(MAX_TOKEN_BYTES + 1)),
        ),
    ] {
        let mut request = parts(None);
        request.headers.insert(header, value.parse().unwrap());
        assert!(
            api.auth.authenticate(&request).await.is_err(),
            "R3 malformed {header}"
        );
    }
    let mut request = parts(None);
    request.method = axum::http::Method::GET;
    assert!(api.auth.authenticate(&request).await.is_err());
    request.method = axum::http::Method::POST;
    request.uri = "/v1/worker/dispatch/attempt/begin".parse().unwrap();
    assert!(api.auth.authenticate(&request).await.is_err());
    let mut request = parts(None);
    request
        .headers
        .insert("authorization", "AwakenWorker fixture".parse().unwrap());
    assert!(
        api.auth.authenticate(&request).await.is_err(),
        "no signed fallback"
    );
    assert_eq!(api.state.calls.load(Ordering::SeqCst), 0, "R3 no review");
}

#[tokio::test]
async fn provider_failure_timeout_and_revocation_never_reuse_prior_authority() {
    // R4 successful review then deletion/revocation => subsequent request fails
    // (no success cache). R5 unavailable, forbidden or invalid JSON => bounded
    // failure with no response-body disclosure. R6 hung API => own timeout.
    let api = MockApi::new(Duration::from_secs(2), Duration::ZERO).await;
    let request = parts(None);
    assert!(api.auth.authenticate(&request).await.is_ok());
    let mut revoked = accepted();
    revoked["status"]["authenticated"] = json!(false);
    api.set(StatusCode::OK, revoked);
    assert!(api.auth.authenticate(&request).await.is_err(), "R4");
    for status in [
        StatusCode::FORBIDDEN,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::OK,
    ] {
        api.set(status, json!({"error":"fixture-secret"}));
        let error = api
            .auth
            .authenticate(&request)
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-secret"), "R5 {status}");
    }
    let api = MockApi::new(Duration::from_millis(20), Duration::from_secs(5)).await;
    let error = tokio::time::timeout(Duration::from_secs(2), api.auth.authenticate(&request))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "R6");
}

#[tokio::test]
async fn projected_token_rotation_and_registration_use_the_existing_authorizer_port() {
    // R7 projected file rotates => next request uses new token, same Pod and
    // allocated incarnation. R8 missing/empty/oversized/malformed token => fail
    // locally, never reuse the old token. R9 foreign binding/bootstrap execution
    // => fail before network. Token contents are never Debug/error diagnostics.
    let api = MockApi::new(Duration::from_secs(2), Duration::ZERO).await;
    let directory = tempfile::tempdir().unwrap();
    let token = directory.path().join("token");
    let authorizer = KubernetesWorkerRequestAuthorizer::new(pod(), token.clone()).unwrap();
    let identity = WorkerIdentity::new(pod().worker_id(), "boot", 7);
    let client = reqwest::Client::new();
    let request = |authorizer: &dyn WorkerRequestAuthorizer, path: &str| {
        let outgoing = authorizer
            .authorize(
                "POST",
                path,
                &pod().worker_id(),
                client.post(format!("https://coordinator{path}")),
            )?
            .build()
            .map_err(|e| e.to_string())?;
        let mut incoming = Request::builder()
            .method("POST")
            .uri(path)
            .body(())
            .unwrap();
        *incoming.headers_mut() = outgoing.headers().clone();
        Ok::<_, String>(incoming.into_parts().0)
    };
    std::fs::write(&token, "fixture-token-1\n").unwrap();
    let bootstrap = request(&authorizer, REGISTER_PATH).unwrap();
    assert!(
        api.auth.authenticate(&bootstrap).await.is_ok(),
        "R7 bootstrap"
    );
    let bound = authorizer.bind_worker_identity(&identity);
    std::fs::write(&token, "fixture-token-2\n").unwrap();
    let path = "/v1/worker/dispatch/attempt/begin";
    let verified = api
        .auth
        .authenticate(&request(bound.as_ref(), path).unwrap())
        .await
        .unwrap();
    assert_eq!(verified.identity(), Some(&identity), "R7 registered");
    assert_eq!(
        *api.state.tokens.lock().unwrap(),
        ["fixture-token-1", "fixture-token-2"]
    );
    for invalid in [
        "".into(),
        "fixture\ntoken".into(),
        "a".repeat(MAX_TOKEN_BYTES + 1),
    ] {
        std::fs::write(&token, invalid).unwrap();
        assert!(request(bound.as_ref(), path).is_err(), "R8 invalid file");
    }
    std::fs::remove_file(&token).unwrap();
    assert!(request(bound.as_ref(), path).is_err(), "R8 removed file");
    assert!(
        request(&authorizer, path).is_err(),
        "R9 bootstrap execution"
    );
    let foreign = authorizer.bind_worker_identity(&WorkerIdentity::new("foreign", "boot", 1));
    assert!(
        request(foreign.as_ref(), path).is_err(),
        "R9 foreign binding"
    );
    assert_eq!(api.state.calls.load(Ordering::SeqCst), 2, "R8/R9 no review");
}
