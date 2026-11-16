//! Wire identities for the existing exact reply, batch and cancellation owners.
use super::*;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt as _;

#[derive(Clone, Default)]
struct Calls {
    reads: Arc<Mutex<Vec<serde_json::Value>>>,
    mutations: Arc<AtomicUsize>,
    cancellations: Arc<Mutex<Vec<(String, String)>>>,
}

async fn read_reply(
    State(calls): State<Calls>,
    Path((session, run)): Path<(String, String)>,
    Json(body): Json<ProfiledSessionRunResume>,
) -> Json<ProfiledSessionRunReplyReceipt> {
    calls.reads.lock().unwrap().push(serde_json::json!({
        "session":session,"run":run,"body":body,
    }));
    Json(ProfiledSessionRunReplyReceipt { applied: true })
}

async fn read_batch(
    State(calls): State<Calls>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Json(body): Json<crate::types::session::SendEventsRequest>,
) -> Json<SessionEventBatchReceipt> {
    calls.reads.lock().unwrap().push(serde_json::json!({
        "session":session,"body":body,
        "key":headers.get("Idempotency-Key").unwrap().to_str().unwrap(),
        "subject":headers.get("anthropic-user-profile-id").unwrap().to_str().unwrap(),
    }));
    Json(SessionEventBatchReceipt { applied: true })
}

async fn mutate_reply(State(calls): State<Calls>) {
    calls.mutations.fetch_add(1, Ordering::SeqCst);
}

async fn cancel_run(State(calls): State<Calls>, Path(identity): Path<(String, String)>) {
    calls.cancellations.lock().unwrap().push(identity);
}

#[tokio::test]
async fn original_receipt_queries_retain_complete_bodies_and_select_only_read_handlers() {
    // Scalar and batch queries retain the complete original request. Neither
    // receipt route dispatches the existing mutation or cancellation handler.
    let calls = Calls::default();
    let app = profiled_session_run_reply_receipt_router(read_reply)
        .merge(session_event_batch_receipt_router(read_batch))
        .merge(profiled_session_run_resume_router(mutate_reply))
        .merge(profiled_session_run_cancel_router(cancel_run))
        .with_state(calls.clone());
    let reply = ProfiledSessionRunResume {
        operation_id: "original-operation".into(),
        expected_thread_version: 17,
        correlation_id: "original-correlation".into(),
        call_id: Some("original-call".into()),
        reply: SessionThreadToolReply::Result {
            content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "complete result",
            )],
            is_error: true,
        },
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/awaken/sessions/original-session/runs/original-run/resume/receipt")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert!(
        serde_json::from_slice::<ProfiledSessionRunReplyReceipt>(&bytes)
            .unwrap()
            .applied
    );
    let batch = crate::types::session::SendEventsRequest {
        events: vec![crate::types::session::InboundEvent::UserToolResult {
            tool_use_id: "original-call".into(),
            content: Some(vec![
                awaken_agent_contract::agent::content::ContentBlock::text("last member"),
            ]),
            is_error: false,
        }],
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/awaken/sessions/original-session/events/receipt")
                .header("content-type", "application/json")
                .header("Idempotency-Key", "original-key")
                .header("anthropic-user-profile-id", "original-subject")
                .body(Body::from(serde_json::to_vec(&batch).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *calls.reads.lock().unwrap(),
        vec![
            serde_json::json!({"session":"original-session","run":"original-run","body":reply}),
            serde_json::json!({"session":"original-session","body":batch,"key":"original-key","subject":"original-subject"}),
        ]
    );
    assert_eq!(calls.mutations.load(Ordering::SeqCst), 0);
    assert!(calls.cancellations.lock().unwrap().is_empty());
    for path in [
        "/v1/awaken/sessions/original-session/runs/original-run/resume/receipt",
        "/v1/awaken/sessions/original-session/events/receipt",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/awaken/sessions/original-session/runs/original-run/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *calls.cancellations.lock().unwrap(),
        vec![("original-session".into(), "original-run".into())]
    );
    assert_eq!(calls.mutations.load(Ordering::SeqCst), 0);
}
