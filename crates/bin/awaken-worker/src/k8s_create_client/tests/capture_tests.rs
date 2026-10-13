use super::*;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::stream;
use serde_json::json;

async fn fixture(router: Router) -> (WorkerK8sCreateClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut client = WorkerK8sCreateClient::new(WorkerUpstream::new(format!("http://{address}")));
    client.request_budget = std::time::Duration::from_millis(100);
    client
        .bind_identity(WorkerIdentity::new("awaken-worker", "boot", 1))
        .unwrap();
    (client, server)
}
fn request() -> K8sCreateRequest {
    K8sCreateRequest {
        path: "/api/v1/namespaces/ns/pods".into(),
        object: json!({"metadata":{"name":"pod"}}),
    }
}

#[tokio::test]
async fn current_request_rejects_the_old_strict_server_before_its_effect_handler() {
    // V2/V3: the old exact private DTO has only identity/create and rejects
    // unknown fields. New required wire_version fails extraction before the
    // handler counter (standing for old admission/effect). This is a test
    // of the historical decoder, never a second production serving path.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OldCreateWire {
        identity: WorkerIdentity,
        create: K8sCreateRequest,
    }
    let effects = Arc::new(AtomicUsize::new(0));
    let router = Router::new().route(
        PATH,
        post({
            let effects = effects.clone();
            move |Json(body): Json<OldCreateWire>| {
                let effects = effects.clone();
                async move {
                    assert_eq!(body.identity.worker_id, "awaken-worker");
                    assert_eq!(body.create.object["metadata"]["name"], "pod");
                    effects.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"status":201,"object":{}}))
                }
            }
        }),
    );
    let (client, server) = fixture(router).await;
    assert!(matches!(
        client.create(request()).await,
        Err(RuntimeError::MayHaveCommitted(_))
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn successful_control_response_requires_its_exact_endpoint_capture_marker() {
    // V4/R4: 2xx with missing/unknown/wrong-family marker rejects before
    // JSON parsing and never falls back to legacy. Non2xx still follows
    // the original auth/permanent/retry table in the unchanged tests.
    for marker in [None, Some("unknown"), Some("bounded-join-v1")] {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route(
            PATH,
            post({
                let calls = calls.clone();
                move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let mut response =
                            Json(json!({"status":201,"object":{"metadata":{"uid":"uid"}}}))
                                .into_response();
                        if let Some(marker) = marker {
                            response
                                .headers_mut()
                                .insert(K8S_CREATE_CAPTURE_HEADER, marker.parse().unwrap());
                        }
                        response
                    }
                }
            }),
        );
        let (client, server) = fixture(router).await;
        assert!(matches!(
            client.create(request()).await,
            Err(RuntimeError::MayHaveCommitted(_))
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "hard marker refusal is not a downgrade retry"
        );
        server.abort();
    }
}

#[tokio::test]
async fn chunked_fresh_reply_is_bounded_and_explicit_legacy_reply_stays_whole() {
    // R3/R5/R8/R9/R10: two actual network chunks without Content-Length
    // exceed complete fresh B -> MayHaveCommitted. The same complete JSON
    // under explicit legacy profile remains readable, including >R UID.
    // Whole join uses its own known marker and rejects oversized/truncated
    // bytes rather than inventing an empty RequestJoin.
    let limit = CreateReplyProfile::FreshCreate.body_limit().unwrap();
    let wire=serde_json::to_vec(&json!({"status":200,"object":{"metadata":{"uid":"u".repeat(8193)},"payload":"x".repeat(limit)}})).unwrap();
    for profile in [
        CreateReplyProfile::FreshCreate,
        CreateReplyProfile::LegacyRetainedCreate,
    ] {
        let wire = wire.clone();
        let router = Router::new().route(
            PATH,
            post(move || {
                let wire = wire.clone();
                async move {
                    let chunks = vec![
                        Ok::<_, std::io::Error>(wire[..limit / 2].to_vec()),
                        Ok(wire[limit / 2..].to_vec()),
                    ];
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(K8S_CREATE_CAPTURE_HEADER, profile.header_value())
                        .header("content-type", "application/json")
                        .body(Body::from_stream(stream::iter(chunks)))
                        .unwrap()
                }
            }),
        );
        let (client, server) = fixture(router).await;
        let result = client.create(request()).await;
        assert_eq!(
            result.is_ok(),
            profile == CreateReplyProfile::LegacyRetainedCreate
        );
        if let Ok(response) = result {
            assert_eq!(response.object["metadata"]["uid"], "u".repeat(8193));
            assert_eq!(response.object["payload"].as_str().unwrap().len(), limit);
        }
        server.abort();
    }
    let router = Router::new().route(
        SEAL_PATH,
        post(move || async move {
            (
                [(
                    K8S_CREATE_CAPTURE_HEADER,
                    CreateReplyProfile::WholeJoin.header_value(),
                )],
                "x".repeat(limit + 1),
            )
        }),
    );
    let (client, server) = fixture(router).await;
    let predecessor = ContainerEffectFence::new("old", "worker", "boot", 1, u64::MAX).unwrap();
    assert!(matches!(
        client
            .seal_predecessor("scope", &predecessor, "awaken-scope")
            .await,
        Err(RuntimeError::MayHaveCommitted(_))
    ));
    server.abort();
}
