//! The live-inbox resource over HTTP: list/queue/replace/reorder/withdraw on
//! the session's in-flight queue, and the error mapping (404 unknown message,
//! 409 stale order, 410 inactive queue, 404 unknown session).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{
    LiveInboxEntry, LiveInboxError, LiveInboxSnapshot, ManagedState, OutcomeReport, RunError,
    SessionRuntime, TurnOutcome, router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn create(app: &Router) -> String {
    let (status, s) = call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "coder" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    s["id"].as_str().unwrap().to_string()
}

fn text_content(text: &str) -> serde_json::Value {
    serde_json::json!({ "content": [{ "type": "text", "text": text }] })
}

/// A fake runtime whose live inbox is a plain in-memory queue, so these tests
/// exercise the wire surface (routing, DTOs, status mapping) — the real queue
/// semantics are covered by the runtime-contract and engine tests.
#[derive(Default)]
struct QueueFake {
    active: AtomicBool,
    queue: Mutex<FakeQueue>,
}

#[derive(Default)]
struct FakeQueue {
    next_id: u64,
    version: u64,
    entries: Vec<(u64, Vec<ContentBlock>)>,
}

#[async_trait::async_trait]
impl SessionRuntime for QueueFake {
    async fn run_turn(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        Ok(TurnOutcome {
            messages: vec![Message::text(Id("a".into()), Role::Assistant, "ok")],
            stop: StopReason::EndTurn,
            pending: None,
            compacted: false,
        })
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_protocol_managed::Decision,
    ) -> Result<TurnOutcome, RunError> {
        unimplemented!("not exercised")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: &str,
        _is_error: bool,
    ) -> Result<TurnOutcome, RunError> {
        unimplemented!("not exercised")
    }

    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        unimplemented!("not exercised")
    }

    fn model(&self) -> String {
        "fake".to_string()
    }

    async fn live_inbox_snapshot(&self, _thread: &str) -> LiveInboxSnapshot {
        if !self.active.load(Ordering::SeqCst) {
            return LiveInboxSnapshot::inactive();
        }
        let queue = self.queue.lock().unwrap();
        LiveInboxSnapshot {
            active: true,
            version: queue.version,
            messages: queue
                .entries
                .iter()
                .map(|(id, content)| LiveInboxEntry {
                    id: *id,
                    content: content.clone(),
                })
                .collect(),
        }
    }

    async fn live_inbox_queue(
        &self,
        _thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        queue.next_id += 1;
        queue.version += 1;
        let id = queue.next_id;
        queue.entries.push((id, content));
        Ok(id)
    }

    async fn live_inbox_remove(&self, _thread: &str, id: u64) -> Result<(), LiveInboxError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        let index = queue
            .entries
            .iter()
            .position(|(entry_id, _)| *entry_id == id)
            .ok_or(LiveInboxError::UnknownMessage)?;
        queue.entries.remove(index);
        queue.version += 1;
        Ok(())
    }

    async fn live_inbox_replace(
        &self,
        _thread: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        let entry = queue
            .entries
            .iter_mut()
            .find(|(entry_id, _)| *entry_id == id)
            .ok_or(LiveInboxError::UnknownMessage)?;
        entry.1 = content;
        queue.version += 1;
        Ok(())
    }

    async fn live_inbox_reorder(
        &self,
        _thread: &str,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        if order.len() != queue.entries.len() {
            return Err(LiveInboxError::StaleOrder);
        }
        let mut reordered = Vec::with_capacity(order.len());
        for id in &order {
            let index = queue
                .entries
                .iter()
                .position(|(entry_id, _)| entry_id == id)
                .ok_or(LiveInboxError::StaleOrder)?;
            reordered.push(queue.entries.remove(index));
        }
        queue.entries = reordered;
        queue.version += 1;
        Ok(())
    }
}

fn app(fake: Arc<QueueFake>) -> Router {
    struct Shared(Arc<QueueFake>);
    #[async_trait::async_trait]
    impl SessionRuntime for Shared {
        async fn run_turn(
            &self,
            a: &str,
            t: &str,
            c: Vec<ContentBlock>,
        ) -> Result<TurnOutcome, RunError> {
            self.0.run_turn(a, t, c).await
        }
        async fn resume(
            &self,
            t: &str,
            i: &str,
            d: awaken_protocol_managed::Decision,
        ) -> Result<TurnOutcome, RunError> {
            self.0.resume(t, i, d).await
        }
        async fn resume_custom(
            &self,
            t: &str,
            i: &str,
            c: &str,
            e: bool,
        ) -> Result<TurnOutcome, RunError> {
            self.0.resume_custom(t, i, c, e).await
        }
        async fn add_system(&self, t: &str, x: &str) -> Result<(), RunError> {
            self.0.add_system(t, x).await
        }
        async fn define_outcome(
            &self,
            t: &str,
            d: &str,
            r: &str,
            m: u32,
        ) -> Result<OutcomeReport, RunError> {
            self.0.define_outcome(t, d, r, m).await
        }
        fn model(&self) -> String {
            self.0.model()
        }
        async fn live_inbox_snapshot(&self, t: &str) -> LiveInboxSnapshot {
            self.0.live_inbox_snapshot(t).await
        }
        async fn live_inbox_queue(
            &self,
            t: &str,
            c: Vec<ContentBlock>,
        ) -> Result<u64, LiveInboxError> {
            self.0.live_inbox_queue(t, c).await
        }
        async fn live_inbox_remove(&self, t: &str, id: u64) -> Result<(), LiveInboxError> {
            self.0.live_inbox_remove(t, id).await
        }
        async fn live_inbox_replace(
            &self,
            t: &str,
            id: u64,
            c: Vec<ContentBlock>,
        ) -> Result<(), LiveInboxError> {
            self.0.live_inbox_replace(t, id, c).await
        }
        async fn live_inbox_reorder(&self, t: &str, o: Vec<u64>) -> Result<(), LiveInboxError> {
            self.0.live_inbox_reorder(t, o).await
        }
    }
    router(Arc::new(ManagedState::new(Shared(fake))))
}

#[tokio::test]
async fn unknown_session_is_404_before_any_queue_logic() {
    let app = app(Arc::new(QueueFake::default()));
    let (status, body) = call(
        &app,
        "GET",
        "/v1/sessions/sess_missing/live-inbox",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn inactive_queue_lists_empty_and_refuses_mutations_with_410() {
    let fake = Arc::new(QueueFake::default());
    let app = app(fake);
    let session = create(&app).await;

    let (status, body) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{session}/live-inbox"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);
    assert_eq!(body["messages"].as_array().unwrap().len(), 0);

    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session}/live-inbox"),
        text_content("late"),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{session}/live-inbox/1"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn queue_edit_reorder_withdraw_round_trip() {
    let fake = Arc::new(QueueFake::default());
    fake.active.store(true, Ordering::SeqCst);
    let app = app(fake);
    let session = create(&app).await;
    let base = format!("/v1/sessions/{session}/live-inbox");

    // Queue two messages.
    let (status, first) = call(&app, "POST", &base, text_content("one")).await;
    assert_eq!(status, StatusCode::OK);
    let first = first["id"].as_u64().unwrap();
    let (_, second) = call(&app, "POST", &base, text_content("two")).await;
    let second = second["id"].as_u64().unwrap();

    // Snapshot shows both, in order.
    let (_, snap) = call(&app, "GET", &base, serde_json::Value::Null).await;
    assert_eq!(snap["active"], true);
    let listed: Vec<u64> = snap["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    assert_eq!(listed, [first, second]);

    // Replace keeps id and position.
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/{first}"),
        text_content("one-edited"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, snap) = call(&app, "GET", &base, serde_json::Value::Null).await;
    assert_eq!(
        snap["messages"][0]["content"][0]["text"], "one-edited",
        "edit landed in place"
    );

    // Reorder: full permutation applies; a stale one is 409.
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/order"),
        serde_json::json!({ "order": [second, first] }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/order"),
        serde_json::json!({ "order": [second] }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Withdraw one; withdrawing it again is 404.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("{base}/{first}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("{base}/{first}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, snap) = call(&app, "GET", &base, serde_json::Value::Null).await;
    let remaining: Vec<u64> = snap["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    assert_eq!(
        remaining,
        [second],
        "reorder then withdraw left `two` alone"
    );
}
