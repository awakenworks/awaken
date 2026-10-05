// @formal evidence session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal evidence session_event_batch.exact_retry_replays_and_changed_retry_conflicts
// @formal evidence session_event_batch.final_non_admission_excludes_late_acceptance
// @formal evidence session_event_batch.idempotent_receipt_is_admission_stable
// awaken-file-limit-ceiling: 7147 (legacy oversized test file: split, never grow)
//! Adapter integration tests with fake runtimes: the happy path, and the HITL
//! await -> `requires_action` -> `user.tool_confirmation` -> resume round-trip.

use awaken_session_execution_contract::{StreamingRunRuntime, ThreadReadRuntime};
use awaken_session_types::SessionId;
mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::awaiting;
use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{
    RunLifecycleEvent, RunLifecycleEventKind, encode_run_lifecycle_cursor,
};
use awaken_protocol_managed::{ManagedState, managed_session_id_from_idempotency};
use awaken_session_application_contract::{Pending, StepOutcome};
use awaken_session_configuration_contract::{AgentCapabilities, BuiltinTool, CustomTool};
use awaken_session_error_contract::RunError;
use awaken_session_repository_contract::SessionRepository;
use awaken_session_execution_contract::{
    AdmitSessionRun, CommittedOutcomeProjection, OutcomeDrive, OutcomeIteration, OutcomeReport,
    SessionRunReservation, SessionRuntime, ToolPermissionDecision,
};
use awaken_session_state_contract::SessionExecutionState;
use awaken_session_store::SqliteManagedSessionRepository;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// The Managed router as a real deployment mounts it — behind the trusted edge
/// stamp. Tests that need the unstamped surface call
/// [`awaken_protocol_managed::router`] directly.
fn router(state: Arc<ManagedState>) -> Router {
    support::edge_stamped(awaken_protocol_managed::router(state))
}

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    Box::pin(json_response(app, method, uri, body))
        .await
        .expect_for(StatusCode::OK, &format!("{method} {uri}"))
}

async fn json_response(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> support::Call<serde_json::Value> {
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
    support::Call::new(status, json)
}

async fn post_events_with_idempotency(
    app: &Router,
    uri: &str,
    key: &str,
    body: &serde_json::Value,
) -> support::Call<serde_json::Value> {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    support::Call::new(status, body)
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
) -> support::Call<serde_json::Value> {
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
    support::Call::new(status, body)
}

#[tokio::test]
async fn native_event_admission_close_preserves_standard_events_and_owner_isolation() {
    // Decision table: N1 close unseen K/F => durable Closed, late standard POST
    // fails; N2 standard POST accepted => native close reports Accepted without
    // another event; N3 same K/changed F=>409; N4 missing key=>400;
    // N5 foreign Workspace=>404 before any close. The native route is explicitly
    // mounted: no fields or paths are added to the standard Anthropic surface.
    let runtime = EchoFake::default();
    let repo = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let state = Arc::new(support::state(runtime.clone()).with_session_repo(repo.clone()));
    // The native admission route is merged before the edge stamp so BOTH
    // surfaces sit behind the one composition-root tenancy layer.
    let app = support::edge_stamped(
        awaken_protocol_managed::router(state.clone()).merge(
            awaken_protocol_awaken::session_event_admission_router(
                awaken_protocol_managed::close_event_admission,
            )
            .with_state(state.clone()),
        ),
    );
    let created = post_session_with_idempotency(&app, Some("admission-http"), "Admission")
        .await
        .expect_for(StatusCode::OK, "/E1");
    let id = created["id"].as_str().unwrap();
    let events = format!("/v1/sessions/{id}/events");
    let close = format!("/v1/awaken/sessions/{id}/event-admissions/close");
    let body = serde_json::json!({"events":[
        {"type":"user.message","content":[{"type":"text","text":"hello"}]},
        {"type":"user.message","content":[{"type":"text","text":"hello"}]}
    ]});
    let receipt = post_events_with_idempotency(&app, &close, "closed-key", &body)
        .await
        .expect_for(StatusCode::OK, "N1");
    assert_eq!(receipt["status"], "closed", "N1");
    assert_eq!(receipt["session_id"], id, "N1 exact identity");
    let late = post_events_with_idempotency(&app, &events, "closed-key", &body)
        .await
        .expect_for(StatusCode::BAD_REQUEST, "N1");
    assert!(
        late["error"]["message"]
            .as_str()
            .unwrap()
            .contains("permanently closed"),
        "N1"
    );
    let accepted = post_events_with_idempotency(&app, &events, "accepted-key", &body)
        .await
        .expect_for(StatusCode::OK, "N2");
    let resolved = post_events_with_idempotency(&app, &close, "accepted-key", &body)
        .await
        .expect_for(StatusCode::OK, "N2");
    assert_eq!(resolved["status"], "accepted", "N2");
    // N6 accepted is not executed: an observer needs the exact input event
    // identity before a later Idle can be attributed to this submission.
    // Never infer it from message text, current status or a client hash.
    let input_ids: Vec<_> = accepted["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].clone())
        .collect();
    assert_eq!(
        input_ids.len(),
        2,
        "N6 identical texts still have separate identities"
    );
    assert_ne!(input_ids[0], input_ids[1], "N6 never correlate by text");
    assert_eq!(
        resolved["event_ids"],
        serde_json::json!(input_ids),
        "N6 stable order"
    );
    assert_eq!(
        receipt["event_ids"],
        serde_json::json!([]),
        "N1 no executable events"
    );
    let replay = post_events_with_idempotency(&app, &events, "accepted-key", &body)
        .await
        .expect_for(StatusCode::OK, "I2 replay succeeds");
    assert_eq!(accepted, replay, "N2 original receipt unchanged");
    let changed = serde_json::json!({"events":[{"type":"user.message","content":[{"type":"text","text":"changed"}]}]});
    assert_eq!(
        post_events_with_idempotency(&app, &close, "closed-key", &changed)
            .await
            .status(),
        StatusCode::CONFLICT,
        "N3"
    );
    assert_eq!(
        json_response(&app, "POST", &close, body.clone())
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "N4"
    );
    let foreign = app
        .clone()
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "foreign-workspace".into(),
        )));
    assert_eq!(
        post_events_with_idempotency(&foreign, &close, "foreign-key", &body)
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "N5"
    );
    let stored = state
        .session_application()
        .session_event_batch_idempotency(&SessionId::from(id), "foreign-key", "any")
        .await
        .unwrap();
    assert_eq!(
        stored,
        awaken_session_application::SessionEventBatchIdempotency::Absent,
        "N5 no mutation"
    );
    // D1 delete after accepted/closed => preserve exact original receipts;
    // D2 unseen identity=>Closed with deletion proof, late POST cannot execute;
    // D3 changed body=>409; D4 foreign owner=>404, including after cold restart.
    // Actual cleanup and store CAS run here; no fabricated tombstone fixture.
    runtime.drive(&state, id).await;
    state.delete_session(id).await.unwrap();
    state
        .session_application()
        .release_terminal_resources("default", id)
        .await
        .unwrap();
    assert!(matches!(
        repo.get(&SessionId::from(id)).await,
        Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
    ));
    let restarted = Arc::new(support::state(EchoFake::default()).with_session_repo(repo));
    let cold = support::edge_stamped(
        awaken_protocol_managed::router(restarted.clone()).merge(
            awaken_protocol_awaken::session_event_admission_router(
                awaken_protocol_managed::close_event_admission,
            )
            .with_state(restarted),
        ),
    );
    for (key, expected) in [("closed-key", receipt), ("accepted-key", resolved)] {
        let retained = post_events_with_idempotency(&cold, &close, key, &body)
            .await
            .expect_for(StatusCode::OK, "D1");
        assert_eq!(
            retained, expected,
            "D1 immutable proof survives restart and deletion"
        );
        assert_eq!(
            post_events_with_idempotency(&cold, &close, key, &changed)
                .await
                .status(),
            StatusCode::CONFLICT,
            "D3"
        );
    }
    let deleted = post_events_with_idempotency(&cold, &close, "unseen", &body)
        .await
        .expect_for(StatusCode::OK, "D2");
    assert_eq!(deleted["status"], "closed");
    assert_eq!(deleted["proof"], "session_deleted");
    assert!(deleted["deleted_revision"].as_u64().unwrap() > 0);
    assert!(
        deleted.get("batch_id").is_none(),
        "no imaginary Event batch"
    );
    let late_body = post_events_with_idempotency(&cold, &events, "unseen", &body)
        .await
        .expect_for(
            StatusCode::BAD_REQUEST,
            "ordinary admission retains its existing missing-root error",
        );
    assert_eq!(late_body["error"]["message"], "Session was not found");
    let foreign_cold = cold.layer(axum::Extension(awaken_tenancy::WorkspaceScope(
        "foreign-workspace".into(),
    )));
    assert_eq!(
        post_events_with_idempotency(&foreign_cold, &close, "accepted-key", &body)
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "D4"
    );
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
// Test design: session_create_idempotency_replays_one_canonical_session
// Cause/effect graph: the owner-scoped key and payload select one canonical Session identity.
// Decision table: same key+payload=replay; changed payload=409; invalid key=400; different owner=independent.
#[tokio::test]
async fn session_create_idempotency_replays_one_canonical_session() {
    let app = router(Arc::new(support::state(EchoFake::default())));
    let first = post_session_with_idempotency(&app, Some("design-project-a"), "Project A")
        .await
        .expect_for(StatusCode::OK, "I2 first create succeeds");
    let replay = post_session_with_idempotency(&app, Some("design-project-a"), "Project A")
        .await
        .expect_for(StatusCode::OK, "I2 replay succeeds");
    assert_eq!(first["id"], replay["id"], "I2 identity is stable");

    let _mismatch = post_session_with_idempotency(&app, Some("design-project-a"), "Changed")
        .await
        .expect_for(StatusCode::CONFLICT, "I3 changed payload conflicts");

    let independent_a = post_session_with_idempotency(&app, None, "ordinary")
        .await
        .expect(StatusCode::OK);
    let independent_b = post_session_with_idempotency(&app, None, "ordinary")
        .await
        .expect(StatusCode::OK);
    assert_ne!(
        independent_a["id"], independent_b["id"],
        "I1 preserves ordinary create"
    );

    let _invalid = post_session_with_idempotency(&app, Some(""), "invalid")
        .await
        .expect_for(StatusCode::BAD_REQUEST, "I4 rejects an empty key");
    let overlong = "x".repeat(256);
    let _invalid = post_session_with_idempotency(&app, Some(&overlong), "invalid")
        .await
        .expect_for(StatusCode::BAD_REQUEST, "I4 rejects an overlong key");
    async fn post_initial(app: &Router) -> support::Call<serde_json::Value> {
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
        support::Call::new(status, serde_json::from_slice(&bytes).unwrap())
    }
    let initial = post_initial(&app)
        .await
        .expect_for(StatusCode::OK, "I6 first create");
    let initial_replay = post_initial(&app)
        .await
        .expect_for(StatusCode::OK, "I6 replay");
    assert_eq!(initial["id"], initial_replay["id"], "I6 identity");
    assert_eq!(initial["status"], "running", "I6 durable batch active");

    let sessions = json_call(&app, "GET", "/v1/sessions", serde_json::Value::Null).await;
    assert_eq!(
        sessions["data"].as_array().unwrap().len(),
        4,
        "I2/I3/I4/I6 add no duplicate"
    );

    let state = Arc::new(support::state(EchoFake::default()));
    let request = || {
        serde_json::from_value(session_request(serde_json::json!({ "agent": "coder" }))).unwrap()
    };
    let owner_a = state
        .create_session_idempotent(request(), "owner-a", "shared-key")
        .await
        .unwrap();
    let owner_b = state
        .create_session_idempotent(request(), "owner-b", "shared-key")
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
async fn prefer_header_cannot_select_a_second_standard_create_semantics() {
    // Cause/effect graph: C1 Prefer respond-async is absent/present; C2 each
    // request admits a new owner-scoped Session. Effects: E1 both requests run
    // the one synchronous official create path and return 200; E2 both return a
    // fully realized, exact current-SDK Session; E3 GET observes each same root.
    // Decision rules: A1 !C1+C2=>E1+E2+E3; A2 C1+C2=>E1+E2+E3.
    // Constraint: product async admission remains under `/v1/awaken`; a generic
    // HTTP preference cannot create an undocumented Managed response contract.
    let app = router(Arc::new(support::state(EchoFake::default())));
    let post = |prefer: bool, key: &'static str| {
        let app = app.clone();
        async move {
            let mut request = Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .header("idempotency-key", key);
            if prefer {
                request = request.header("prefer", "respond-async");
            }
            let response = app
                .clone()
                .oneshot(
                    request
                        .body(Body::from(
                            serde_json::to_vec(&session_request(serde_json::json!({
                                "agent": "coder",
                                "title": "recoverable"
                            })))
                            .unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            support::Call::new(
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    };

    for (rule, created) in [
        (
            "A1",
            post(false, "official-create-without-prefer")
                .await
                .expect(StatusCode::OK),
        ),
        (
            "A2",
            post(true, "official-create-with-prefer")
                .await
                .expect(StatusCode::OK),
        ),
    ] {
        assert_eq!(created["status"], "idle", "{rule}/E2 fully realized");
        support::assert_current_sdk_session_shape(&created);

        let retrieved = json_call(
            &app,
            "GET",
            &format!("/v1/sessions/{}", created["id"].as_str().unwrap()),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(retrieved["id"], created["id"], "{rule}/E3");
        assert_eq!(retrieved["status"], "idle", "{rule}/E3");
        support::assert_current_sdk_session_shape(&retrieved);
    }
}

#[tokio::test]
async fn session_routes_emit_only_the_current_official_sdk_recursive_shape() {
    // Cause/effect graph: C1 the current SDK declares required and optional
    // Session and nested SessionAgent properties; C2 create/retrieve/list
    // serialize the same adapter DTO; C3 an internal lifecycle/property is
    // present or a required official property is missing at either level.
    // Effects: E1 every route emits all required fields and only current-SDK
    // fields recursively; E2 C3 fails this offline gate. Decision rules:
    // S1=C1+C2+!C3=>E1; S2=C3=>E2. The SDK oracle, not this test, owns the
    // property catalog; the default agent's absent coordinator covers the
    // required-nullable `multiagent` serialization rule.
    let app = router(Arc::new(support::state(EchoFake::default())));
    let created = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;
    support::assert_current_sdk_session_shape(&created);

    let retrieved = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{}", created["id"].as_str().unwrap()),
        serde_json::Value::Null,
    )
    .await;
    support::assert_current_sdk_session_shape(&retrieved);

    let listed = json_call(&app, "GET", "/v1/sessions", serde_json::Value::Null).await;
    support::assert_current_sdk_session_shape(&listed["data"][0]);

    let mut missing_required_nested = created;
    missing_required_nested["agent"]
        .as_object_mut()
        .expect("Session Agent is an object")
        .remove("multiagent");
    assert!(
        std::panic::catch_unwind(|| {
            support::assert_current_sdk_session_shape(&missing_required_nested)
        })
        .is_err(),
        "S2/E2 recursively rejects an omitted required-nullable Agent field"
    );
}

#[tokio::test]
async fn failed_idempotent_create_is_409_while_exact_get_exposes_failure() {
    // HTTP cause/effect decision table. C1 the deterministic create receipt is
    // durable; C2 owner and request fingerprint match; C3 execution is
    // ActivationFailed; C4 the process cache is cold. Effects: E1 replay is 409
    // `invalid_request_error` with the stable machine-readable message; E2 exact
    // GET returns the same official Session projection as terminated while the
    // durable root retains the adapter-private realization error; E3 neither
    // request creates, retries, replaces, or mutates the failed Session; E4 the
    // sole committed projector emits exactly one deterministic `session.error`
    // (`unknown_error`/`terminal`) before the Thread and Session terminated
    // edges, and warm refresh plus cold restart reproduce the same ids without
    // duplicates.
    //
    // | Rule | C1 | C2 | C3 | C4 | POST replay | exact GET | Side effect |
    // |---|---|---|---|---|---|---|---|
    // | F1 | yes | yes | yes | yes | E1 | 200 failed (E2) | E3+E4 |
    //
    // Live and mismatch partitions are owned by the complete idempotency table
    // above. The same router and repository paths are used; this test adds no
    // recovery or failure-only implementation.
    let repo = Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open Session repository"),
    );
    let original = Arc::new(support::state(EchoFake::default()).with_session_repo(repo.clone()));
    let app = router(original.clone());
    let created = post_session_with_idempotency(&app, Some("failed-create"), "Failed create")
        .await
        .expect_for(StatusCode::OK, "F1 fixture create");
    let id = created["id"].as_str().unwrap().to_string();
    let mut failed = repo
        .get(&SessionId::from(&id))
        .await
        .expect("durable Session");
    failed.execution = SessionExecutionState::ActivationFailed;
    failed.realization_progress.last_error = Some("repository realization timed out".into());
    let failed = support::replace_session_fixture(
        repo.as_ref(),
        "default",
        failed,
        "test:activation-failed",
    )
    .await;
    drop(app);
    drop(original);

    let restarted = Arc::new(support::state(EchoFake::default()).with_session_repo(repo.clone()));
    let app = router(restarted.clone());
    let replay = post_session_with_idempotency(&app, Some("failed-create"), "Failed create")
        .await
        .expect_for(StatusCode::CONFLICT, "F1/E1");
    assert_eq!(replay["type"], "error", "F1/E1");
    assert_eq!(replay["error"]["type"], "invalid_request_error", "F1/E1");
    assert_eq!(
        replay["error"]["message"], "session_create_terminal_conflict",
        "F1/E1 stable machine-readable message"
    );
    assert!(restarted.list_sessions().is_empty(), "F1/E3 no rehydration");

    let body = raw_call(&app, "GET", &format!("/v1/sessions/{id}"), Body::empty())
        .await
        .expect_for(StatusCode::OK, "F1/E2");
    assert_eq!(body["id"], id, "F1/E2 stable identity");
    assert_eq!(body["status"], "terminated", "F1/E2");
    support::assert_current_sdk_session_shape(&body);
    assert_eq!(
        failed.realization_progress.last_error.as_deref(),
        Some("repository realization timed out"),
        "F1/E2 durable cause remains internal"
    );
    let first_events = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&first_events),
        vec![
            "session.error",
            "session.thread_status_terminated",
            "session.status_terminated",
        ],
        "F1/E4 failure precedes the complete terminal projection"
    );
    let error = &first_events["data"][0]["error"];
    assert_eq!(error["type"], "unknown_error", "F1/E4");
    assert_eq!(
        error["message"], "repository realization timed out",
        "F1/E4"
    );
    assert_eq!(error["retry_status"]["type"], "terminal", "F1/E4");
    let warm_refresh = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        warm_refresh["data"], first_events["data"],
        "F1/E4 warm idempotency"
    );
    drop(app);
    drop(restarted);

    let cold = Arc::new(support::state(EchoFake::default()).with_session_repo(repo.clone()));
    let app = router(cold);
    let cold_events = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        cold_events["data"], first_events["data"],
        "F1/E4 cold idempotency"
    );
    assert_eq!(
        repo.get(&SessionId::from(id)).await.unwrap(),
        failed,
        "F1/E3 durable truth"
    );
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
    reservations: HashMap<String, AdmitSessionRun>,
    settlements: Vec<awaken_session_execution_contract::SessionAgentBoundaryCommand>,
    runs: HashMap<String, RunState>,
    messages: HashMap<String, Vec<Message>>,
    message_commit_cursors: HashMap<String, Vec<u64>>,
    lifecycle: HashMap<String, Vec<RunLifecycleEvent>>,
}

impl EchoFake {
    async fn drive(&self, state: &ManagedState, session_id: &str) {
        // Fixture boundary: the canonical supervisor drives admission/activation;
        // this fake substitutes only for the absent Host Dispatch observer.
        // Each pass must produce an exact activation receipt or stop. No sleep,
        // guessed epoch, transcript-derived settlement, or processed-bit write.
        loop {
            support::drive_retained_session_events(state, session_id).await;
            let boundaries = {
                let mut state = self.state.lock().unwrap();
                let (current, others) = std::mem::take(&mut state.settlements)
                    .into_iter()
                    .partition::<Vec<_>, _>(|boundary| boundary.session_id == session_id);
                state.settlements = others;
                current
            };
            if boundaries.is_empty() {
                break;
            }
            for boundary in boundaries {
                support::finalize_applied_user_dispatch(state, boundary).await;
            }
        }
    }

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
                commit_timestamps: Default::default(),
                thread_id,
                claimed_run_id: latest.run_id.clone(),
                runs,
                latest_run_id: Some(latest.run_id.clone()),
                messages,
                message_commit_cursors,
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: awaken_protocol_managed::test_support::lifecycle_audit_records(lifecycle),
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

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for EchoFake);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for EchoFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for EchoFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for EchoFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for EchoFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for EchoFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for EchoFake {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        let mut state = self.state.lock().unwrap();
        if state.runs.contains_key(&command.run_id.0) {
            return Ok(SessionRunReservation::Completed);
        }
        if state.reservations.contains_key(&command.run_id.0) {
            return Ok(SessionRunReservation::AlreadyReserved);
        }
        state.reservations.insert(command.run_id.0.clone(), command);
        Ok(SessionRunReservation::Reserved)
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        let mut state = self.state.lock().unwrap();
        if delivery.session_activity_epoch == 0 {
            return Err(RunError::bad_request(
                "fixture activation requires an activity",
            ));
        }
        if state.runs.contains_key(&delivery.run_id.0) {
            return Ok(awaken_session_execution_contract::SessionRunActivation::Completed);
        }
        let command = state
            .reservations
            .get(&delivery.run_id.0)
            .filter(|command| command.session_id == delivery.session_id)
            .ok_or_else(|| RunError::bad_request("fixture activation has no exact reservation"))?
            .clone();
        state.reservations.remove(&delivery.run_id.0);
        state.settlements.push(
            awaken_session_execution_contract::SessionAgentBoundaryCommand {
                session_id: delivery.session_id.clone(),
                source_thread_id: ThreadId(delivery.session_id.into_string()),
                source_run_id: delivery.run_id,
                source_agent_id: command.agent_id.clone(),
                session_activity_epoch: delivery.session_activity_epoch,
                cancellation_requested: false,
            },
        );
        state.runs.insert(
            command.run_id.0.clone(),
            RunState::Ended(EndCause::NaturalEnd),
        );
        let opening_commit_cursor = u64::try_from(
            state
                .lifecycle
                .get(command.session_id.as_str())
                .map_or(1, |lifecycle| lifecycle.len() + 1),
        )
        .unwrap();
        let terminal_commit_cursor = opening_commit_cursor + 1;

        let user_text = command
            .messages
            .iter()
            .find(|message| message.role == Role::User)
            .map(Message::text_content)
            .unwrap_or_default();
        let mut new_message_commit_cursors = Vec::new();
        {
            let transcript = state
                .messages
                .entry(command.session_id.to_string())
                .or_default();
            for message in command.messages {
                if !transcript.iter().any(|existing| existing.id == message.id) {
                    transcript.push(message);
                    new_message_commit_cursors.push(opening_commit_cursor);
                }
            }
            transcript.push(Message::text(
                Id(format!("{}/reply", command.run_id.0)),
                Role::Assistant,
                format!("echo: {user_text}"),
            ));
            new_message_commit_cursors.push(terminal_commit_cursor);
        }
        state
            .message_commit_cursors
            .entry(command.session_id.to_string())
            .or_default()
            .extend(new_message_commit_cursors);

        // The adapter's one warm/cold projector is intentionally driven by the
        // committed lifecycle feed, never by `session_run_state` or the
        // disposable transcript cache. Keep the fake's three query surfaces
        // causally consistent with the production Thread commit boundary.
        let lifecycle = state
            .lifecycle
            .entry(command.session_id.to_string())
            .or_default();
        lifecycle.push(RunLifecycleEvent {
            cursor: encode_run_lifecycle_cursor(opening_commit_cursor, 0).unwrap(),
            source_commit_cursor: opening_commit_cursor,
            thread_id: ThreadId(command.session_id.to_string()),
            run_id: RunId(command.run_id.0.clone()),
            kind: RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        });
        lifecycle.push(RunLifecycleEvent {
            cursor: encode_run_lifecycle_cursor(terminal_commit_cursor, 0).unwrap(),
            source_commit_cursor: terminal_commit_cursor,
            thread_id: ThreadId(command.session_id.into_string()),
            run_id: RunId(command.run_id.0),
            kind: RunLifecycleEventKind::Completed,
            state: RunState::Ended(EndCause::NaturalEnd),
            await_reason: None,
        });
        Ok(awaken_session_execution_contract::SessionRunActivation::Completed)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    async fn session_run_state(
        &self,
        _session_id: &SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<RunState>, RunError> {
        Ok(self.state.lock().unwrap().runs.get(&run_id.0).cloned())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_error_contract::RunError,
    > {
        self.streaming_runs()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "StreamingRunRuntime is unsupported by this runtime",
                )
            })?
            .interrupt(thread)
            .await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for EchoFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
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
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if session_id != thread_id {
            return Ok(None);
        }
        Ok(self.recovery_snapshot(thread_id))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for EchoFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Execute terminal Repository publication under the same realization
    /// generation that owns the cleanup command set and return either its
    /// secret-free receipt or one permanent compare-and-swap rejection. The
    /// caller verifies and durably records that outcome before ordinary root
    /// cleanup may dispose the shared environment. Runtimes must override this
    /// edge and carry the lease to the Repository transport boundary; falling
    /// back would erase the terminal ownership proof.
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<
        awaken_session_state_contract::SessionRepositoryPublicationEffect,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        awaken_protocol_managed::test_support::complete_terminal_cleanup_preparation(
            &effect,
            &authorization,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_preparation(effect)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        Ok(awaken_protocol_managed::test_support::complete_terminal_cleanup_disposal(&effect))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_completed_terminal_cleanup(session_id, lease)
            .await
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for EchoFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    Box::pin(initial_events_idle_case()).await;

    Box::pin(initial_events_message_case()).await;

    Box::pin(initial_events_outcome_case()).await;

    Box::pin(initial_events_rejected_cases()).await;
}

async fn initial_events_idle_case() {
    let idle_app = router(Arc::new(support::state(EchoFake::default())));
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
        let session = json_response(&idle_app, "POST", "/v1/sessions", body)
            .await
            .expect_for(StatusCode::OK, "expected status");
        assert_eq!(session["status"], "idle", "{rule}");
    }
}

async fn initial_events_message_case() {
    let runtime = EchoFake::default();
    let running_state = Arc::new(support::state(runtime.clone()));
    let running_app = router(running_state.clone());
    let session = json_response(
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
    .await
    .expect_for(StatusCode::OK, "C2 message admitted");
    assert_eq!(session["status"], "running", "C2 starts immediately");
    let id = session["id"].as_str().unwrap();
    runtime.drive(&running_state, id).await;
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
}

async fn initial_events_outcome_case() {
    let outcome_app = router(Arc::new(support::state(OutcomeFake::default())));
    let session = json_response(
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
    .await
    .expect_for(StatusCode::OK, "C2 single outcome admitted");
    assert_eq!(
        session["status"], "running",
        "C2 outcome starts immediately"
    );
}

async fn initial_events_rejected_cases() {
    let rejected_app = router(Arc::new(support::state(EchoFake::default())));
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
                "agent":"coder", "initial_events": vec![message.clone(); 51]
            })),
        ),
        (
            "C6 two outcomes",
            session_request(serde_json::json!({
                "agent":"coder", "initial_events":[outcome.clone(), outcome]
            })),
        ),
        (
            "C6 missing rubric",
            session_request(serde_json::json!({"agent":"coder", "initial_events":[{
                "type":"user.define_outcome", "description":"finish"
            }]})),
        ),
    ];
    for (_rule, body) in invalid_cases {
        let _ = json_response(&rejected_app, "POST", "/v1/sessions", body)
            .await
            .expect_for(StatusCode::BAD_REQUEST, "expected status");
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

// Test design: happy_path_projects_message_and_idle
// Cause/effect graph: accepted user.message -> one Run -> agent.message -> committed idle boundary.
// Decision table: idle+valid event=receipt/message/idle; invalid event or terminal Session=4xx without a Run.
#[tokio::test]
async fn happy_path_projects_message_and_idle() {
    // Causes: the fixtures below establish `happy path` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = EchoFake::default();
    let repo = Arc::new(SqliteManagedSessionRepository::open(":memory:").unwrap());
    let state = Arc::new(support::state(runtime.clone()).with_session_repo(repo.clone()));
    let app = router(state.clone());
    let id = create(&app).await;
    // Send-response cause/effect rules: C1 one valid User Event is atomically
    // retained; C2 the opportunistic reconciler may or may not finish before the
    // HTTP response. Effects: W1 returns the complete inbound DTO and stable id;
    // W2 keeps the admission receipt's `processed_at` null even if the
    // opportunistic drive wins, while eventual history contains the same id
    // marked processed; W3 the committed root Run
    // emits Running/Idle using the public primary Thread id. Decision rules:
    // R1=C1+not-C2 -> W1+W2; R2=C1+C2 -> W1+W2; both -> W3. This keeps an
    // idempotent response independent of the processing race.
    let sent = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    // Settlement decision table: S1 terminal Thread + no Dispatch callback =>
    // unprocessed input and retained activity; S2 exact Applied callback =>
    // processed input, one Running/output/Idle bracket; S3 callback/read replay
    // and cold rehydration => identical ids/order, no duplicate bracket. This
    // fast Run has separate opening/terminal commits but no Running-phase
    // input observation, reproducing the terminal-anchor misclassification.
    support::drive_retained_session_events(&state, &id).await;
    let application = state.session_application();
    let unsettled = application.session(&SessionId::from(&id)).await.unwrap();
    assert!(
        !unsettled.active_activity_epochs.is_empty(),
        "S1 retains activity"
    );
    assert!(
        unsettled
            .event_batches
            .iter()
            .flat_map(|batch| &batch.events)
            .all(|entry| !entry.processed && entry.dispatch_settlement.is_none()),
        "S1"
    );
    let pending = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert!(pending["data"][0]["processed_at"].is_null(), "S1");
    runtime.drive(&state, &id).await;
    assert_eq!(sent["data"][0]["type"], "user.message", "R1/R2 W1");
    assert_eq!(
        sent["data"][0]["content"],
        serde_json::json!([{ "type": "text", "text": "hi" }]),
        "R1/R2 W1"
    );
    assert!(sent["data"][0]["id"].is_string(), "R1/R2 W1");
    assert!(sent["data"][0]["processed_at"].is_null(), "R1/R2 W2");
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
    let settled = application.session(&SessionId::from(&id)).await.unwrap();
    assert!(settled.active_activity_epochs.is_empty(), "S2");
    assert_eq!(settled.closed_runtime_intervals.len(), 1, "S2 one interval");
    let cold = router(Arc::new(
        support::state(runtime.clone()).with_session_repo(repo),
    ));
    for reader in [&app, &cold, &cold] {
        let replay = json_call(
            reader,
            "GET",
            &format!("/v1/sessions/{id}/events"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(replay["data"], list["data"], "S3 warm/cold exact history");
    }
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
    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for CapableFake);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for CapableFake);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for CapableFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for CapableFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for CapableFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for CapableFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for CapableFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for CapableFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &SessionId,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(None)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for CapableFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for CapableFake {
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The capability surface visible to one prepared session. Runtimes whose
    /// catalogs are workspace-scoped override this; simple runtimes inherit the
    /// process-wide view for backwards compatibility.
    fn capabilities_for(
        &self,
        _thread: &str,
    ) -> awaken_session_configuration_contract::AgentCapabilities {
        self.capabilities()
    }
}

/// A created session advertises the runtime's surface on its agent object: the
/// built-in toolset, a custom tool, skills, and a multiagent roster — not an empty set.
#[tokio::test]
async fn create_session_advertises_capabilities() {
    let app = router(Arc::new(support::state(CapableFake)));
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
/// `capabilities` advertises no tools, skills, or resources, and emits the
/// SDK-required nullable `multiagent` property as `null`.
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    // Wire-shape partition: ordinary synchronous creation exposes only the
    // official Session DTO. Internal realization state is lifecycle evidence,
    // not an extra public response field.
    // This exact key set is the official SDK's required Session response plus
    // no product extension; adding an unconditional field cannot hide behind
    // the SDK's permissive JSON decoder.
    assert_eq!(
        s.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "agent",
            "archived_at",
            "budget",
            "created_at",
            "environment_id",
            "id",
            "metadata",
            "outcome_evaluations",
            "resources",
            "stats",
            "status",
            "title",
            "type",
            "updated_at",
            "usage",
            "vault_ids",
        ]
        .into_iter()
        .collect(),
        "ready Session exposes only the official top-level wire shape",
    );
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
    let app = router(Arc::new(support::state(CapableFake)));
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

#[derive(Clone)]
struct ToolAwaitingFake {
    scenario: AdapterToolScenario,
    run: Arc<Mutex<Option<AdapterToolRun>>>,
    settlement: Arc<Mutex<Option<awaken_session_execution_contract::SessionAgentBoundaryCommand>>>,
}

impl ToolAwaitingFake {
    fn permission() -> Self {
        Self {
            scenario: AdapterToolScenario::Permission,
            run: Arc::new(Mutex::new(None)),
            settlement: Arc::new(Mutex::new(None)),
        }
    }

    fn custom() -> Self {
        Self {
            scenario: AdapterToolScenario::Custom,
            run: Arc::new(Mutex::new(None)),
            settlement: Arc::new(Mutex::new(None)),
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

    fn await_reason(&self) -> awaiting::AwaitReason {
        match self.scenario {
            AdapterToolScenario::Permission => awaiting::AwaitReason::ToolPermission,
            AdapterToolScenario::Custom => awaiting::AwaitReason::ExternalEvent,
        }
    }

    fn await_target(&self, pending: Pending) -> awaiting::AwaitTarget {
        awaken_protocol_managed::test_support::tool_call_target(
            pending,
            match self.scenario {
                AdapterToolScenario::Permission => awaiting::ToolAwaitReason::Permission,
                AdapterToolScenario::Custom => awaiting::ToolAwaitReason::ClientExecution,
            },
        )
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
                    ticket: awaiting::ResumeTicket::new(
                        format!("adapter-awaiting-ticket:{}", pending.tool_use_id),
                        run.run_id.clone(),
                        thread_id.clone(),
                        "adapter-snapshot",
                        "adapter-catalog",
                        self.await_target(pending),
                    ),
                },
            )
            .into_iter()
            .collect();
        Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                commit_timestamps: Default::default(),
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
                // This fake only ever pauses on its scenario's one tool.
                events: awaken_protocol_managed::test_support::lifecycle_audit_pausing_on(
                    &run.lifecycle,
                    &self.await_target(self.pending()),
                ),
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

    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for ToolAwaitingFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for ToolAwaitingFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for ToolAwaitingFake);

awaken_session_execution_contract::neutral_role!(OutcomeRuntime for ToolAwaitingFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for ToolAwaitingFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for ToolAwaitingFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for ToolAwaitingFake {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        let mut slot = self.run.lock().unwrap();
        if let Some(existing) = slot.as_ref() {
            return if existing.run_id == command.run_id {
                Ok(SessionRunReservation::Completed)
            } else {
                Err(RunError::bad_request(
                    "adapter fixture already owns a different Run",
                ))
            };
        }
        let pending = self.pending();
        let mut messages = command.messages;
        messages.push(awaken_agent_contract::agent::message::Message {
            id: Id(format!("{}/tool-use", command.run_id.0)),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: pending.tool_use_id.clone(),
                name: pending.name.clone(),
                input: pending.input.clone(),
            }],
        });
        let opening_commit_cursor = 1;
        let awaiting_commit_cursor = 2;
        let lifecycle = vec![
            RunLifecycleEvent {
                cursor: encode_run_lifecycle_cursor(opening_commit_cursor, 0).unwrap(),
                source_commit_cursor: opening_commit_cursor,
                thread_id: ThreadId(command.session_id.to_string()),
                run_id: command.run_id.clone(),
                kind: RunLifecycleEventKind::Running,
                state: RunState::Running,
                await_reason: None,
            },
            RunLifecycleEvent {
                cursor: encode_run_lifecycle_cursor(awaiting_commit_cursor, 0).unwrap(),
                source_commit_cursor: awaiting_commit_cursor,
                thread_id: ThreadId(command.session_id.to_string()),
                run_id: command.run_id.clone(),
                kind: RunLifecycleEventKind::Awaiting,
                state: RunState::Awaiting,
                await_reason: Some(self.await_reason()),
            },
        ];
        *slot = Some(AdapterToolRun {
            session_id: command.session_id.into_string(),
            run_id: command.run_id,
            state: RunState::Awaiting,
            message_commit_cursors: vec![opening_commit_cursor, awaiting_commit_cursor],
            messages,
            lifecycle,
            pending: Some(pending),
        });
        Ok(SessionRunReservation::Completed)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivation,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    async fn session_run_state(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        Ok(self
            .run
            .lock()
            .unwrap()
            .as_ref()
            .filter(|run| run.session_id == session_id.as_str() && run.run_id == *run_id)
            .map(|run| run.state.clone()))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_execution_contract::SessionThreadToolReplyFence, RunError> {
        let run = self.run.lock().unwrap();
        let run = run
            .as_ref()
            .ok_or_else(|| RunError::bad_request("adapter fixture has no pending Run"))?;
        let pending = run
            .pending
            .as_ref()
            .ok_or_else(|| RunError::bad_request("adapter fixture Run is not Awaiting"))?;
        if command.target != awaken_session_execution_contract::SessionThreadTarget::Primary
            || command.session_id != run.session_id
            || command
                .tool_request_event_id
                .as_deref()
                .is_none_or(str::is_empty)
            || command.expected_thread_version != Some(u64::try_from(run.lifecycle.len()).unwrap())
            || command.expected_run_id != run.run_id
            || command.expected_correlation_id
                != format!("adapter-awaiting-ticket:{}", pending.tool_use_id)
            || command.tool_use_id != pending.tool_use_id
        {
            return Err(RunError::bad_request(
                "adapter fixture reply does not match the committed ticket",
            ));
        }
        Ok(
            awaken_session_execution_contract::SessionThreadToolReplyFence {
                // Session creation owns epoch 1; the completed Awaiting boundary
                // settled it but the durable dispatch fence still identifies that
                // prior epoch when the reply transfers activity to the same Run.
                activity:
                    awaken_session_execution_contract::SessionThreadToolReplyActivity::Session {
                        prior_epoch: Some(1),
                    },
                already_applied: false,
            },
        )
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        let expected = self
            .session_thread_tool_reply_fence(&delivery.command)
            .await?;
        if delivery.fence != expected {
            return Err(RunError::bad_request("adapter fixture reply fence changed"));
        }
        *self.settlement.lock().unwrap() = Some(
            awaken_session_execution_contract::SessionAgentBoundaryCommand {
                session_id: delivery.command.session_id.clone(),
                source_thread_id: delivery
                    .command
                    .target
                    .thread_id(&delivery.command.session_id),
                source_run_id: delivery.command.expected_run_id.clone(),
                source_agent_id: String::new(),
                session_activity_epoch: delivery
                    .session_activity_epoch
                    .expect("adapter models a Session-owned Run"),
                cancellation_requested: false,
            },
        );
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
                awaken_session_execution_contract::SessionThreadToolReply::Confirm(
                    ToolPermissionDecision::Allow { .. },
                ),
            ) => (vec![ContentBlock::text("wrote x.txt")], "done".to_string()),
            (
                AdapterToolScenario::Custom,
                awaken_session_execution_contract::SessionThreadToolReply::Custom {
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_error_contract::RunError,
    > {
        self.interrupt(thread).await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for ToolAwaitingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`awaken_session_application_contract::StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run_streaming(agent, thread, content, sink).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, thread: &str) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(
        &self,
        thread: &str,
        model: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .rebind_model(thread, model)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        awaken_session_execution_contract::NeutralRuntime
            .supports_mid_conversation_system(thread)
            .await
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
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for ToolAwaitingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
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

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(self.recovery_snapshot(session_id.as_str(), thread_id))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for ToolAwaitingFake {
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The capability surface visible to one prepared session. Runtimes whose
    /// catalogs are workspace-scoped override this; simple runtimes inherit the
    /// process-wide view for backwards compatibility.
    fn capabilities_for(
        &self,
        _thread: &str,
    ) -> awaken_session_configuration_contract::AgentCapabilities {
        self.capabilities()
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

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for OutcomeFake);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for OutcomeFake);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for OutcomeFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for OutcomeFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for OutcomeFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for OutcomeFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for OutcomeFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for OutcomeFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for OutcomeFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &SessionId,
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
        reports.sort_by_key(|(left, _)| *left);
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
            let terminal_cursor = source_commit_cursor
                .saturating_add(u64::try_from(report.iterations.len()).unwrap());
            state.push(awaken_agent_contract::agent::state::Command::set(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                format!("outcome/{outcome_id}/state"),
                serde_json::json!({"fixture": "terminal"}),
            ));
            state_commit_cursors.push(terminal_cursor);
            state.push(awaken_agent_contract::agent::state::Command::remove(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                "outcome/active",
            ));
            state_commit_cursors.push(terminal_cursor);
        }
        let store_cursor = state_commit_cursors.iter().copied().max().unwrap_or(0);
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                commit_timestamps: Default::default(),
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
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
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for OutcomeFake {
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
                            // Round 1 was graded by a backend that reports
                            // counters.
                            usage: Some(awaken_agent_contract::audit::model_request::TokenUsage {
                                prompt_tokens: 210,
                                completion_tokens: 14,
                                cache_read_tokens: 8,
                                cache_creation_tokens: 0,
                            }),
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
                            // Round 2 recorded no usage at all; the wire field
                            // is required, so it reports zeroes.
                            usage: None,
                        },
                    ],
                },
            ),
        );
        Ok(source_commit_cursor)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Convenience composition over the canonical prepare/continue phases.
    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<
        awaken_session_execution_contract::OutcomeDrive,
        awaken_session_error_contract::RunError,
    > {
        let outcome_id = awaken_session_event_contract::session_outcome_convenience_id(
            thread,
            description,
            rubric,
            max_iterations,
        );
        let _ = self
            .prepare_outcome(thread, &outcome_id, description, rubric, max_iterations)
            .await?;
        self.continue_outcome(thread)
            .await?
            .ok_or_else(|| RunError::internal("prepared Outcome is not active"))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
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
    let state = Arc::new(support::state(OutcomeFake {
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
        let boundary_app = router(Arc::new(support::state(OutcomeFake::default())));
        let boundary_id = create(&boundary_app).await;
        let _ = json_response(
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
        .await
        .expect_for(expected, "H8 max_iterations=");
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

    let state = Arc::new(support::state(OutcomeFake::default()));
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
    // Usage cause/effect: C6 the committed round recorded the Judge Run's token
    // counters; C7 it recorded none. The SDK declares `usage` required, so rule
    // U1=C6 -> the recorded counters reach the wire unchanged under the SDK's
    // field names, and rule U2=C7 -> the required field is present and zeroed
    // rather than omitted or carried over from the neighbouring round.
    assert_eq!(ends[0]["usage"]["input_tokens"], 210, "U1");
    assert_eq!(ends[0]["usage"]["output_tokens"], 14, "U1");
    assert_eq!(ends[0]["usage"]["cache_read_input_tokens"], 8, "U1");
    assert_eq!(ends[0]["usage"]["cache_creation_input_tokens"], 0, "U1");
    assert!(
        ends[0]["usage"].get("speed").is_none(),
        "U1 no recorded observation carries an inference speed"
    );
    assert_eq!(ends[1]["usage"]["input_tokens"], 0, "U2");
    assert_eq!(ends[1]["usage"]["output_tokens"], 0, "U2");
    assert_eq!(ends[1]["usage"]["cache_read_input_tokens"], 0, "U2");
    assert_eq!(ends[1]["usage"]["cache_creation_input_tokens"], 0, "U2");
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
    let runtime = ToolAwaitingFake::permission();
    let state = Arc::new(support::state(runtime.clone()));
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
    for (_rule, events) in [
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
        let _ = json_response(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({"events": events}),
        )
        .await
        .expect_for(StatusCode::BAD_REQUEST, "expected status");
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
    // This narrow adapter fixture intentionally has no Host settlement observer.
    // Thread completion is not aggregate settlement. Explicitly deliver the
    // committed boundary through the SAME Session application observer used by
    // the Host before requiring usage/Session-idle, never by sleeping or making
    // the read-side projector infer another activity authority.
    // Transport-retry extension of H6: C1 two HTTP callers race with one new
    // key/body while the exact tool ticket is pending => E1 append once and
    // return the same receipt to both; C2 the same
    // key/body arrives after the ticket was consumed => E2 replay that byte-wise
    // JSON receipt without re-admission or another reply; C3 same key/different
    // decision => E3 409 without mutation. These three rules cover response loss
    // at the HTTP/Runtime boundary while the application test owns restart/CAS.
    let confirmation_body = serde_json::json!({ "events": [
        { "type": "user.tool_confirmation", "tool_use_id": public_tool_id, "result": "allow" }
    ] });
    let events_path = format!("/v1/sessions/{id}/events");
    let (confirmation_a, confirmation_b) = tokio::join!(
        post_events_with_idempotency(
            &app,
            &events_path,
            "hitl-confirmation-1",
            &confirmation_body,
        ),
        post_events_with_idempotency(
            &app,
            &events_path,
            "hitl-confirmation-1",
            &confirmation_body,
        ),
    );
    let confirmation = confirmation_a.expect_for(StatusCode::OK, "H6/E1");
    assert_eq!(
        confirmation_b.expect_for(StatusCode::OK, "H6/E1 concurrent retry"),
        confirmation,
        "H6/E1 one canonical receipt"
    );
    assert_eq!(confirmation["data"][0]["type"], "user.tool_confirmation");
    assert_eq!(confirmation["data"][0]["tool_use_id"], public_tool_id);
    assert_eq!(confirmation["data"][0]["result"], "allow");
    assert!(confirmation["data"][0].get("content").is_none());
    assert!(
        confirmation["data"][0]["processed_at"].is_null(),
        "H6/E1 receipt is the immutable admission view"
    );
    let application = state.session_application();
    let before_settlement = application.session(&SessionId::from(&id)).await.unwrap();
    let boundary = runtime
        .settlement
        .lock()
        .unwrap()
        .clone()
        .expect("H8 exact delivery fence");
    // H8: a committed resumed terminal Run + its exact activity epoch closes
    // one interval; duplicate observer delivery is a no-op. Assert root truth
    // as well as the HTTP suffix so a cache-only repair cannot pass this case.
    // Deliver the boundary exactly as the Host's observer does: `settle`, then
    // the post-Dispatch `finalize` that closes a primary Run still retained by
    // its user.message batch. Whether the racing confirmation callers left the
    // Session already idle or still Running at this point depends on their
    // interleaving; the two-step delivery settles both, so this test never
    // assumes the inline drive got there first. `finalize` reports the
    // committed boundary as not yet visible only while the lifecycle feed lags;
    // that is the one condition polled, bounded and named, never slept over.
    let finalize_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    for _ in 0..2 {
        awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
            application.as_ref(),
            boundary.clone(),
        )
        .await
        .unwrap();
        loop {
            match awaken_session_execution_contract::SessionAgentCoordination::finalize_session_agent_boundary(
                application.as_ref(),
                boundary.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error)
                    if error.kind == awaken_session_error_contract::RunErrorKind::Unavailable
                        && tokio::time::Instant::now() < finalize_deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(error) => panic!(
                    "timed out or failed waiting for the resumed Run's boundary to finalize: {error}"
                ),
            }
        }
    }
    let settled = application.session(&SessionId::from(&id)).await.unwrap();
    assert_eq!(settled.execution, SessionExecutionState::Idle, "H8");
    assert_eq!(
        settled.closed_runtime_intervals.len(),
        before_settlement.closed_runtime_intervals.len()
            + usize::from(before_settlement.running_interval.is_some()),
        "H8 no duplicate interval"
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
            "agent.tool_use",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle",
            "user.tool_confirmation",
            "session.status_running",
            "session.thread_status_running",
            "agent.tool_result",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
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

    let replay = post_events_with_idempotency(
        &app,
        &format!("/v1/sessions/{id}/events"),
        "hitl-confirmation-1",
        &confirmation_body,
    )
    .await
    .expect_for(StatusCode::OK, "H6/E2");
    assert_eq!(replay, confirmation, "H6/E2 exact receipt");
    let _ = post_events_with_idempotency(
        &app,
        &format!("/v1/sessions/{id}/events"),
        "hitl-confirmation-1",
        &serde_json::json!({ "events": [
            { "type": "user.tool_confirmation", "tool_use_id": public_tool_id, "result": "deny" }
        ] }),
    )
    .await
    .expect_for(StatusCode::CONFLICT, "H6/E3");
    let after_retry = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&after_retry)
            .iter()
            .filter(|kind| kind.as_str() == "user.tool_confirmation")
            .count(),
        1,
        "H6/E2+E3 no duplicate reply"
    );
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
    // text/image blocks -> preserve ordered blocks in the one admitted
    // user.custom_tool_result while Runtime consumes its neutral ToolResult;
    // R3 wrong id/kind (covered by batch validation) -> reject without a partial
    // event. R2 suppresses the duplicate agent.tool_result projection because the
    // client receipt already owns the public result. Its adapter effect is the
    // same Thread's terminal committed prefix; aggregate Session settlement
    // belongs to the Host observer and official SDK E2E, not this test fixture.
    let state = Arc::new(support::state(ToolAwaitingFake::custom()));
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
    let public_results: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "user.custom_tool_result")
        .collect();
    assert_eq!(public_results.len(), 1, "R2 has one public result");
    let public_result = public_results[0];
    assert_eq!(public_result["content"][0]["text"], "42");
    assert_eq!(
        public_result["content"][1]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(
        public_result["content"][1]["source"]["data"],
        "iVBORw0KGgo="
    );
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["type"] != "agent.tool_result"),
        "R2 suppresses the Runtime projection duplicate"
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for RecordingFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for RecordingFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for RecordingFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RecordingFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for RecordingFake {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        self.subjects
            .lock()
            .unwrap()
            .push(command.data_subject_id.clone());
        self.committed.reserve_session_run(command).await
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        self.committed.activate_session_run(delivery).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    async fn session_run_state(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        self.committed.session_run_state(session_id, run_id).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_error_contract::RunError,
    > {
        self.interrupt(thread).await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for RecordingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`awaken_session_application_contract::StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run_streaming(agent, thread, content, sink).await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupts.lock().unwrap().push(thread.to_string());
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(
        &self,
        thread: &str,
        model: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .rebind_model(thread, model)
            .await
    }

    async fn supports_mid_conversation_system(&self, _thread: &str) -> bool {
        self.supports_mid_conversation_system
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The tool currently blocking `thread`, if any. Admission uses this
    /// read-only projection to reject events that cannot legally precede the
    /// matching result; resume remains the sole mutating authority.
    async fn pending_tool(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_application_contract::Pending>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .pending_tool(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for RecordingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.committed.committed_messages(thread).await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.committed
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for RecordingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
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
    let runtime = EchoFake::default();
    let state = Arc::new(support::state(RecordingFake {
        interrupts: interrupts.clone(),
        subjects: subjects.clone(),
        committed: runtime.clone(),
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
    runtime.drive(&state, &id).await;
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
    let _ = json_response(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({"events": [
            {"type":"user.message", "content":[{"type":"text", "text":"boundary"}]},
            {"type":"system.message", "content":thousand}
        ]}),
    )
    .await
    .expect_for(StatusCode::OK, "H2 inclusive maximum");
    runtime.drive(&state, &id).await;
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
    for (_rule, events) in invalid_cases {
        let _ = json_response(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({"events": events}),
        )
        .await
        .expect_for(StatusCode::BAD_REQUEST, "expected status");
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

    let unsupported = router(Arc::new(support::state(RecordingFake {
        interrupts: Arc::new(Mutex::new(Vec::new())),
        subjects: Arc::new(Mutex::new(Vec::new())),
        committed: EchoFake::default(),
        supports_mid_conversation_system: false,
    })));
    let unsupported_id = create(&unsupported).await;
    let body = json_response(
        &unsupported,
        "POST",
        &format!("/v1/sessions/{unsupported_id}/events"),
        serde_json::json!({"events": [
            {"type":"user.message", "content":[{"type":"text", "text":"must not run"}]},
            {"type":"system.message", "content":[{"type":"text", "text":"x"}]}
        ]}),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "H4 unsupported model");
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
    let runtime = EchoFake::default();
    let state = Arc::new(support::state(RecordingFake {
        interrupts: Arc::new(Mutex::new(Vec::new())),
        subjects: subjects.clone(),
        committed: runtime.clone(),
        supports_mid_conversation_system: true,
    }));
    let app = router(state.clone());
    let id = create(&app).await;
    let _ = json_response(
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
    .await
    .expect_for(StatusCode::BAD_REQUEST, "R1");
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
    runtime.drive(&state, &id).await;
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
    runtime.drive(&state, &id).await;
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
    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for InterruptRedirectFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for InterruptRedirectFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for InterruptRedirectFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for InterruptRedirectFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for InterruptRedirectFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for InterruptRedirectFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for InterruptRedirectFake {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        let text = command
            .messages
            .iter()
            .find(|message| message.role == Role::User)
            .map(Message::text_content)
            .unwrap_or_default();
        self.order.lock().unwrap().push(format!("run:{text}"));
        self.committed.reserve_session_run(command).await
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        self.committed.activate_session_run(delivery).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    async fn session_run_state(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        self.committed.session_run_state(session_id, run_id).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_error_contract::RunError,
    > {
        self.interrupt(thread).await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for InterruptRedirectFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`awaken_session_application_contract::StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run_streaming(agent, thread, content, sink).await
    }

    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        self.order.lock().unwrap().push("interrupt".into());
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(
        &self,
        thread: &str,
        model: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .rebind_model(thread, model)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        awaken_session_execution_contract::NeutralRuntime
            .supports_mid_conversation_system(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The tool currently blocking `thread`, if any. Admission uses this
    /// read-only projection to reject events that cannot legally precede the
    /// matching result; resume remains the sole mutating authority.
    async fn pending_tool(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_application_contract::Pending>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .pending_tool(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for InterruptRedirectFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.committed.committed_messages(thread).await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.committed
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for InterruptRedirectFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
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
    let runtime = EchoFake::default();
    let state = Arc::new(support::state(InterruptRedirectFake {
        order: order.clone(),
        committed: runtime.clone(),
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
    runtime.drive(&state, &id).await;

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

// Test design: retrieve_session_and_sse_event_names
// Cause/effect graph: committed public events map to official SSE names; internal facts remain hidden.
// Decision table: known Session=ordered typed frames; unknown=404; unsupported selector=400.
#[tokio::test]
async fn retrieve_session_and_sse_event_names() {
    // Coverage rationale: `retrieve session and sse event names` is one independent branch
    // selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let runtime = EchoFake::default();
    let state = Arc::new(support::state(runtime.clone()));
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
    runtime.drive(&state, &id).await;
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
) -> support::Call<serde_json::Value> {
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
    support::Call::new(status, json)
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
    let app = router(Arc::new(support::state(EchoFake::default())));
    let body = raw_call(&app, "GET", "/v1/sessions/nope/events", Body::empty())
        .await
        .expect(StatusCode::NOT_FOUND);
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
    let app = router(Arc::new(support::state(EchoFake::default())));
    // Syntactically broken JSON on a managed route.
    let body = raw_call(&app, "POST", "/v1/sessions", Body::from("{ not json "))
        .await
        .expect(StatusCode::BAD_REQUEST);
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    let app = router(Arc::new(support::state(EchoFake::default())));
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
    let app = router(Arc::new(support::state(EchoFake::default())));
    let _ = create_owned(&app, Some("ws_a")).await;
    assert_eq!(
        call_owned(&app, "GET", "/v1/sessions", Some("ws_b")).await,
        StatusCode::OK
    );
}

/// The event list is paged by cursor (`?page=<event_id>`?cursor=<event_id>&limit=<n>`limit=<n>`): the pages
/// walk the session's events oldest-first with no gap or overlap, `has_more` and
/// `next_page` bracket the walk, and a fabricated cursor is a 400.
// Test design: events_are_paged_by_cursor
// Cause/effect graph: order/filter/cursor/limit partition one committed history into disjoint pages.
// Decision table: valid cursor=next page; reverse=inverted traversal; fabricated cursor=400.
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
    let runtime = EchoFake::default();
    let state = Arc::new(support::state(runtime.clone()));
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
        runtime.drive(&state, &id).await;
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
