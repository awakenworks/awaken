//! Cross-adapter startup proof for the Awaken live-inbox resource: list,
//! queue, replace, reorder, and withdraw over the Managed Session application.
//! the session's in-flight queue, and the error mapping (404 unknown message,
//! 409 stale order, 410 inactive queue, 404 unknown session).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::EndCause;
use awaken_protocol_awaken::live_inbox_router;
use awaken_protocol_managed::{ManagedState, router};
use awaken_session_contract::{
    LiveInboxEntry, LiveInboxError, LiveInboxSnapshot, OutcomeDrive, RunError, SessionRuntime,
    StepOutcome,
};
use awaken_tenancy::WorkspaceScope;
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
    call_in_scope(app, method, uri, body, Some("default")).await
}

async fn call_in_scope(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
    workspace: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    if let Some(workspace) = workspace {
        req.extensions_mut()
            .insert(WorkspaceScope(workspace.to_string()));
    }
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

async fn create(app: &Router) -> String {
    let (status, s) = call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({
            "agent": "coder",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
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
    user_runs: Mutex<HashMap<String, awaken_session_contract::SessionUserRunCommand>>,
}

#[derive(Default)]
struct FakeQueue {
    next_id: u64,
    version: u64,
    entries: Vec<(u64, Vec<ContentBlock>)>,
}

fn unsupported_runtime_operation() -> RunError {
    RunError::internal("operation is outside the live-inbox test scope")
}

#[async_trait::async_trait]
impl SessionRuntime for QueueFake {
    async fn reserve_session_user_run(
        &self,
        command: awaken_session_contract::SessionUserRunCommand,
    ) -> Result<awaken_session_contract::SessionUserRunReservation, RunError> {
        let mut runs = self.user_runs.lock().unwrap();
        if let Some(existing) = runs.get(&command.run_id.0) {
            if existing != &command {
                return Err(RunError::bad_request(
                    "live-inbox test reservation replay changed its command",
                ));
            }
            return Ok(awaken_session_contract::SessionUserRunReservation::Completed);
        }
        runs.insert(command.run_id.0.clone(), command);
        Ok(awaken_session_contract::SessionUserRunReservation::Completed)
    }

    async fn session_user_run_state(
        &self,
        _session_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        Ok(self
            .user_runs
            .lock()
            .unwrap()
            .contains_key(&run_id.0)
            .then(|| awaken_agent_contract::agent::run::RunState::Ended(EndCause::NaturalEnd)))
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        // Fake boundary rule F2/R1: this adapter fake owns no committed Thread
        // store, so the only consistent recovery answer is no committed Run.
        // Returning `None` exercises the durable event ingress without teaching
        // the test a forbidden reconstruction from independently read fields.
        Ok(None)
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Ok(StepOutcome::ended(
            vec![Message::text(Id("a".into()), Role::Assistant, "ok")],
            EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(unsupported_runtime_operation())
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(unsupported_runtime_operation())
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(unsupported_runtime_operation())
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
        async fn reserve_session_user_run(
            &self,
            command: awaken_session_contract::SessionUserRunCommand,
        ) -> Result<awaken_session_contract::SessionUserRunReservation, RunError> {
            self.0.reserve_session_user_run(command).await
        }

        async fn session_user_run_state(
            &self,
            session_id: &str,
            run_id: &awaken_agent_contract::agent::run::Id,
        ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
            self.0.session_user_run_state(session_id, run_id).await
        }

        async fn session_thread_recovery_snapshot(
            &self,
            session_id: &str,
            thread_id: &str,
        ) -> Result<
            Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
            RunError,
        > {
            self.0
                .session_thread_recovery_snapshot(session_id, thread_id)
                .await
        }

        async fn run(
            &self,
            a: &str,
            t: &str,
            c: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            self.0.run(a, t, c).await
        }
        async fn resume(
            &self,
            t: &str,
            i: &str,
            d: awaken_session_contract::ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            self.0.resume(t, i, d).await
        }
        async fn resume_custom(
            &self,
            t: &str,
            i: &str,
            c: Vec<ContentBlock>,
            e: bool,
        ) -> Result<StepOutcome, RunError> {
            self.0.resume_custom(t, i, c, e).await
        }
        async fn define_outcome(
            &self,
            t: &str,
            d: &str,
            r: &str,
            m: u32,
        ) -> Result<OutcomeDrive, RunError> {
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
    let state = Arc::new(ManagedState::new(Shared(fake)));
    router(state.clone()).merge(live_inbox_router(state.session_application()))
}

#[tokio::test]
async fn workspace_scope_fences_every_live_inbox_operation() {
    // FMECA cause/effect graph:
    // C1 Session exists; C2 request scope is missing, foreign, or owning; C3
    // operation is read or mutation. Effects: E1 missing/foreign always returns
    // indistinguishable 404; E2 owning reads/mutations reach the queue; E3 a
    // rejected request causes no queue side effect.
    //
    // | Rule | Session | scope | verb | effect |
    // |---|---|---|---|---|
    // | S1 | exists | missing | GET | 404, no disclosure |
    // | S2 | exists | foreign | POST | 404, no mutation |
    // | S3 | exists | owner | POST | accepted by active queue |
    let fake = Arc::new(QueueFake::default());
    fake.active.store(true, Ordering::SeqCst);
    let app = app(fake);
    let session = create(&app).await;
    let base = format!("/v1/awaken/sessions/{session}/live-inbox");

    let (status, _) = call_in_scope(&app, "GET", &base, serde_json::Value::Null, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "S1");

    let (status, _) = call_in_scope(
        &app,
        "POST",
        &base,
        text_content("foreign"),
        Some("workspace-b"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "S2");

    let (status, queued) = call(&app, "POST", &base, text_content("owner")).await;
    assert_eq!(status, StatusCode::OK, "S3");
    assert_eq!(queued["id"], 1, "S2 must not consume a queue id");
}

#[tokio::test]
async fn malformed_live_inbox_commands_are_rejected_before_application_mutation() {
    // FMECA parser-boundary graph: C1 request has the required content/order;
    // C2 it contains an unknown field (version skew or injection); C3 the queue
    // is active. Effects: E1 exact command mutates once; E2 malformed command is
    // a 4xx and never reaches the application. Rule P1 C1+C3 -> E1; P2 C2 -> E2.
    let fake = Arc::new(QueueFake::default());
    fake.active.store(true, Ordering::SeqCst);
    let app = app(fake);
    let session = create(&app).await;
    let base = format!("/v1/awaken/sessions/{session}/live-inbox");

    let (status, _) = call(
        &app,
        "POST",
        &base,
        serde_json::json!({
            "content": [{"type": "text", "text": "must-not-land"}],
            "unexpected": true
        }),
    )
    .await;
    assert!(status.is_client_error(), "P2: {status}");

    let (status, snapshot) = call(&app, "GET", &base, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["messages"].as_array().unwrap().len(), 0, "P2/E2");
}

#[tokio::test]
async fn unknown_session_is_404_before_any_queue_logic() {
    // Decision rule R1: C1 the neutral application reports an unknown Session
    // -> E1 the Awaken adapter returns 404 and never consults queue state.
    let app = app(Arc::new(QueueFake::default()));
    let (status, body) = call(
        &app,
        "GET",
        "/v1/awaken/sessions/sess_missing/live-inbox",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn inactive_queue_lists_empty_and_refuses_mutations_with_410() {
    // Decision rules R2/R3: C1 known Session + inactive queue + read -> E1 an
    // empty inactive snapshot; the same state + mutation -> E2 410 with no edit.
    let fake = Arc::new(QueueFake::default());
    let app = app(fake);
    let session = create(&app).await;

    let (status, body) = call(
        &app,
        "GET",
        &format!("/v1/awaken/sessions/{session}/live-inbox"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);
    assert_eq!(body["messages"].as_array().unwrap().len(), 0);

    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/awaken/sessions/{session}/live-inbox"),
        text_content("late"),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/v1/awaken/sessions/{session}/live-inbox/1"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn inactive_live_steer_has_the_existing_durable_event_fallback() {
    // Causes: the fixtures below establish `inactive live steer has the existing durable event
    // fallback` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // End-to-end FMECA rule: C1 no locally reachable active attempt; C2 caller
    // first tries best-effort steer; C3 caller retries the content through the
    // ordinary Session event ingress. Effects: E1 live steer returns 410 without
    // consuming content; E2 the existing event command accepts and processes the
    // message exactly once; E3 committed event history, not LiveInbox, owns truth.
    //
    // | Rule | live active | selected ingress | effect |
    // |---|---|---|---|
    // | F1 | no | LiveInbox | 410, no queue entry |
    // | F2 | no | Session events | 200, one committed user.message |
    let app = app(Arc::new(QueueFake::default()));
    let session = create(&app).await;
    let live = format!("/v1/awaken/sessions/{session}/live-inbox");

    let (status, _) = call(&app, "POST", &live, text_content("reliable")).await;
    assert_eq!(status, StatusCode::GONE, "F1");

    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({
            "events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "reliable"}]
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "F2: {body}");

    let (status, events) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let committed = events["data"]
        .as_array()
        .expect("event page")
        .iter()
        .filter(|event| event["type"] == "user.message")
        .count();
    assert_eq!(committed, 1, "F2/E3");
}

#[tokio::test]
async fn queue_edit_reorder_withdraw_round_trip() {
    // Cause/effect graph: active queue -> append stable ids -> replace preserves
    // identity/position -> full reorder commits -> stale reorder is 409 -> first
    // remove is terminal success and repeated remove is 404. These rules cover
    // all five methods through one neutral LiveInboxApplication owner.
    let fake = Arc::new(QueueFake::default());
    fake.active.store(true, Ordering::SeqCst);
    let app = app(fake);
    let session = create(&app).await;
    let base = format!("/v1/awaken/sessions/{session}/live-inbox");

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
