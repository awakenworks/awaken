use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Observation;
use awaken_coordinator_client::HttpDispatchQueue;
use awaken_dispatch_contract::{
    ClaimedStreamPublisher, STREAM_PREVIEW_BATCH_PATH, StreamObservationRequest,
    StreamPreviewBatchRequest,
};
use awaken_worker_contract::{RunClaim, WorkerIdentity};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::mpsc;

async fn spawn(
    router: Router,
) -> (
    HttpDispatchQueue,
    WorkerIdentity,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let identity = WorkerIdentity::new("preview-worker", "preview-boot", 1);
    (
        HttpDispatchQueue::new(url, identity.clone()),
        identity,
        task,
    )
}

fn claim(identity: &WorkerIdentity, run: &str) -> RunClaim {
    RunClaim {
        run_id: RunId(run.into()),
        owner: identity.lease_owner(),
        epoch: 1,
    }
}

async fn offer(queue: &HttpDispatchQueue, claim: &RunClaim, text: &str) {
    queue
        .publish_observation(
            claim,
            Observation::assistant_delta(
                claim.run_id.clone(),
                ThreadId("preview-thread".into()),
                0,
                0,
                AgentEvent::Delta(Delta::TextDelta { delta: text.into() }),
            ),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn sparse_delta_is_sent_before_another_delta_is_available() {
    let (messages, mut received) = mpsc::unbounded_channel();
    let (queue, identity, server) = spawn(Router::new().route(
        STREAM_PREVIEW_BATCH_PATH,
        post(move |Json(request): Json<StreamPreviewBatchRequest>| {
            messages.send(request).unwrap();
            std::future::ready(Json(json!({"accepted": true})))
        }),
    ))
    .await;
    let claim = claim(&identity, "sparse");

    offer(&queue, &claim, "first").await;
    let first = tokio::time::timeout(Duration::from_millis(200), received.recv())
        .await
        .expect("a single delta must flush by itself")
        .unwrap();
    assert_eq!(first.claim, claim);
    assert_eq!(first.observations.len(), 1);
    offer(&queue, &claim, "second").await;
    let second = tokio::time::timeout(Duration::from_millis(200), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.observations.len(), 1);
    assert_eq!(
        second.observations[0].event.kind,
        AgentEvent::Delta(Delta::TextDelta {
            delta: "second".into()
        })
    );
    server.abort();
}

#[tokio::test]
async fn one_slow_run_does_not_delay_another_runs_preview() {
    let (messages, mut received) = mpsc::unbounded_channel();
    let (queue, identity, server) = spawn(Router::new().route(
        STREAM_PREVIEW_BATCH_PATH,
        post(move |Json(request): Json<StreamPreviewBatchRequest>| {
            let run = request.claim.run_id;
            messages.send(run.clone()).unwrap();
            async move {
                if run.0 == "slow" {
                    pending::<()>().await;
                }
                Json(json!({"accepted": true}))
            }
        }),
    ))
    .await;
    offer(&queue, &claim(&identity, "slow"), "held by server").await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap(),
        Some(RunId("slow".into()))
    );

    offer(&queue, &claim(&identity, "fast"), "independent delta").await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(150), received.recv())
            .await
            .expect("the other Run can publish while the first HTTP request is pending"),
        Some(RunId("fast".into()))
    );
    server.abort();
}

#[tokio::test]
async fn successive_batches_for_one_run_wait_for_the_previous_response() {
    let (messages, mut received) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let (queue, identity, server) = spawn(Router::new().route(
        STREAM_PREVIEW_BATCH_PATH,
        post(move |Json(request): Json<StreamPreviewBatchRequest>| {
            let gate = gate.clone();
            let kind = request.observations[0].event.kind.clone();
            messages.send(kind.clone()).unwrap();
            async move {
                if kind
                    == (AgentEvent::Delta(Delta::TextDelta {
                        delta: "first".into(),
                    }))
                {
                    gate.notified().await;
                }
                StatusCode::OK
            }
        }),
    ))
    .await;
    let claim = claim(&identity, "ordered");
    offer(&queue, &claim, "first").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .is_some()
    );

    offer(&queue, &claim, "second").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), received.recv())
            .await
            .is_err()
    );
    release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap(),
        Some(AgentEvent::Delta(Delta::TextDelta {
            delta: "second".into()
        }))
    );
    server.abort();
}

#[tokio::test]
async fn preview_transport_limits_concurrent_requests_across_runs() {
    let (messages, mut received) = mpsc::unbounded_channel();
    let permits = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = permits.clone();
    let (queue, identity, server) = spawn(Router::new().route(
        STREAM_PREVIEW_BATCH_PATH,
        post(move |Json(request): Json<StreamPreviewBatchRequest>| {
            let gate = gate.clone();
            messages.send(request.claim.run_id).unwrap();
            async move {
                gate.acquire().await.unwrap().forget();
                StatusCode::OK
            }
        }),
    ))
    .await;
    for run in ["one", "two", "three", "four", "five"] {
        offer(&queue, &claim(&identity, run), "delta").await;
    }
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_millis(150), received.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), received.recv())
            .await
            .is_err()
    );
    permits.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), received.recv())
            .await
            .unwrap()
            .is_some()
    );
    server.abort();
}

#[tokio::test]
async fn mixed_peer_fallback_is_decided_per_batch_and_preserves_delta_order() {
    let (singles, mut received) = mpsc::unbounded_channel();
    let (batches, mut attempted) = mpsc::unbounded_channel();
    // The legacy peer rejects the batch route. After an ambiguous response
    // elsewhere, subsequent deliveries must still try the batch protocol.
    let app = Router::new()
        .route(
            STREAM_PREVIEW_BATCH_PATH,
            post(move |Json(request): Json<StreamPreviewBatchRequest>| {
                let run = request.claim.run_id;
                let status = match run.0.as_str() {
                    "legacy" => StatusCode::NOT_FOUND,
                    "ambiguous" => StatusCode::BAD_GATEWAY,
                    _ => StatusCode::OK,
                };
                batches.send(run).unwrap();
                std::future::ready(status)
            }),
        )
        .route(
            "/v1/worker/dispatch/stream",
            post(move |Json(request): Json<StreamObservationRequest>| {
                singles.send(request).unwrap();
                std::future::ready(StatusCode::OK)
            }),
        );
    let (queue, identity, server) = spawn(app).await;
    let legacy = claim(&identity, "legacy");
    offer(&queue, &legacy, "first").await;
    offer(&queue, &legacy, "second").await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), attempted.recv())
            .await
            .unwrap(),
        Some(legacy.run_id.clone())
    );
    for delta in ["first", "second"] {
        let observation = tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observation.request.claim, legacy);
        assert_eq!(
            observation.request.event.kind,
            AgentEvent::Delta(Delta::TextDelta {
                delta: delta.into()
            })
        );
    }
    for run in ["ambiguous", "current"] {
        offer(&queue, &claim(&identity, run), run).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), attempted.recv())
                .await
                .unwrap(),
            Some(RunId(run.into()))
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(200), received.recv())
            .await
            .is_err()
    );
    server.abort();
}
