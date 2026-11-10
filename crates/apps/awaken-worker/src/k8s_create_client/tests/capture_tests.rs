use super::*;
use awaken_container_contract::RuntimeError;
use awaken_container_contract::{CreateReplyProfile, K8S_CREATE_CAPTURE_HEADER, RequestJoin};
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

#[tokio::test(start_paused = true)]
async fn successful_control_response_requires_its_exact_endpoint_capture_marker() {
    // V4/R4: 2xx with missing/unknown/wrong-family marker rejects before
    // JSON parsing and never falls back to legacy. Non2xx still follows
    // the original auth/permanent/retry table in the unchanged tests.
    for (path, marker) in [
        (PATH, None),
        (PATH, Some("unknown")),
        (PATH, Some(CreateReplyProfile::WholeJoin.header_value())),
        (SEAL_PATH, None),
        (SEAL_PATH, Some("unknown")),
        (
            SEAL_PATH,
            Some(CreateReplyProfile::FreshCreate.header_value()),
        ),
        (
            SEAL_PATH,
            Some(CreateReplyProfile::LegacyRetainedCreate.header_value()),
        ),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route(
            path,
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
        let result = with_frozen_http_clock(async {
            if path == PATH {
                client.create(request()).await.map(drop)
            } else {
                let predecessor =
                    SandboxEffectFence::new("old", "worker", "boot", 1, u64::MAX).unwrap();
                client
                    .seal_predecessor("scope", &predecessor, "pod")
                    .await
                    .map(drop)
            }
        })
        .await;
        let error = result.expect_err("an endpoint must reject a missing or foreign profile");
        assert!(
            matches!(error, RuntimeError::MayHaveCommitted(ref reason) if reason == "Create control response lacks its exact current capture profile"),
            "{path} with {marker:?} must fail on the response marker, not on its deadline or decoder: {error:?}"
        );
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
        let (mut client, server) = fixture(router).await;
        // This case tests a >4 MiB byte policy, not the short retry deadline.
        // Both profiles get the same transfer/decode budget: disabling the
        // fresh limiter would make the fresh reply succeed like legacy.
        client.request_budget = std::time::Duration::from_secs(5);
        let result = client.create(request()).await;
        assert_eq!(
            result.is_ok(),
            profile == CreateReplyProfile::LegacyRetainedCreate,
            "profile {profile:?}: {:?}",
            result.as_ref().err()
        );
        if let Ok(response) = result {
            assert_eq!(response.object["metadata"]["uid"], "u".repeat(8193));
            assert_eq!(response.object["payload"].as_str().unwrap().len(), limit);
        }
        server.abort();
    }
    for length in [limit - 1, limit, limit + 1] {
        let mut wire = serde_json::to_vec(&RequestJoin::Complete(Vec::new())).unwrap();
        // Valid JSON whitespace keeps the payload decodable even at B+1.
        // Without the limiter the oversize arm would invent a complete join;
        // an invalid JSON body could fail for the wrong reason and hide that.
        wire.resize(length, b' ');
        assert_eq!(
            serde_json::from_slice::<RequestJoin>(&wire).unwrap(),
            RequestJoin::Complete(Vec::new())
        );
        let router = Router::new().route(
            SEAL_PATH,
            post(move || {
                let wire = wire.clone();
                async move {
                    let chunks = vec![
                        Ok::<_, std::io::Error>(wire[..limit / 2].to_vec()),
                        Ok(wire[limit / 2..].to_vec()),
                    ];
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(
                            K8S_CREATE_CAPTURE_HEADER,
                            CreateReplyProfile::WholeJoin.header_value(),
                        )
                        .header("content-type", "application/json")
                        .body(Body::from_stream(stream::iter(chunks)))
                        .unwrap()
                }
            }),
        );
        let (mut client, server) = fixture(router).await;
        client.request_budget = std::time::Duration::from_secs(5);
        let predecessor = SandboxEffectFence::new("old", "worker", "boot", 1, u64::MAX).unwrap();
        let result = client
            .seal_predecessor("scope", &predecessor, "awaken-scope")
            .await;
        if length <= limit {
            assert_eq!(result.unwrap(), RequestJoin::Complete(Vec::new()));
        } else {
            assert!(matches!(result, Err(RuntimeError::MayHaveCommitted(_))));
        }
        server.abort();
    }
}
