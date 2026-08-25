//! Adapter integration tests with fake runtimes: the happy path, and the HITL
//! await -> `requires_action` -> `user.tool_confirmation` -> resume round-trip.

mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{
    RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind, RunLifecyclePage,
    encode_run_lifecycle_cursor,
};
use awaken_protocol_managed::{ManagedState, managed_session_id_from_idempotency, router};
use awaken_session_contract::{
    AgentCapabilities, BuiltinTool, CommittedOutcomeProjection, CustomTool,
    ManagedSessionRepository, OutcomeDrive, OutcomeIteration, OutcomeReport, Pending, RunError,
    SessionExecutionState, SessionRuntime, SessionUserRunCommand, SessionUserRunReservation,
    StepOutcome, ToolPermissionDecision,
};
use awaken_session_store::SqliteManagedSessionRepository;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let (status, json) = Box::pin(json_response(app, method, uri, body)).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {json}");
    json
}

async fn json_response(
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
    let resp = Box::pin(app.clone().oneshot(req)).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn types(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

/// Add the SDK-required Environment to an exact wire fixture. Call sites remain
/// explicit about constructing a Session request; the test harness never grants
/// the production-forbidden implicit local fallback.
fn session_request(mut body: serde_json::Value) -> serde_json::Value {
    body.as_object_mut()
        .expect("Session request fixture is an object")
        .insert(
            "environment_id".into(),
            serde_json::json!(awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID),
        );
    body
}

async fn create(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

async fn post_session_with_idempotency(
    app: &Router,
    key: Option<&str>,
    title: &str,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(
                    serde_json::to_vec(&session_request(serde_json::json!({
                        "agent": "coder",
                        "title": title,
                    })))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// Session-create idempotency cause/effect decision table.
///
/// | Rule | Key | Payload | Effect |
/// |---|---|---|---|
/// | I1 | absent | same | ordinary independent Sessions |
/// | I2 | valid, repeated | same | one Session id and one durable aggregate |
/// | I3 | valid, repeated | changed | 409 idempotency mismatch |
/// | I4 | empty/overlong | any | 400 before Session creation |
/// | I5 | same key, different owner | same | distinct owner-scoped Sessions |
/// | I6 | valid, repeated | same non-empty initial Events | one Session and one durable batch |
/// | I7 | valid key and exact owner | same | exported prediction equals the server-selected Session id |
#[tokio::test]
async fn session_create_idempotency_replays_one_canonical_session() {
    // Causes: the fixtures below establish `session create idempotency replays one canonical
    // session` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let first = post_session_with_idempotency(&app, Some("design-project-a"), "Project A").await;
    let replay = post_session_with_idempotency(&app, Some("design-project-a"), "Project A").await;
    assert_eq!(first.0, StatusCode::OK, "I2 first create succeeds");
    assert_eq!(replay.0, StatusCode::OK, "I2 replay succeeds");
    assert_eq!(first.1["id"], replay.1["id"], "I2 identity is stable");

    let mismatch = post_session_with_idempotency(&app, Some("design-project-a"), "Changed").await;
    assert_eq!(
        mismatch.0,
        StatusCode::CONFLICT,
        "I3 changed payload conflicts"
    );

    let independent_a = post_session_with_idempotency(&app, None, "ordinary").await;
    let independent_b = post_session_with_idempotency(&app, None, "ordinary").await;
    assert_ne!(
        independent_a.1["id"], independent_b.1["id"],
        "I1 preserves ordinary create"
    );

    let invalid = post_session_with_idempotency(&app, Some(""), "invalid").await;
    assert_eq!(
        invalid.0,
        StatusCode::BAD_REQUEST,
        "I4 rejects an empty key"
    );
    let overlong = "x".repeat(256);
    let invalid = post_session_with_idempotency(&app, Some(&overlong), "invalid").await;
    assert_eq!(
        invalid.0,
        StatusCode::BAD_REQUEST,
        "I4 rejects an overlong key"
    );
    async fn post_initial(app: &Router) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sessions")
                    .header("content-type", "application/json")
                    .header("idempotency-key", "seeded-session")
                    .body(Body::from(
                        serde_json::to_vec(&session_request(serde_json::json!({
                            "agent": "coder",
                            "initial_events": [{
                                "type": "user.message",
                                "content": [{"type": "text", "text": "once"}]
                            }]
                        })))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    let initial = post_initial(&app).await;
    let initial_replay = post_initial(&app).await;
    assert_eq!(initial.0, StatusCode::OK, "I6 first create: {}", initial.1);
    assert_eq!(initial_replay.0, StatusCode::OK, "I6 replay");
    assert_eq!(initial.1["id"], initial_replay.1["id"], "I6 identity");
    assert_eq!(initial.1["status"], "running", "I6 durable batch active");

    let sessions = json_call(&app, "GET", "/v1/sessions", serde_json::Value::Null).await;
    assert_eq!(
        sessions["data"].as_array().unwrap().len(),
        4,
        "I2/I3/I4/I6 add no duplicate"
    );

    let state = Arc::new(ManagedState::new(EchoFake::default()));
    let request = || {
        serde_json::from_value(session_request(serde_json::json!({ "agent": "coder" }))).unwrap()
    };
    let owner_a = state
        .create_session_idempotent(request(), Some("owner-a".into()), "shared-key")
        .await
        .unwrap();
    let owner_b = state
        .create_session_idempotent(request(), Some("owner-b".into()), "shared-key")
        .await
        .unwrap();
    assert_eq!(
        owner_a.id,
        managed_session_id_from_idempotency("owner-a", "shared-key"),
        "I7 the public predictor and create path share one formula"
    );
    assert_ne!(owner_a.id, owner_b.id, "I5 keys are owner-scoped");
}

#[tokio::test]
async fn failed_idempotent_create_is_409_while_exact_get_remains_404() {
    // HTTP cause/effect decision table. C1 the deterministic create receipt is
    // durable; C2 owner and request fingerprint match; C3 execution is
    // ActivationFailed; C4 the process cache is cold. Effects: E1 replay is 409
    // `invalid_request_error` with the stable machine-readable message; E2 exact
    // GET remains 404 `not_found_error`; E3 neither request creates, rehydrates,
    // retries, replaces, or mutates the failed Session.
    //
    // | Rule | C1 | C2 | C3 | C4 | POST replay | exact GET | Side effect |
    // |---|---|---|---|---|---|---|---|
    // | F1 | yes | yes | yes | yes | E1 | E2 | E3 |
    //
    // Live and mismatch partitions are owned by the complete idempotency table
    // above. The same router and repository paths are used; this test adds no
    // recovery or failure-only implementation.
    let repo = Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open Session repository"),
    );
    let original = Arc::new(ManagedState::new(EchoFake::default()).with_session_repo(repo.clone()));
    let app = router(original.clone());
    let created = post_session_with_idempotency(&app, Some("failed-create"), "Failed create").await;
    assert_eq!(
        created.0,
        StatusCode::OK,
        "F1 fixture create: {}",
        created.1
    );
    let id = created.1["id"].as_str().unwrap().to_string();
    let mut failed = repo.get(&id).await.expect("durable Session");
    failed.execution = SessionExecutionState::ActivationFailed;
    let failed = support::replace_session_fixture(
        repo.as_ref(),
        "default",
        failed,
        "test:activation-failed",
    )
    .await;
    drop(app);
    drop(original);

    let restarted =
        Arc::new(ManagedState::new(EchoFake::default()).with_session_repo(repo.clone()));
    let app = router(restarted.clone());
    let replay = post_session_with_idempotency(&app, Some("failed-create"), "Failed create").await;
    assert_eq!(replay.0, StatusCode::CONFLICT, "F1/E1: {}", replay.1);
    assert_eq!(replay.1["type"], "error", "F1/E1");
    assert_eq!(replay.1["error"]["type"], "invalid_request_error", "F1/E1");
    assert_eq!(
        replay.1["error"]["message"], "session_create_terminal_conflict",
        "F1/E1 stable machine-readable message"
    );
    assert!(restarted.list_sessions().is_empty(), "F1/E3 no rehydration");

    let (status, body) = raw_call(&app, "GET", &format!("/v1/sessions/{id}"), Body::empty()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "F1/E2: {body}");
    assert_eq!(body["error"]["type"], "not_found_error", "F1/E2");
    assert_eq!(repo.get(&id).await.unwrap(), failed, "F1/E3 durable truth");
}

fn ended(messages: Vec<Message>) -> StepOutcome {
    StepOutcome::ended(messages, EndCause::NaturalEnd)
}

/// The happy path: one assistant text reply, no tools.
#[derive(Clone, Default)]
struct EchoFake {
    state: Arc<Mutex<EchoState>>,
}

static ECHO_MESSAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[derive(Default)]
struct EchoState {
    runs: HashMap<String, RunState>,
    messages: HashMap<String, Vec<Message>>,
    message_commit_cursors: HashMap<String, Vec<u64>>,
    lifecycle: HashMap<String, Vec<RunLifecycleEvent>>,
}

impl EchoFake {
    fn recovery_snapshot(
        &self,
        thread_id: &str,
    ) -> Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot> {
        // This fake uses the same mutex for lifecycle, transcript, and Run state
        // so its snapshot models the production port's one-prefix guarantee.
        let state = self.state.lock().unwrap();
        let lifecycle = state.lifecycle.get(thread_id)?;
        let latest = lifecycle.last()?;
        let thread_id = ThreadId(thread_id.to_string());
        let mut runs = Vec::<awaken_agent_contract::agent::run::Record>::new();
        for event in lifecycle {
            if let Some(run) = runs.iter_mut().find(|run| run.id == event.run_id) {
                run.state = event.state.clone();
            } else {
                runs.push(awaken_agent_contract::agent::run::Record {
                    id: event.run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: event.state.clone(),
                });
            }
        }
        let messages = state
            .messages
            .get(&latest.thread_id.0)
            .cloned()
            .unwrap_or_default();
        let message_commit_cursors = state
            .message_commit_cursors
            .get(&latest.thread_id.0)
            .cloned()
            .unwrap_or_default();
        let next_commit_ordinal = u64::try_from(messages.len()).unwrap();
        Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                thread_id,
                claimed_run_id: latest.run_id.clone(),
                runs,
                latest_run_id: Some(latest.run_id.clone()),
                messages,
                message_commit_cursors,
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: u64::try_from(lifecycle.len()).unwrap(),
                store_cursor: latest.source_commit_cursor,
                next_commit_ordinal,
            },
        )
    }
}

#[async_trait::async_trait]
impl SessionRuntime for EchoFake {
    async fn reserve_session_user_run(
        &self,
        command: SessionUserRunCommand,
    ) -> Result<SessionUserRunReservation, RunError> {
        let mut state = self.state.lock().unwrap();
        if state.runs.contains_key(&command.run_id.0) {
            return Ok(SessionUserRunReservation::Completed);
        }
        state.runs.insert(
            command.run_id.0.clone(),
            RunState::Ended(EndCause::NaturalEnd),
        );
        let opening_commit_cursor = u64::try_from(
            state
                .lifecycle
                .get(&command.session_id)
                .map_or(1, |lifecycle| lifecycle.len() + 1),
        )
        .unwrap();
        let terminal_commit_cursor = opening_commit_cursor + 1;

        let user_text = Message::new(
            Id::session_event_input(&command.session_id, &command.operation_id),
            Role::User,
            command.content.clone(),
        )
        .text_content();
        let mut new_message_commit_cursors = Vec::new();
        {
            let transcript = state
                .messages
                .entry(command.session_id.clone())
                .or_default();
            if let Some(system) = command.accompanying_system {
                let message = Message::new(
                    Id::session_system(&command.session_id, &system.operation_id),
                    Role::System,
                    system.content,
                );
                if !transcript.iter().any(|existing| existing.id == message.id) {
                    transcript.push(message);
                    new_message_commit_cursors.push(opening_commit_cursor);
                }
            }
            transcript.push(Message::new(
                Id::session_event_input(&command.session_id, &command.operation_id),
                Role::User,
                command.content,
            ));
            new_message_commit_cursors.push(opening_commit_cursor);
            transcript.push(Message::text(
                Id(format!("{}/reply", command.run_id.0)),
                Role::Assistant,
                format!("echo: {user_text}"),
            ));
            new_message_commit_cursors.push(terminal_commit_cursor);
        }
        state
            .message_commit_cursors
            .entry(command.session_id.clone())
            .or_default()
            .extend(new_message_commit_cursors);

        // The adapter's one warm/cold projector is intentionally driven by the
        // committed lifecycle feed, never by `session_user_run_state` or the
        // disposable transcript cache. Keep the fake's three query surfaces
        // causally consistent with the production Thread commit boundary.
        let lifecycle = state
            .lifecycle
            .entry(command.session_id.clone())
            .or_default();
        lifecycle.push(RunLifecycleEvent {
            cursor: encode_run_lifecycle_cursor(opening_commit_cursor, 0).unwrap(),
            source_commit_cursor: opening_commit_cursor,
            thread_id: ThreadId(command.session_id.clone()),
            run_id: RunId(command.run_id.0.clone()),
            kind: RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        });
        lifecycle.push(RunLifecycleEvent {
            cursor: encode_run_lifecycle_cursor(terminal_commit_cursor, 0).unwrap(),
            source_commit_cursor: terminal_commit_cursor,
            thread_id: ThreadId(command.session_id),
            run_id: RunId(command.run_id.0),
            kind: RunLifecycleEventKind::Completed,
            state: RunState::Ended(EndCause::NaturalEnd),
            await_reason: None,
        });
        Ok(SessionUserRunReservation::Completed)
    }

    async fn session_user_run_state(
        &self,
        _session_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<RunState>, RunError> {
        Ok(self.state.lock().unwrap().runs.get(&run_id.0).cloned())
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .messages
            .get(thread)
            .cloned()
            .unwrap_or_default())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if session_id != thread_id {
            return Ok(None);
        }
        Ok(self.recovery_snapshot(thread_id))
    }

    async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        let events = self
            .state
            .lock()
            .unwrap()
            .lifecycle
            .get(thread)
            .into_iter()
            .flatten()
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        Ok(RunLifecyclePage {
            next_cursor: events.last().map_or(cursor, |event| event.cursor),
            events,
        })
    }

    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &command,
            Vec::new(),
        ))
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        let user_text = Message::new(Id("u".into()), Role::User, content).text_content();
        Ok(ended(vec![Message::text(
            // Runtime message identity is the active-active projection fence; a
            // test double must obey the production contract that every committed
            // message has a distinct id, including across Runs.
            Id(format!(
                "a-{}",
                ECHO_MESSAGE_SEQUENCE.fetch_add(1, Ordering::SeqCst)
            )),
            Role::Assistant,
            format!("echo: {user_text}"),
        )]))
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no awaiting run"))
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("no outcome"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn an_unknown_inbound_event_type_is_rejected() {
    // Causes: the fixtures below establish `an unknown inbound event type` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `is rejected` and every asserted state transition or side
    // effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `an unknown inbound event type` is one independent branch selecting `is
    // rejected`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    // A well-formed body carrying an unknown event `type` fails the tagged-enum
    // decode → 400, not a silently-ignored event.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let id = create(&app).await;
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sessions/{id}/events"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "events": [{ "type": "user.bogus" }] }))
                .unwrap(),
        ))
        .unwrap();
    let status = app.clone().oneshot(req).await.unwrap().status();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The session honors the official `agent` model axis without constructing an
/// unpublished route from a model string. Cause/effect: a plain reference
/// inherits the published model; an equal-id override may change public
/// inference controls; a different id or unavailable version is rejected by the
/// state-layer authority test. Rules: inherit -> published id/version 1;
/// equal id + fast -> same route/fast/inherited version.
#[tokio::test]
async fn session_agent_model_override_is_honored_and_echoed() {
    // Causes: the fixtures below establish `session agent model override` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    // Plain reference → the host default model (`EchoFake::model`), version 1.
    let base = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;
    assert_eq!(base["agent"]["model"]["id"], "test-model");
    assert_eq!(base["agent"]["version"], 1);
    // The official object form changes controls while retaining the published route.
    let over = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({
            "agent": {
                "id": "coder",
                "type": "agent_with_overrides",
                "model": { "id": "test-model", "speed": "fast" }
            }
        })),
    )
    .await;
    assert_eq!(over["agent"]["model"]["id"], "test-model");
    assert_eq!(over["agent"]["model"]["speed"], "fast");
    assert_eq!(over["agent"]["version"], 1);
}

/// `agent_with_overrides` with `model: null` clears the model — rejected, since a
/// session always needs one (400, mirroring the API's `agent_model_required`).
#[tokio::test]
async fn clearing_the_model_on_a_session_override_is_rejected() {
    // Causes: the fixtures below establish `clearing the model on a session override` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `is rejected` and every asserted state transition or side
    // effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&session_request(serde_json::json!({
                "agent": { "id": "coder", "type": "agent_with_overrides", "model": null }
            })))
            .unwrap(),
        ))
        .unwrap();
    let status = app.clone().oneshot(req).await.unwrap().status();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Session-create `initial_events` is derived from the documentation's complete
/// cause graph for this field:
///
/// omitted/empty ───────────────────────────────────────────────> create idle
/// 1..=50 message/outcome + <=1 outcome + valid rubric/limits ──> create running,
///                                                                persist in order
/// unsupported type OR invalid member OR >50 OR >1 outcome ─────> reject atomically
///
/// | Rule | Count | Members | Outcomes | Effect |
/// |---|---:|---|---:|---|
/// | C1 | 0 | - | 0 | 200 idle; no execution |
/// | C2 | 1..=50 | message/outcome valid | 0..=1 | 200 running; shared executor; one Running edge even if a warm refresh precedes terminal |
/// | C3 | valid | unsupported | any | 400; no Session |
/// | C4 | valid | one invalid in mixed batch | any | 400; no partial Session/event |
/// | C5 | 51 | otherwise valid | 0 | 400 |
/// | C6 | valid | outcome missing rubric or two outcomes | >1/invalid | 400 |
#[tokio::test]
async fn session_initial_events_follow_the_atomic_decision_table() {
    // This decision-table fixture intentionally retains the maximum-size JSON
    // boundary cases. Heap-own its generated future so the default Tokio test
    // thread stack is independent of those wire-fixture sizes.
    Box::pin(session_initial_events_decision_table_case()).await;
}

async fn session_initial_events_decision_table_case() {
    // Causes: the fixtures below establish `session initial events follow the atomic decision
    // table` with the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let idle_app = router(Arc::new(ManagedState::new(EchoFake::default())));
    for (rule, body) in [
        (
            "C1 omitted",
            session_request(serde_json::json!({"agent": "coder"})),
        ),
        (
            "C1 empty",
            session_request(serde_json::json!({"agent": "coder", "initial_events": []})),
        ),
    ] {
        let (status, session) = json_response(&idle_app, "POST", "/v1/sessions", body).await;
        assert_eq!(status, StatusCode::OK, "{rule}");
        assert_eq!(session["status"], "idle", "{rule}");
    }

    let running_state = Arc::new(ManagedState::new(EchoFake::default()));
    let running_app = router(running_state.clone());
    let (status, session) = json_response(
        &running_app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({
            "agent": "coder",
            "initial_events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "start"}]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C2 message admitted");
    assert_eq!(session["status"], "running", "C2 starts immediately");
    let id = session["id"].as_str().unwrap();
    // This adapter table owns atomic create/Event effects, not supervisor
    // scheduling. Reuse the canonical application test driver once so no
    // parallel test-only lifecycle loop competes with the retained batch.
    support::drive_retained_session_events(&running_state, id).await;
    let completed = json_call(
        &running_app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&completed),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ],
        "C2 preserves inbound-before-output ordering"
    );
    assert!(completed["data"][0]["processed_at"].is_string());

    let outcome_app = router(Arc::new(ManagedState::new(OutcomeFake::default())));
    let (status, session) = json_response(
        &outcome_app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({
            "agent": "coder",
            "initial_events": [{
                "type": "user.define_outcome",
                "description": "finish",
                "rubric": {"type": "text", "content": "done"},
                "max_iterations": 20
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C2 single outcome admitted");
    assert_eq!(
        session["status"], "running",
        "C2 outcome starts immediately"
    );

    let rejected_app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let message = serde_json::json!({
        "type": "user.message",
        "content": [{"type": "text", "text": "must not run"}]
    });
    let outcome = serde_json::json!({
        "type": "user.define_outcome",
        "description": "finish",
        "rubric": {"type": "text", "content": "done"}
    });
    let invalid_cases = [
        (
            "C3 unsupported",
            session_request(serde_json::json!({"agent":"coder", "initial_events":[{
                "type":"system.message", "content":[{"type":"text", "text":"x"}]
            }]})),
        ),
        (
            "C4 mixed atomic",
            session_request(serde_json::json!({"agent":"coder", "initial_events":[
                message.clone(), {"type":"user.interrupt"}
            ]})),
        ),
        (
            "C5 over maximum",
            session_request(serde_json::json!({
                "agent":"coder",
                "initial_events": vec![message.clone(); 51]
            })),
        ),
        (
            "C6 two outcomes",
            session_request(serde_json::json!({
                "agent":"coder",
                "initial_events":[outcome.clone(), outcome]
            })),
        ),
        (
            "C6 missing rubric",
            session_request(serde_json::json!({"agent":"coder", "initial_events":[{
                "type":"user.define_outcome", "description":"finish"
            }]})),
        ),
    ];
    for (rule, body) in invalid_cases {
        let (status, _) = json_response(&rejected_app, "POST", "/v1/sessions", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
    }
    let sessions = json_call(
        &rejected_app,
        "GET",
        "/v1/sessions",
        serde_json::Value::Null,
    )
    .await;
    assert!(sessions["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn happy_path_projects_message_and_idle() {
    // Causes: the fixtures below establish `happy path` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let state = Arc::new(ManagedState::new(EchoFake::default()));
    let app = router(state.clone());
    let id = create(&app).await;
    // Send-response cause/effect rules: C1 one valid User Event is atomically
    // retained; C2 the opportunistic reconciler may or may not finish before the
    // HTTP response. Effects: W1 returns the complete inbound DTO and stable id;
    // W2 permits `processed_at` to be null only while queued, while the eventual
    // history contains the same id marked processed; W3 the committed root Run
    // emits Running/Idle using the public primary Thread id. Decision rules:
    // R1=C1+not-C2 -> W1+nullable W2; R2=C1+C2 -> W1+timestamp W2; both -> W3.
    let sent = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    // R1/R2 deliberately stop at durable acceptance. Advance the same
    // application driver used by the lifecycle supervisor before asserting W3.
    support::drive_retained_session_events(&state, &id).await;
    assert_eq!(sent["data"][0]["type"], "user.message", "R1/R2 W1");
    assert_eq!(
        sent["data"][0]["content"],
        serde_json::json!([{ "type": "text", "text": "hi" }]),
        "R1/R2 W1"
    );
    assert!(sent["data"][0]["id"].is_string(), "R1/R2 W1");
    assert!(
        sent["data"][0]["processed_at"].is_null() || sent["data"][0]["processed_at"].is_string(),
        "R1/R2 W2"
    );
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ]
    );
    assert_eq!(list["data"][0]["id"], sent["data"][0]["id"], "R1/R2 W2");
    assert!(list["data"][0]["processed_at"].is_string(), "R1/R2 W2");
    let threads = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    let primary_id = threads["data"][0]["id"]
        .as_str()
        .expect("W2 public primary Thread");
    assert!(primary_id.starts_with("sthr_"), "W2");
    assert_ne!(primary_id, id, "W2");
    assert!(
        list["data"].as_array().unwrap().iter().all(|event| {
            !event["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("session.thread_status_"))
                || event["session_thread_id"] == primary_id
        }),
        "W2"
    );
}

/// A runtime that reports a provisioned surface, exercised by session creation.
struct CapableFake;

#[async_trait::async_trait]
impl SessionRuntime for CapableFake {
    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(None)
    }

    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            builtin_tools: vec![
                BuiltinTool {
                    name: "read".into(),
                    ask: false,
                },
                BuiltinTool {
                    name: "write".into(),
                    ask: true,
                },
            ],
            custom_tools: vec![CustomTool {
                name: "submit".into(),
                description: "Submit the answer".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            skills: vec!["deploy".into()],
            delegates: vec!["researcher".into()],
        }
    }
}

/// A created session advertises the runtime's surface on its agent object: the
/// built-in toolset, a custom tool, skills, and a multiagent roster — not an empty set.
#[tokio::test]
async fn create_session_advertises_capabilities() {
    let app = router(Arc::new(ManagedState::new(CapableFake)));
    let s = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;

    let tools = s["agent"]["tools"].as_array().unwrap();
    assert_eq!(tools[0]["type"], "agent_toolset_20260401");
    assert_eq!(tools[1]["type"], "custom");
    assert_eq!(tools[1]["name"], "submit");
    assert!(s["agent"]["mcp_servers"].as_array().unwrap().is_empty());
    assert_eq!(s["agent"]["skills"][0]["skill_id"], "deploy");
    assert_eq!(s["agent"]["multiagent"]["type"], "coordinator");
    assert!(s["resources"].as_array().unwrap().is_empty());
}

/// The default capability surface is empty: a runtime that does not override
/// `capabilities` advertises no tools, skills, or resources, and omits `multiagent`.
#[tokio::test]
async fn create_session_defaults_to_empty_surface() {
    // Causes: the fixtures below establish `create session defaults to empty surface` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `create session defaults to empty surface` is one independent branch
    // selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let s = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;
    assert!(s["agent"]["tools"].as_array().unwrap().is_empty());
    assert!(s["agent"]["skills"].as_array().unwrap().is_empty());
    assert!(s["agent"]["multiagent"].is_null());
    assert!(s["resources"].as_array().unwrap().is_empty());
}

/// Golden wire contract for the created session's agent object: the exact Managed
/// Agents shapes the SDK parses — one `agent_toolset_20260401` reference (with the
/// required per-tool 0.120 output discriminant, unregistered tools disabled, and
/// the confirmation-gated ones `always_ask`), a
/// `custom` tool, a `custom` skill reference, a `coordinator` multiagent roster, and
/// empty `mcp_servers` / `resources`. A field rename or extra key breaks this.
#[tokio::test]
async fn session_capability_objects_match_wire_contract() {
    // Causes: the fixtures below establish `session capability objects match wire contract` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `session capability objects match wire contract` is one independent
    // branch selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let app = router(Arc::new(ManagedState::new(CapableFake)));
    let s = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;

    assert_eq!(
        s["agent"]["tools"],
        serde_json::json!([
            {
                "type": "agent_toolset_20260401",
                "configs": [
                    { "name": "bash", "type": "bash", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "write", "type": "write", "enabled": true, "permission_policy": { "type": "always_ask" } },
                    { "name": "edit", "type": "edit", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "glob", "type": "glob", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "grep", "type": "grep", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "web_fetch", "type": "web_fetch", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "web_search", "type": "web_search", "enabled": false, "permission_policy": { "type": "always_allow" } }
                ],
                "default_config": { "enabled": true, "permission_policy": { "type": "always_allow" } }
            },
            {
                "type": "custom",
                "name": "submit",
                "description": "Submit the answer",
                "input_schema": { "type": "object" }
            }
        ])
    );
    assert_eq!(s["agent"]["mcp_servers"], serde_json::json!([]));
    assert_eq!(
        s["agent"]["skills"],
        serde_json::json!([{ "type": "custom", "skill_id": "deploy", "version": "latest" }])
    );
    assert_eq!(s["agent"]["multiagent"]["type"], "coordinator");
    let child = &s["agent"]["multiagent"]["agents"][0];
    assert_eq!(child["id"], "researcher");
    assert_eq!(child["name"], "researcher");
    assert_eq!(child["type"], "agent");
    assert_eq!(child["version"], 1);
    assert_eq!(s["resources"], serde_json::json!([]));
}

/// One canonical adapter Runtime fixture for both official reply families.
///
/// It models only the durable boundaries the Managed adapter reads: one reserved
/// User Run, one committed Awaiting snapshot/ticket, and one exact staged reply
/// that commits the same Run terminal. Application/Host tests own dispatch and
/// crash recovery; this fixture must never revive the removed direct run/resume
/// projection path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdapterToolScenario {
    Permission,
    Custom,
}

struct AdapterToolRun {
    session_id: String,
    run_id: RunId,
    state: RunState,
    messages: Vec<Message>,
    message_commit_cursors: Vec<u64>,
    lifecycle: Vec<RunLifecycleEvent>,
    pending: Option<Pending>,
}

struct ToolAwaitingFake {
    scenario: AdapterToolScenario,
    run: Mutex<Option<AdapterToolRun>>,
}

impl ToolAwaitingFake {
    fn permission() -> Self {
        Self {
            scenario: AdapterToolScenario::Permission,
            run: Mutex::new(None),
        }
    }

    fn custom() -> Self {
        Self {
            scenario: AdapterToolScenario::Custom,
            run: Mutex::new(None),
        }
    }

    fn pending(&self) -> Pending {
        match self.scenario {
            AdapterToolScenario::Permission => Pending {
                tool_use_id: "call-1".into(),
                name: "write".into(),
                input: serde_json::json!({ "path": "x.txt", "content": "hi" }),
                client_executed: false,
            },
            AdapterToolScenario::Custom => Pending {
                tool_use_id: "cc1".into(),
                name: "submit_answer".into(),
                input: serde_json::json!({ "question": "6x7" }),
                client_executed: true,
            },
        }
    }

    fn await_reason(&self) -> awaken_agent_contract::agent::awaiting::AwaitReason {
        match self.scenario {
            AdapterToolScenario::Permission => {
                awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission
            }
            AdapterToolScenario::Custom => {
                awaken_agent_contract::agent::awaiting::AwaitReason::ExternalEvent
            }
        }
    }

    fn recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot> {
        if session_id != thread_id {
            return None;
        }
        let run = self.run.lock().unwrap();
        let run = run.as_ref()?;
        let thread_id = ThreadId(thread_id.to_string());
        let resume_tickets = run
            .pending
            .clone()
            .map(
                |pending| awaken_agent_contract::thread::read::recovery::RunResumeTicket {
                    run_id: run.run_id.clone(),
                    ticket: awaken_agent_contract::agent::awaiting::ResumeTicket::new(
                        format!("adapter-awaiting-ticket:{}", pending.tool_use_id),
                        run.run_id.clone(),
                        thread_id.clone(),
                        "adapter-snapshot",
                        "adapter-catalog",
                        awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
                            reason: match self.scenario {
                                AdapterToolScenario::Permission => awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission,
                                AdapterToolScenario::Custom => awaken_agent_contract::agent::awaiting::ToolAwaitReason::ClientExecution,
                            },
                            call_id: pending.tool_use_id,
                            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                                tool_id: pending.name,
                                arguments: pending.input,
                            },
                        },
                    ),
                },
            )
            .into_iter()
            .collect();
        Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                thread_id: thread_id.clone(),
                claimed_run_id: run.run_id.clone(),
                runs: vec![awaken_agent_contract::agent::run::Record {
                    id: run.run_id.clone(),
                    thread_id,
                    state: run.state.clone(),
                }],
                latest_run_id: Some(run.run_id.clone()),
                messages: run.messages.clone(),
                message_commit_cursors: run.message_commit_cursors.clone(),
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets,
                thread_version: u64::try_from(run.lifecycle.len()).unwrap(),
                store_cursor: run
                    .lifecycle
                    .last()
                    .map_or(0, |event| event.source_commit_cursor),
                next_commit_ordinal: u64::try_from(run.messages.len()).unwrap(),
            },
        )
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ToolAwaitingFake {
    async fn reserve_session_user_run(
        &self,
        command: SessionUserRunCommand,
    ) -> Result<SessionUserRunReservation, RunError> {
        let mut slot = self.run.lock().unwrap();
        if let Some(existing) = slot.as_ref() {
            return if existing.run_id == command.run_id {
                Ok(SessionUserRunReservation::Completed)
            } else {
                Err(RunError::bad_request(
                    "adapter fixture already owns a different Run",
                ))
            };
        }
        let pending = self.pending();
        let messages = vec![
            Message::new(
                Id::session_event_input(&command.session_id, &command.operation_id),
                Role::User,
                command.content,
            ),
            Message {
                id: Id(format!("{}/tool-use", command.run_id.0)),
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: pending.tool_use_id.clone(),
                    name: pending.name.clone(),
                    input: pending.input.clone(),
                }],
            },
        ];
        let opening_commit_cursor = 1;
        let awaiting_commit_cursor = 2;
        let lifecycle = vec![
            RunLifecycleEvent {
                cursor: encode_run_lifecycle_cursor(opening_commit_cursor, 0).unwrap(),
                source_commit_cursor: opening_commit_cursor,
                thread_id: ThreadId(command.session_id.clone()),
                run_id: command.run_id.clone(),
                kind: RunLifecycleEventKind::Running,
                state: RunState::Running,
                await_reason: None,
            },
            RunLifecycleEvent {
                cursor: encode_run_lifecycle_cursor(awaiting_commit_cursor, 0).unwrap(),
                source_commit_cursor: awaiting_commit_cursor,
                thread_id: ThreadId(command.session_id.clone()),
                run_id: command.run_id.clone(),
                kind: RunLifecycleEventKind::Awaiting,
                state: RunState::Awaiting,
                await_reason: Some(self.await_reason()),
            },
        ];
        *slot = Some(AdapterToolRun {
            session_id: command.session_id,
            run_id: command.run_id,
            state: RunState::Awaiting,
            message_commit_cursors: vec![opening_commit_cursor, awaiting_commit_cursor],
            messages,
            lifecycle,
            pending: Some(pending),
        });
        Ok(SessionUserRunReservation::Completed)
    }

    async fn session_user_run_state(
        &self,
        session_id: &str,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        Ok(self
            .run
            .lock()
            .unwrap()
            .as_ref()
            .filter(|run| run.session_id == session_id && run.run_id == *run_id)
            .map(|run| run.state.clone()))
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(self
            .run
            .lock()
            .unwrap()
            .as_ref()
            .filter(|run| run.session_id == thread)
            .map(|run| run.messages.clone())
            .unwrap_or_default())
    }

    async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(self
            .run
            .lock()
            .unwrap()
            .as_ref()
            .filter(|run| run.session_id == thread)
            .and_then(|run| run.pending.clone()))
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(self.recovery_snapshot(session_id, thread_id))
    }

    async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        let events = self
            .run
            .lock()
            .unwrap()
            .as_ref()
            .filter(|run| run.session_id == thread)
            .into_iter()
            .flat_map(|run| run.lifecycle.iter())
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        Ok(RunLifecyclePage {
            next_cursor: events.last().map_or(cursor, |event| event.cursor),
            events,
        })
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, RunError> {
        let run = self.run.lock().unwrap();
        let run = run
            .as_ref()
            .ok_or_else(|| RunError::bad_request("adapter fixture has no pending Run"))?;
        let pending = run
            .pending
            .as_ref()
            .ok_or_else(|| RunError::bad_request("adapter fixture Run is not Awaiting"))?;
        if command.target != awaken_session_contract::SessionThreadTarget::Primary
            || command.session_id != run.session_id
            || command.expected_run_id != run.run_id
            || command.expected_correlation_id
                != format!("adapter-awaiting-ticket:{}", pending.tool_use_id)
            || command.tool_use_id != pending.tool_use_id
        {
            return Err(RunError::bad_request(
                "adapter fixture reply does not match the committed ticket",
            ));
        }
        Ok(awaken_session_contract::SessionThreadToolReplyFence {
            // Session creation owns epoch 1; the completed Awaiting boundary
            // settled it but the durable dispatch fence still identifies that
            // prior epoch when the reply transfers activity to the same Run.
            prior_session_activity_epoch: Some(1),
        })
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        let expected = self
            .session_thread_tool_reply_fence(&delivery.command)
            .await?;
        if delivery.fence != expected {
            return Err(RunError::bad_request("adapter fixture reply fence changed"));
        }
        let mut slot = self.run.lock().unwrap();
        let run = slot
            .as_mut()
            .ok_or_else(|| RunError::bad_request("adapter fixture has no Run"))?;
        let pending = run
            .pending
            .take()
            .ok_or_else(|| RunError::bad_request("adapter fixture Run already resumed"))?;
        let (content, result_text) = match (&self.scenario, &delivery.command.reply) {
            (
                AdapterToolScenario::Permission,
                awaken_session_contract::SessionThreadToolReply::Confirm(
                    ToolPermissionDecision::Allow { .. },
                ),
            ) => (vec![ContentBlock::text("wrote x.txt")], "done".to_string()),
            (
                AdapterToolScenario::Custom,
                awaken_session_contract::SessionThreadToolReply::Custom {
                    content,
                    is_error: false,
                },
            ) => (content.clone(), format!("got: {}", extract_text(content))),
            _ => {
                return Err(RunError::bad_request(
                    "adapter fixture reply family does not match the pending tool",
                ));
            }
        };
        // The staged reply is one atomic Thread commit: its ToolResult, final
        // assistant Message, Resumed edge, and terminal edge therefore share
        // one durable source coordinate while retaining distinct feed cursors.
        let reply_commit_cursor = run
            .lifecycle
            .last()
            .map_or(1, |event| event.source_commit_cursor + 1);
        run.messages.push(Message {
            id: Id(format!("{}/tool-result", run.run_id.0)),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: pending.tool_use_id,
                content,
                is_error: false,
            }],
        });
        run.messages.push(Message::text(
            Id(format!("{}/assistant-final", run.run_id.0)),
            Role::Assistant,
            result_text,
        ));
        run.state = RunState::Ended(EndCause::NaturalEnd);
        run.message_commit_cursors
            .extend([reply_commit_cursor, reply_commit_cursor]);
        run.lifecycle.push(RunLifecycleEvent {
            cursor: encode_run_lifecycle_cursor(reply_commit_cursor, 0).unwrap(),
            source_commit_cursor: reply_commit_cursor,
            thread_id: ThreadId(run.session_id.clone()),
            run_id: run.run_id.clone(),
            kind: RunLifecycleEventKind::Resumed,
            state: RunState::Running,
            await_reason: None,
        });
        run.lifecycle.push(RunLifecycleEvent {
            cursor: encode_run_lifecycle_cursor(reply_commit_cursor, 1).unwrap(),
            source_commit_cursor: reply_commit_cursor,
            thread_id: ThreadId(run.session_id.clone()),
            run_id: run.run_id.clone(),
            kind: RunLifecycleEventKind::Completed,
            state: run.state.clone(),
            await_reason: None,
        });
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal(
            "adapter fixture accepts User input only through durable reservation",
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal(
            "adapter fixture replies only through the Session coordination port",
        ))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal(
            "adapter fixture replies only through the Session coordination port",
        ))
    }

    fn capabilities(&self) -> AgentCapabilities {
        match self.scenario {
            AdapterToolScenario::Permission => AgentCapabilities {
                builtin_tools: vec![BuiltinTool {
                    name: "write".into(),
                    ask: true,
                }],
                ..Default::default()
            },
            AdapterToolScenario::Custom => AgentCapabilities {
                custom_tools: vec![CustomTool {
                    name: "submit_answer".into(),
                    description: "Submit an answer".into(),
                    input_schema: serde_json::json!({ "type": "object" }),
                }],
                ..Default::default()
            },
        }
    }

    fn model(&self) -> String {
        "test-model".into()
    }
}

/// A fake durable Outcome owner: prepare atomically exposes the committed report
/// that the adapter's read-only projector consumes. Outcome execution/recovery
/// itself is covered by the Outcome aggregate and lifecycle-supervisor tests;
/// this fixture owns no second drive loop.
#[derive(Default)]
struct OutcomeFake {
    reports: Mutex<HashMap<String, (u64, OutcomeReport)>>,
    next_commit_cursor: AtomicU64,
    prepare_gate: Option<Arc<OutcomePrepareGate>>,
}

#[derive(Default)]
struct OutcomePrepareGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl SessionRuntime for OutcomeFake {
    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if session_id != thread_id {
            return Ok(None);
        }
        let reports = self.reports.lock().unwrap();
        if reports.is_empty() {
            return Ok(None);
        }
        let mut reports = reports.iter().collect::<Vec<_>>();
        reports.sort_by(|(left, _), (right, _)| left.cmp(right));
        let thread_version = u64::try_from(reports.len()).unwrap();
        let mut state = Vec::new();
        let mut state_commit_cursors = Vec::new();
        for (outcome_id, (source_commit_cursor, report)) in reports {
            for (offset, iteration) in report.iterations.iter().enumerate() {
                state.push(awaken_agent_contract::agent::state::Command::set(
                    awaken_agent_contract::agent::state::Scope::Thread,
                    awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                    format!("outcome/{outcome_id}/evaluation/{}", iteration.iteration),
                    serde_json::json!({"fixture": "committed"}),
                ));
                state_commit_cursors.push(source_commit_cursor + u64::try_from(offset).unwrap());
            }
        }
        let store_cursor = state_commit_cursors.iter().copied().max().unwrap_or(0);
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                thread_id: ThreadId(thread_id.to_string()),
                claimed_run_id: RunId(format!("outcome-snapshot:{thread_id}")),
                runs: Vec::new(),
                latest_run_id: None,
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                state,
                state_commit_cursors,
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version,
                store_cursor,
                next_commit_ordinal: 0,
            },
        ))
    }

    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no Run"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn prepare_outcome(
        &self,
        _t: &str,
        outcome_id: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<u64, RunError> {
        if let Some(gate) = &self.prepare_gate {
            gate.entered.notify_waiters();
            gate.release.notified().await;
        }
        let source_commit_cursor = self.next_commit_cursor.fetch_add(2, Ordering::SeqCst) + 1;
        self.reports.lock().unwrap().insert(
            outcome_id.to_string(),
            (
                source_commit_cursor,
                OutcomeReport {
                    iterations: vec![
                        OutcomeIteration {
                            messages: Vec::new(),
                            outcome_id: outcome_id.to_string(),
                            description: "produce final answer".into(),
                            iteration: 1,
                            result: "needs_revision".into(),
                            explanation: "add FINAL".into(),
                        },
                        OutcomeIteration {
                            messages: vec![Message::text(
                                Id("r".into()),
                                Role::Assistant,
                                "FINAL answer",
                            )],
                            outcome_id: outcome_id.to_string(),
                            description: "produce final answer".into(),
                            iteration: 2,
                            result: "satisfied".into(),
                            explanation: "ok".into(),
                        },
                    ],
                },
            ),
        );
        Ok(source_commit_cursor)
    }
    async fn committed_outcome_projection(
        &self,
        _thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcomeProjection>, RunError> {
        Ok(self
            .reports
            .lock()
            .unwrap()
            .get(outcome_id)
            .map(|(_, report)| report.clone())
            .map(CommittedOutcomeProjection::Completed))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn outcome_send_returns_the_root_receipt_before_lifecycle_execution() {
    // Causes: the fixtures below establish `outcome send` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a valid DefineOutcome is atomically retained;
    // C2 the sole lifecycle supervisor enters prepare but its dependency is
    // blocked. Effects: E1 HTTP returns the exact retained receipt without
    // waiting for C2, E2 the receipt is still unprocessed, and E3 releasing C2
    // lets the same supervisor continue. Constraint: no request-local Outcome
    // executor or second task may bypass the Session root.
    //
    // | Rule | Root admitted | lifecycle prepare | Effect |
    // |---|---|---|---|
    // | R1 | no | n/a | no receipt |
    // | R2 | yes | blocked | E1 + E2 |
    // | R3 | yes | released | E3 through the sole supervisor |
    let gate = Arc::new(OutcomePrepareGate::default());
    let state = Arc::new(ManagedState::new(OutcomeFake {
        reports: Default::default(),
        next_commit_cursor: AtomicU64::new(0),
        prepare_gate: Some(gate.clone()),
    }));
    let cancellation = awaken_runtime_contract::CancellationToken::new();
    let supervisor = tokio::spawn(
        state
            .session_application()
            .run_lifecycle_supervisor(cancellation.clone()),
    );
    tokio::task::yield_now().await;
    let app = router(state);
    let id = create(&app).await;
    let entered = gate.entered.notified();
    let request = tokio::spawn({
        let app = app.clone();
        let id = id.clone();
        async move {
            json_call(
                &app,
                "POST",
                &format!("/v1/sessions/{id}/events"),
                serde_json::json!({ "events": [{
                    "type": "user.define_outcome",
                    "description": "finish",
                    "rubric": { "type": "text", "content": "FINAL" }
                }] }),
            )
            .await
        }
    });
    entered.await;
    let receipt = tokio::time::timeout(std::time::Duration::from_millis(100), request)
        .await
        .expect("R2/E1 receipt must not await Outcome execution")
        .expect("R2 request task");
    assert_eq!(receipt["data"][0]["type"], "user.define_outcome", "R2/E1");
    assert!(receipt["data"][0]["processed_at"].is_null(), "R2/E2");

    gate.release.notify_waiters();
    cancellation.cancel();
    supervisor
        .await
        .expect("R3 supervisor task")
        .expect("R3/E3 cooperative stop");
}

#[tokio::test]
async fn outcome_loop_projects_evaluations() {
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Causes: explicit max_iterations is the inclusive minimum/maximum or one
    // outside either edge. Constraint: 1..=20. Effects: H8 admits 1/20 and rejects
    // 0/21 before the inbound event is persisted.
    for (iterations, expected) in [
        (0, StatusCode::BAD_REQUEST),
        (1, StatusCode::OK),
        (20, StatusCode::OK),
        (21, StatusCode::BAD_REQUEST),
    ] {
        let boundary_app = router(Arc::new(ManagedState::new(OutcomeFake::default())));
        let boundary_id = create(&boundary_app).await;
        let (status, _) = json_response(
            &boundary_app,
            "POST",
            &format!("/v1/sessions/{boundary_id}/events"),
            serde_json::json!({ "events": [{
                "type": "user.define_outcome",
                "description": "finish",
                "rubric": { "type": "text", "content": "FINAL" },
                "max_iterations": iterations
            }] }),
        )
        .await;
        assert_eq!(status, expected, "H8 max_iterations={iterations}");
        if expected == StatusCode::BAD_REQUEST {
            let events = json_call(
                &boundary_app,
                "GET",
                &format!("/v1/sessions/{boundary_id}/events"),
                serde_json::Value::Null,
            )
            .await;
            assert!(
                events["data"].as_array().unwrap().is_empty(),
                "H8 invalid boundary is atomic"
            );
        }
    }

    let state = Arc::new(ManagedState::new(OutcomeFake::default()));
    let cancellation = awaken_runtime_contract::CancellationToken::new();
    let supervisor = tokio::spawn(
        state
            .session_application()
            .run_lifecycle_supervisor(cancellation.clone()),
    );
    tokio::task::yield_now().await;
    let app = router(state);
    let id = create(&app).await;
    let sent = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish", "rubric": { "type": "text", "content": "FINAL" }, "max_iterations": 3 }] }),
    )
    .await;
    let echoed_outcome = &sent["data"][0];
    assert_eq!(echoed_outcome["type"], "user.define_outcome");
    assert_eq!(echoed_outcome["description"], "finish");
    assert_eq!(echoed_outcome["rubric"]["content"], "FINAL");
    assert_eq!(echoed_outcome["max_iterations"], 3);
    assert!(
        echoed_outcome["outcome_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("outc_")),
        "H8 full Outcome echo uses the official identity family"
    );
    // Projection cause/effect: C1 the retained root command is processed and C2
    // the Outcome aggregate exposes a committed two-iteration report, but C3
    // this wire fixture has no committed Run/message lifecycle. Rule O1=C1+C2+
    // not-C3 -> echo plus exactly two span triples, with no fabricated status,
    // usage, or assistant Message; Runtime/Host E2E owns the C3-positive rule.
    let mut list = serde_json::Value::Null;
    for _ in 0..100 {
        list = json_call(
            &app,
            "GET",
            &format!("/v1/sessions/{id}/events"),
            serde_json::Value::Null,
        )
        .await;
        if types(&list)
            .iter()
            .filter(|kind| kind.as_str() == "span.outcome_evaluation_end")
            .count()
            == 2
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        types(&list),
        vec![
            "user.define_outcome",
            "span.outcome_evaluation_start",
            "span.outcome_evaluation_ongoing",
            "span.outcome_evaluation_end",
            "span.outcome_evaluation_start",
            "span.outcome_evaluation_ongoing",
            "span.outcome_evaluation_end"
        ]
    );
    let ends: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "span.outcome_evaluation_end")
        .collect();
    let outcome_id = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "user.define_outcome")
        .and_then(|event| event["outcome_id"].as_str())
        .expect("input echo carries the server Outcome identity");
    assert!(
        ends.iter().all(|event| event["outcome_id"] == outcome_id),
        "every evaluation span references the echoed Outcome identity"
    );
    assert_eq!(ends[0]["result"], "needs_revision");
    assert_eq!(ends[1]["result"], "satisfied");
    cancellation.cancel();
    supervisor
        .await
        .expect("O1 supervisor task")
        .expect("O1 supervisor stop");
}

#[tokio::test]
async fn hitl_await_confirm_resume() {
    // Causes: the fixtures below establish `hitl await confirm resume` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let state = Arc::new(ManagedState::new(ToolAwaitingFake::permission()));
    let app = router(state.clone());
    let id = create(&app).await;

    // H1 cause/effect rule: one accepted primary Run reaches an answerable
    // permission gate; therefore Session and primary Thread each publish one
    // complete running -> requires_action bracket, both terminal reasons carry
    // the exact qualified tool id, and every Thread status uses the listed
    // public `sthr_` identity.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "write it" }] }] }),
    )
    .await;
    support::drive_retained_session_events(&state, &id).await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.tool_use",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ]
    );

    let tool_use = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.tool_use")
        .unwrap();
    let public_tool_id = tool_use["id"].as_str().unwrap().to_owned();
    assert_ne!(public_tool_id, "call-1", "H1 qualifies batch-local ids");
    assert_eq!(tool_use["evaluated_permission"], "ask");
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], public_tool_id);
    let primary_thread_id = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(primary_thread_id.starts_with("sthr_"), "H1 public codec");
    let thread_idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "session.thread_status_idle")
        .unwrap();
    assert_eq!(thread_idle["session_thread_id"], primary_thread_id, "H1");
    assert_eq!(thread_idle["stop_reason"]["type"], "requires_action");
    assert_eq!(
        thread_idle["stop_reason"]["event_ids"][0], public_tool_id,
        "H1 primary Thread and aggregate Session carry the same answerable identity"
    );

    // Causes/constraints while requires_action: System is legal only after a
    // User message or client-executed tool result, never after a permission
    // confirmation; a wrong id or wrong reply kind cannot resolve the ticket.
    // Effects: every invalid batch is rejected before persistence; the exact
    // confirmation alone resumes the same Run. Decision rules: H5-H7.
    for (rule, events) in [
        (
            "H5 system alone",
            serde_json::json!([{
                "type":"system.message",
                "content":[{"type":"text", "text":"after tool"}]
            }]),
        ),
        (
            "H5 system plus user message",
            serde_json::json!([
                {"type":"system.message", "content":[{"type":"text", "text":"after tool"}]},
                {"type":"user.message", "content":[{"type":"text", "text":"continue"}]}
            ]),
        ),
        (
            "H5 user message cannot bypass the pending reply",
            serde_json::json!([{
                "type":"user.message",
                "content":[{"type":"text", "text":"continue without resolving"}]
            }]),
        ),
        (
            "H7 wrong tool id",
            serde_json::json!([{
                "type":"user.tool_confirmation",
                "tool_use_id":"call-wrong",
                "result":"allow"
            }]),
        ),
        (
            "H7 wrong resolution kind",
            serde_json::json!([{
                "type":"user.custom_tool_result",
                "custom_tool_use_id":public_tool_id,
                "content":[{"type":"text", "text":"forged"}]
            }]),
        ),
        (
            "H5 confirmation cannot carry system context",
            serde_json::json!([
                {"type":"user.tool_confirmation", "tool_use_id":public_tool_id, "result":"allow"},
                {"type":"system.message", "content":[{"type":"text", "text":"after tool"}]}
            ]),
        ),
    ] {
        let (status, _) = json_response(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({"events": events}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
    }
    let unchanged = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(types(&unchanged), types(&list), "H5/H7 no partial events");

    // H6 cause/effect: the exact confirmation is durably staged against the
    // committed ticket, transfers aggregate activity once, and the same Run's
    // new committed prefix is projected. The transfer therefore opens one
    // aggregate Running edge before Thread output.
    // The retained confirmation and staged reply share one atomic source
    // coordinate; the canonical phase order keeps the inbound receipt before
    // the Run's Resumed edge and output.
    // This narrow adapter fixture intentionally has no Host settlement observer,
    // so H6 asserts Thread terminal truth only; the official SDK E2E and Host
    // decision table own aggregate Session activity/usage settlement.
    let confirmation = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "user.tool_confirmation", "tool_use_id": public_tool_id, "result": "allow" }
        ] }),
    )
    .await;
    assert_eq!(confirmation["data"][0]["type"], "user.tool_confirmation");
    assert_eq!(confirmation["data"][0]["tool_use_id"], public_tool_id);
    assert_eq!(confirmation["data"][0]["result"], "allow");
    assert!(confirmation["data"][0].get("content").is_none());
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.tool_use",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle",
            "user.tool_confirmation",
            "session.status_running",
            "session.thread_status_running",
            "agent.tool_result",
            "agent.message",
            "session.thread_status_idle"
        ]
    );
    let last_idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["type"] == "session.thread_status_idle")
        .unwrap();
    assert_eq!(last_idle["stop_reason"]["type"], "end_turn");
}

#[tokio::test]
async fn custom_tool_use_await_and_result() {
    // Causes: the fixtures below establish `custom tool use await and result` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Custom-result cause/effect decision table:
    // R1 matching pending id + text only -> resume and end; R2 matching id +
    // text/image blocks -> preserve ordered blocks through SessionRuntime and the
    // committed agent.tool_result; R3 wrong id/kind (covered by batch validation)
    // -> reject without a partial event. R2's adapter effect is the same Thread's
    // terminal committed prefix; aggregate Session settlement belongs to the
    // Host observer and official SDK E2E, not this test fixture.
    let state = Arc::new(ManagedState::new(ToolAwaitingFake::custom()));
    let app = router(state.clone());
    let id = create(&app).await;

    // A message -> the client tool awaits as agent.custom_tool_use.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "answer" }] }] }),
    )
    .await;
    support::drive_retained_session_events(&state, &id).await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.custom_tool_use",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ]
    );
    let custom = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.custom_tool_use")
        .unwrap();
    let public_tool_id = custom["id"].as_str().unwrap().to_owned();
    assert_ne!(public_tool_id, "cc1", "R2 qualifies the batch-local id");
    assert_eq!(custom["name"], "submit_answer");
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], public_tool_id);

    // The client returns the result -> the run resumes and completes. W3
    // verifies the response reuses the complete custom-result Event shape.
    let custom_receipt = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{
            "type": "user.custom_tool_result",
            "custom_tool_use_id": public_tool_id,
            "content": [
                { "type": "text", "text": "42" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } }
            ]
        }] }),
    )
    .await;
    assert_eq!(custom_receipt["data"][0]["type"], "user.custom_tool_result");
    assert_eq!(
        custom_receipt["data"][0]["custom_tool_use_id"],
        public_tool_id
    );
    assert_eq!(custom_receipt["data"][0]["content"][0]["text"], "42");
    assert_eq!(custom_receipt["data"][0]["is_error"], false);
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let msgs: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("got: 42")),
        "messages: {msgs:?}"
    );
    let tool_result = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "agent.tool_result")
        .expect("runtime result is projected");
    assert_eq!(tool_result["content"][0]["text"], "42");
    assert_eq!(
        tool_result["content"][1]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(tool_result["content"][1]["source"]["data"], "iVBORw0KGgo=");
    let last_idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["type"] == "session.thread_status_idle")
        .unwrap();
    assert_eq!(last_idle["stop_reason"]["type"], "end_turn");
}

#[tokio::test]
async fn interrupt_event_is_acknowledged_without_starting_a_run() {
    // Causes: the fixtures below establish `interrupt event` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is acknowledged without starting a run` and every asserted
    // state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect rule: C1 an interrupt targets the idle primary Thread and C2
    // the Runtime accepts it. Effect I1 retains and processes the exact inbound
    // id; I2 starts no Run and therefore emits no fabricated usage/lifecycle.
    // Decision rule R1=C1+C2 -> I1+I2. Runtime failure/retry is owned by the
    // retained-batch reconciliation table, not by a second adapter path.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let id = create(&app).await;

    let receipts = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.interrupt" }] }),
    )
    .await;
    let receipt_types: Vec<&str> = receipts["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(receipt_types, vec!["user.interrupt"]);

    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(types(&list), vec!["user.interrupt"], "R1/I2");
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["processed_at"].is_string()),
        "accepted inbound events become processed"
    );
    assert_eq!(receipts["data"][0]["id"], list["data"][0]["id"]);
}

/// A runtime that records the `interrupt` thread and request attribution it is
/// handed. System messages are intentionally not mirrored here: their sole durable
/// owner is the Session root, covered by the Session-application authority table.
struct RecordingFake {
    interrupts: Arc<Mutex<Vec<String>>>,
    subjects: Arc<Mutex<Vec<Option<String>>>>,
    committed: EchoFake,
    supports_mid_conversation_system: bool,
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingFake {
    async fn reserve_session_user_run(
        &self,
        command: SessionUserRunCommand,
    ) -> Result<SessionUserRunReservation, RunError> {
        self.subjects
            .lock()
            .unwrap()
            .push(command.data_subject_id.clone());
        self.committed.reserve_session_user_run(command).await
    }

    async fn session_user_run_state(
        &self,
        session_id: &str,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        self.committed
            .session_user_run_state(session_id, run_id)
            .await
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.committed.committed_messages(thread).await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.committed
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await
    }

    async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        self.committed
            .committed_run_lifecycle(thread, cursor, limit)
            .await
    }

    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn supports_mid_conversation_system(&self, _thread: &str) -> bool {
        self.supports_mid_conversation_system
    }
    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupts.lock().unwrap().push(thread.to_string());
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// Cause/effect graph: C1=one System event; C2=it is final; C3=its predecessor is
/// User message/tool result; C4=text-only count 1..=1000; C5=model capability.
/// Effects: E1=whole batch accepted in request order and the User Run is driven;
/// E2=400 before any Event or Run; E3=System reaches the sole Session-root owner.
/// Constraints: confirmation, interrupt, and Outcome are not legal predecessors.
/// Decision table: H1(all true)->E1+E3; H2(C4 boundary 1000)->E1; H3(any
/// C1..C4 false)->E2; H4(C5 false)->E2. Lower-layer authority tests own exact
/// replay, CAS conflicts, Runtime projection, and committed Message idempotency.
#[tokio::test]
async fn system_message_follows_the_batch_admission_decision_table() {
    // Causes: the fixtures below establish `system message` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let interrupts = Arc::new(Mutex::new(Vec::new()));
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(ManagedState::new(RecordingFake {
        interrupts: interrupts.clone(),
        subjects: subjects.clone(),
        committed: EchoFake::default(),
        supports_mid_conversation_system: true,
    }));
    let app = router(state.clone());
    let id = create(&app).await;

    let receipt = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "user.message", "content": [{ "type": "text", "text": "answer tersely" }] },
            { "type": "system.message", "content": [{ "type": "text", "text": "be terse" }] }
        ] }),
    )
    .await;
    support::drive_retained_session_events(&state, &id).await;
    assert_eq!(
        types(&receipt),
        vec!["user.message", "system.message"],
        "H1"
    );
    assert_eq!(receipt["data"][0]["content"][0]["text"], "answer tersely");
    assert_eq!(receipt["data"][1]["content"][0]["text"], "be terse");
    assert_eq!(
        subjects.lock().unwrap().len(),
        1,
        "H1 drives exactly one Run"
    );

    let thousand = vec![serde_json::json!({"type": "text", "text": "x"}); 1000];
    let (status, _) = json_response(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({"events": [
            {"type":"user.message", "content":[{"type":"text", "text":"boundary"}]},
            {"type":"system.message", "content":thousand}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H2 inclusive maximum");
    support::drive_retained_session_events(&state, &id).await;
    assert_eq!(
        subjects.lock().unwrap().len(),
        2,
        "H2 drives exactly one Run"
    );

    let before_invalid = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let invalid_cases = [
        (
            "H3 standalone",
            serde_json::json!([{"type":"system.message", "content":[{"type":"text", "text":"x"}]}]),
        ),
        (
            "H3 non-final",
            serde_json::json!([
                {"type":"user.message", "content":[{"type":"text", "text":"must not run"}]},
                {"type":"system.message", "content":[{"type":"text", "text":"x"}]},
                {"type":"user.interrupt"}
            ]),
        ),
        (
            "H3 multiple",
            serde_json::json!([
                {"type":"user.message", "content":[{"type":"text", "text":"must not run"}]},
                {"type":"system.message", "content":[{"type":"text", "text":"x"}]},
                {"type":"system.message", "content":[{"type":"text", "text":"y"}]}
            ]),
        ),
        (
            "H3 confirmation predecessor",
            serde_json::json!([
                {"type":"user.tool_confirmation", "tool_use_id":"missing", "result":"allow"},
                {"type":"system.message", "content":[{"type":"text", "text":"x"}]}
            ]),
        ),
        (
            "H3 Outcome predecessor",
            serde_json::json!([
                {"type":"user.define_outcome", "description":"x", "rubric":{"type":"text", "content":"y"}},
                {"type":"system.message", "content":[{"type":"text", "text":"x"}]}
            ]),
        ),
        (
            "H3 empty",
            serde_json::json!([
                {"type":"user.message", "content":[{"type":"text", "text":"must not run"}]},
                {"type":"system.message", "content":[]}
            ]),
        ),
        (
            "H3 over maximum",
            serde_json::json!([
                {"type":"user.message", "content":[{"type":"text", "text":"must not run"}]},
                {"type":"system.message", "content":vec![serde_json::json!({"type":"text", "text":"x"}); 1001]}
            ]),
        ),
        (
            "H3 non-text content",
            serde_json::json!([
                {"type":"user.message", "content":[{"type":"text", "text":"must not run"}]},
                {"type":"system.message", "content":[{"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"AA=="}}]}
            ]),
        ),
    ];
    for (rule, events) in invalid_cases {
        let (status, _) = json_response(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({"events": events}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
    }
    let after_invalid = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        after_invalid["data"], before_invalid["data"],
        "H3 rejects the whole batch before persistence"
    );
    assert_eq!(subjects.lock().unwrap().len(), 2, "H3 starts no Run");
    assert!(
        interrupts.lock().unwrap().is_empty(),
        "H3 has no interrupt effect"
    );

    let unsupported = router(Arc::new(ManagedState::new(RecordingFake {
        interrupts: Arc::new(Mutex::new(Vec::new())),
        subjects: Arc::new(Mutex::new(Vec::new())),
        committed: EchoFake::default(),
        supports_mid_conversation_system: false,
    })));
    let unsupported_id = create(&unsupported).await;
    let (status, body) = json_response(
        &unsupported,
        "POST",
        &format!("/v1/sessions/{unsupported_id}/events"),
        serde_json::json!({"events": [
            {"type":"user.message", "content":[{"type":"text", "text":"must not run"}]},
            {"type":"system.message", "content":[{"type":"text", "text":"x"}]}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "H4 unsupported model");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("model_does_not_support_mid_conversation_system")
    );
    let events = json_call(
        &unsupported,
        "GET",
        &format!("/v1/sessions/{unsupported_id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert!(events["data"].as_array().unwrap().is_empty(), "H4 no event");
}

/// Official event-envelope decision table: R1 removed `user_profile_id` present
/// -> 400 and no Runtime effect; R2 official user.message only -> admitted with
/// no attribution; R3 the same envelope plus request-context header -> the
/// neutral runtime receives the subject. User Profiles remain a separate resource
/// and do not add a field to the Session events SDK shape.
#[tokio::test]
async fn managed_event_attribution_uses_request_header_not_body_field() {
    // Causes: the fixtures below establish `managed event attribution` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `uses request header not body field` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(ManagedState::new(RecordingFake {
        interrupts: Arc::new(Mutex::new(Vec::new())),
        subjects: subjects.clone(),
        committed: EchoFake::default(),
        supports_mid_conversation_system: true,
    }));
    let app = router(state.clone());
    let id = create(&app).await;
    let (status, _) = json_response(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({
            "events": [{
                "type": "user.message",
                "content": [{ "type": "text", "text": "attributed" }]
            }],
            "user_profile_id": "user_alice"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "R1");
    assert!(subjects.lock().unwrap().is_empty(), "R1");

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({"events": [{
            "type": "user.message",
            "content": [{"type": "text", "text": "unattributed"}]
        }]}),
    )
    .await;
    support::drive_retained_session_events(&state, &id).await;
    assert_eq!(*subjects.lock().unwrap(), vec![None], "R2");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sessions/{id}/events"))
                .header("content-type", "application/json")
                .header("anthropic-user-profile-id", "user_alice")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"events": [{
                        "type": "user.message",
                        "content": [{"type": "text", "text": "attributed"}]
                    }]}))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "R3");
    support::drive_retained_session_events(&state, &id).await;
    assert_eq!(
        *subjects.lock().unwrap(),
        vec![None, Some("user_alice".into())],
        "R3"
    );
}

/// A runtime that records the ORDER of `interrupt` vs `run` calls and echoes each
/// Run, so a test can prove the documented interrupt-then-redirect batch flow.
struct InterruptRedirectFake {
    order: Arc<Mutex<Vec<String>>>,
    committed: EchoFake,
}

#[async_trait::async_trait]
impl SessionRuntime for InterruptRedirectFake {
    async fn reserve_session_user_run(
        &self,
        command: SessionUserRunCommand,
    ) -> Result<SessionUserRunReservation, RunError> {
        let text = Message::new(
            Id::session_event_input(&command.session_id, &command.operation_id),
            Role::User,
            command.content.clone(),
        )
        .text_content();
        self.order.lock().unwrap().push(format!("run:{text}"));
        self.committed.reserve_session_user_run(command).await
    }

    async fn session_user_run_state(
        &self,
        session_id: &str,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        self.committed
            .session_user_run_state(session_id, run_id)
            .await
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.committed.committed_messages(thread).await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.committed
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await
    }

    async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        self.committed
            .committed_run_lifecycle(thread, cursor, limit)
            .await
    }

    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal(
            "adapter fixture accepts User input only through durable reservation",
        ))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        self.order.lock().unwrap().push("interrupt".into());
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// The documented interrupt-then-redirect batch (events-and-streaming: "Send a
/// `user.interrupt` event to stop the agent mid-execution, then follow up with a
/// `user.message` event to redirect it"): a single `events` array carrying
/// `[user.interrupt, user.message]` interrupts first, then runs the redirect Run —
/// in that order — and the new direction produces the Run's `agent.message`.
#[tokio::test]
async fn interrupt_then_message_redirects_in_order() {
    // Causes: the fixtures below establish `interrupt then message redirects in order` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `interrupt then message redirects in order` is one independent branch
    // selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let order = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(ManagedState::new(InterruptRedirectFake {
        order: order.clone(),
        committed: EchoFake::default(),
    }));
    let app = router(state.clone());
    let id = create(&app).await;

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "user.interrupt" },
            { "type": "user.message", "content": [{ "type": "text", "text": "fix line 42 instead" }] }
        ] }),
    )
    .await;
    support::drive_retained_session_events(&state, &id).await;

    // The interrupt is handled before the redirect Run starts (documented order).
    assert_eq!(
        *order.lock().unwrap(),
        vec![
            "interrupt".to_string(),
            "run:fix line 42 instead".to_string()
        ],
        "interrupt is processed first, then the redirect message starts the Run"
    );
    // The redirect produced this Run's agent.message (the new direction ran).
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert!(
        types(&list).contains(&"agent.message".to_string()),
        "the redirect Run projected an agent.message: {}",
        list
    );
}

#[tokio::test]
async fn retrieve_session_and_sse_event_names() {
    // Coverage rationale: `retrieve session and sse event names` is one independent branch
    // selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let state = Arc::new(ManagedState::new(EchoFake::default()));
    let app = router(state.clone());
    let id = create(&app).await;

    // GET /v1/sessions/{id} returns the session.
    let session = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(session["id"], id);
    assert_eq!(session["type"], "session");

    // Execute a Run, then the SSE stream carries `event:` lines named by type.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    support::drive_retained_session_events(&state, &id).await;
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/sessions/{id}/events/stream"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    // Causes: C1 the retained User command has reached one committed terminal
    // prefix; C2 the SSE snapshot contains that terminal edge. Effects: E1 the
    // stream replays the committed event names; E2 the test stops at the exact
    // terminal frame. Constraint: an active SSE is intentionally open-ended,
    // so a protocol test must consume through its semantic terminal rather than
    // await transport EOF. Decision rule S1=C1+C2 => E1+E2 within the bound.
    let mut body = resp.into_body();
    let sse = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut sse = String::new();
        loop {
            let frame = body
                .frame()
                .await
                .expect("S1 SSE ended before the terminal frame")
                .expect("S1 readable SSE frame");
            if let Ok(data) = frame.into_data() {
                let chunk = std::str::from_utf8(&data).expect("S1 UTF-8 SSE data");
                sse.push_str(chunk);
            }
            if sse.contains("event: session.status_idle") {
                break sse;
            }
        }
    })
    .await
    .expect("S1 terminal Session SSE frame arrives");
    assert!(sse.contains("event: agent.message"), "sse: {sse}");
    assert!(sse.contains("event: session.status_idle"), "sse: {sse}");
}

/// Read a response's status and JSON body.
async fn raw_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Body,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn unknown_session_is_404_with_error_envelope() {
    // Causes: the fixtures below establish `unknown session` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is 404 with error envelope` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `unknown session` is one independent branch selecting `is 404 with error
    // envelope`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let (status, body) = raw_call(&app, "GET", "/v1/sessions/nope/events", Body::empty()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The Anthropic error envelope the SDK parses.
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "message is populated: {body}"
    );
}

#[tokio::test]
async fn malformed_body_is_400_with_error_envelope() {
    // Causes: the fixtures below establish `malformed body` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is 400 with error envelope` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `malformed body` is one independent branch selecting `is 400 with error
    // envelope`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    // Syntactically broken JSON on a managed route.
    let (status, body) = raw_call(&app, "POST", "/v1/sessions", Body::from("{ not json ")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty())
    );
}

#[tokio::test]
async fn missing_content_type_is_400_with_error_envelope() {
    // Causes: the fixtures below establish `missing content type` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is 400 with error envelope` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `missing content type` is one independent branch selecting `is 400 with
    // error envelope`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    // A body without `content-type: application/json` is rejected as an invalid
    // request in the envelope shape (not axum's plain-text default).
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

// --- Tenant session isolation (ADR-0051): the ownership guard ----------------

use awaken_tenancy::WorkspaceScope;

/// Create a session with an edge-resolved owner scope stamped as `WorkspaceScope`
/// (what the ingress guard does), returning its id.
async fn create_owned(app: &Router, scope: Option<&str>) -> String {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&session_request(serde_json::json!({ "agent": "coder" }))).unwrap(),
        ))
        .unwrap();
    if let Some(scope) = scope {
        req.extensions_mut()
            .insert(WorkspaceScope(scope.to_string()));
    }
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["id"].as_str().unwrap().to_string()
}

/// A request to `uri` carrying (optionally) an edge-resolved `WorkspaceScope`.
async fn call_owned(app: &Router, method: &str, uri: &str, scope: Option<&str>) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    if let Some(scope) = scope {
        req.extensions_mut()
            .insert(WorkspaceScope(scope.to_string()));
    }
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn a_scoped_session_is_invisible_to_another_workspace() {
    // Causes: the fixtures below establish `a scoped session` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is invisible to another workspace` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `a scoped session` is one independent branch selecting `is invisible to
    // another workspace`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let id = create_owned(&app, Some("ws_a")).await;
    let path = format!("/v1/sessions/{id}");
    // The owner reads it.
    assert_eq!(
        call_owned(&app, "GET", &path, Some("ws_a")).await,
        StatusCode::OK
    );
    // Another workspace gets 404 — never 403, so the id's existence is not disclosed.
    assert_eq!(
        call_owned(&app, "GET", &path, Some("ws_b")).await,
        StatusCode::NOT_FOUND
    );
    // A bare (unscoped) request cannot see a scoped session either.
    assert_eq!(
        call_owned(&app, "GET", &path, None).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_cross_tenant_write_is_also_fenced() {
    // Causes: the fixtures below establish `a cross tenant write` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is also fenced` and every asserted state transition or side
    // effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `a cross tenant write` is one independent branch selecting `is also
    // fenced`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let id = create_owned(&app, Some("ws_a")).await;
    // A write (archive) from another workspace is 404'd before the handler runs.
    assert_eq!(
        call_owned(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/archive"),
            Some("ws_b")
        )
        .await,
        StatusCode::NOT_FOUND
    );
    // The owner's write is admitted (200).
    assert_eq!(
        call_owned(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/archive"),
            Some("ws_a")
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_bare_session_stays_visible_to_bare_requests() {
    // Causes: the fixtures below establish `a bare session stays visible to bare requests` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `a bare session stays visible to bare requests` is one independent branch
    // selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    // A single-tenant deployment resolves no workspace; the session owns under the
    // seeded default scope and a bare request (also default) never 404s itself.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let id = create_owned(&app, None).await;
    assert_eq!(
        call_owned(&app, "GET", &format!("/v1/sessions/{id}"), None).await,
        StatusCode::OK
    );
    // But a scoped request cannot claim a default-owned session.
    assert_eq!(
        call_owned(&app, "GET", &format!("/v1/sessions/{id}"), Some("ws_a")).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_collection_route_is_never_fenced() {
    // Causes: the fixtures below establish `the collection route is` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `never fenced` and every asserted state transition or side
    // effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `the collection route is` is one independent branch selecting `never
    // fenced`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    // POST/GET /v1/sessions has no id → the guard passes it through regardless of scope.
    let app = router(Arc::new(ManagedState::new(EchoFake::default())));
    let _ = create_owned(&app, Some("ws_a")).await;
    assert_eq!(
        call_owned(&app, "GET", "/v1/sessions", Some("ws_b")).await,
        StatusCode::OK
    );
}

/// The event list is paged by cursor (`?page=<event_id>`?cursor=<event_id>&limit=<n>`limit=<n>`): the pages
/// walk the session's events oldest-first with no gap or overlap, `has_more` and
/// `next_page` bracket the walk, and a fabricated cursor is a 400.
#[tokio::test]
async fn events_are_paged_by_cursor() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    /* Event-list ordering decision table. Causes: C1 order is absent/asc or
     * desc; C2 a page cursor is absent or names the prior page's terminal
     * event; C3 every fixture event has the same processed_at; C4 order is an
     * unsupported value. Effects: E1 default pages remain chronological; E2
     * desc pages start at the newest committed event and walk without gaps or
     * overlap; E3 commit order is the deterministic tie-break for equal
     * timestamps; E4 invalid order is rejected. Rules: R1=C1(asc)+C2=>E1;
     * R2=C1(desc)+C2+C3=>E2+E3; R3=C4=>E4. */
    let state = Arc::new(ManagedState::new(EchoFake::default()));
    let app = router(state.clone());
    let id = create(&app).await;
    // Two Runs → 14 events (User/Session-running/Thread-running/message/
    // Thread-idle/usage/Session-idle × 2).
    for text in ["one", "two"] {
        json_call(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
        )
        .await;
        support::drive_retained_session_events(&state, &id).await;
    }
    let ids = |list: &serde_json::Value| -> Vec<String> {
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    };

    // Full, unpaged page: all events, no cursor.
    let full = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let full_ids = ids(&full);
    assert_eq!(full_ids.len(), 14, "two Runs produced fourteen events");
    assert!(full.get("has_more").is_none());
    assert_eq!(full["next_page"], serde_json::Value::Null);

    // First page of 2 → more remain, cursor names the 2nd event.
    let p1 = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?limit=2"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(ids(&p1), full_ids[0..2]);
    assert!(p1.get("has_more").is_none());
    assert_eq!(p1["next_page"], serde_json::json!(full_ids[1]));

    // RunResume after the cursor, to the end.
    let cursor = p1["next_page"].as_str().unwrap();
    let p2 = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?page={cursor}&limit=50"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(ids(&p2), full_ids[2..]);
    assert!(p2.get("has_more").is_none());
    assert_eq!(p2["next_page"], serde_json::Value::Null);

    // The two pages reassemble the whole list, in order, no overlap.
    let walked: Vec<String> = ids(&p1).into_iter().chain(ids(&p2)).collect();
    assert_eq!(walked, full_ids);

    let descending = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?order=desc&limit=2"),
        serde_json::Value::Null,
    )
    .await;
    let descending_ids = ids(&descending);
    assert_eq!(
        descending_ids,
        full_ids.iter().rev().take(2).cloned().collect::<Vec<_>>()
    );
    let descending_cursor = descending["next_page"].as_str().unwrap();
    let descending_tail = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?order=desc&page={descending_cursor}&limit=50"),
        serde_json::Value::Null,
    )
    .await;
    let descending_walked = descending_ids
        .into_iter()
        .chain(ids(&descending_tail))
        .collect::<Vec<_>>();
    assert_eq!(
        descending_walked,
        full_ids.iter().rev().cloned().collect::<Vec<_>>()
    );

    let invalid_order = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/sessions/{id}/events?order=newest"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_order.status(), StatusCode::BAD_REQUEST);

    // A fabricated cursor is a caller error (400).
    let bad = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/sessions/{id}/events?page=evt_nope"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}
