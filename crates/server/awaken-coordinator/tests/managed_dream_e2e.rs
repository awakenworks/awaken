// @formal evidence session_run.reservation_never_executes
//! Cross-module Managed Agents overview -> Dream E2E.

mod support;

use awaken_protocol_managed::test_support::DurableFact;
use awaken_scenario_host::build_dream_router_and_host;
use awaken_service_lifecycle::run_composed_async_test;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use support::wait_for_session_events;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri).header(
        "anthropic-beta",
        "managed-agents-2026-04-01,dreaming-2026-04-21",
    );
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, value) = call(app, method, uri, body).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {value}");
    value
}

async fn terminal_dream(app: &Router, id: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..1_000 {
        let value = ok(app, "GET", &format!("/v1/dreams/{id}?beta=true"), None).await;
        if matches!(
            value["status"].as_str(),
            Some("completed" | "failed" | "canceled")
        ) {
            return value;
        }
        last = value;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let session_id = last["session_id"].as_str().unwrap();
    let session = call(app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    let events = call(
        app,
        "GET",
        &format!("/v1/sessions/{session_id}/events"),
        None,
    )
    .await;
    panic!(
        "Dream remained non-terminal: {last}; auxiliary Session: {session:?}; events: {events:?}"
    )
}

#[test]
fn agent_session_events_files_memory_and_dream_share_one_runtime_and_data_plane() {
    // Causes: the fixtures below establish `agent session events files memory and dream share one
    // runtime and data plane` with the concrete inputs, state, dependencies, and failure triggers
    // used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Managed-overview cause/effect graph:
    // C0 scope-less local scenario request -> E0 the startup edge stamps the
    // Host's default Workspace before every data and policy handler;
    // C1 Agent-referenced Session + user event -> E1 durable full event history;
    // the HTTP receipt is acceptance, so E1 is observed only after the one
    // lifecycle supervisor reaches committed aggregate Idle (bounded yields,
    // never a request-local executor or timing sleep);
    // C2 ordinary MemoryStore + selected Session -> E2 asynchronous Dream;
    // C3 frozen transcript export -> E3 JSONL bytes reach the auxiliary Session
    // through InlineBytes and no transcript File record is created, including
    // a tombstone hidden from the public Files list;
    // C4 consolidation execution -> E4 ordinary archived
    // Session + an independent output MemoryStore reached through a real write
    // tool call; C5
    // source/output separation -> E5 source stays unchanged;
    // C6 explicit `view=full` -> E6 list projections include Dream contents (the
    // official default `basic` projection intentionally omits them); C7 the Runtime
    // terminal lifecycle cursor is consumed after the Session archive CAS -> E7 the
    // delayed event projection cannot reverse `terminated` to `idle`; C8 the sole
    // lifecycle supervisor composes its complete recovery cycle on the repository's
    // production-equivalent composed-test runtime -> E8 the cycle reaches terminal
    // state without exhausting the explicitly provisioned worker stack.
    // Decision rule R1 covers the successful end-to-end combination of all nine
    // causes, including the late-feed ordering selected by the shared ephemeral
    // Runtime authority.
    // Route/unit suites own invalid, default-basic, and nonterminal alternatives.
    run_composed_async_test(|| async {
        let (app, host) = build_dream_router_and_host().await;

        let session = ok(
            &app,
            "POST",
            "/v1/sessions",
            Some(json!({
                "agent": "assistant",
                "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
            })),
        )
        .await;
        let session_id = session["id"].as_str().unwrap();
        let receipt = ok(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/events"),
            Some(json!({
                "events":[{
                    "type":"user.message",
                    "content":[{"type":"text","text":"Remember that Project Atlas uses Rust."}]
                }]
            })),
        )
        .await;
        let events = wait_for_session_events(
            &app,
            session_id,
            DurableFact::command_processed(&receipt),
            "the Session's aggregate idle boundary",
            |events| {
                events
                    .iter()
                    .any(|event| event["type"] == "session.status_idle")
            },
        )
        .await;
        let event_types = events["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect::<Vec<_>>();
        for expected in [
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "session.thread_status_idle",
            "session.status_idle",
        ] {
            assert!(
                event_types.contains(&expected),
                "C1/E1 missing {expected}: {events}"
            );
        }

        let store = ok(
            &app,
            "POST",
            "/v1/memory_stores",
            Some(json!({"name":"Project memory"})),
        )
        .await;
        let store_id = store["id"].as_str().unwrap();
        let default_policy = ok(
            &app,
            "GET",
            &format!("/v1/awaken/memory-stores/{store_id}/dream-policy"),
            None,
        )
        .await;
        assert_eq!(default_policy["type"], "dream_policy");
        assert_eq!(default_policy["enabled"], false);
        let configured_policy = ok(
            &app,
            "PUT",
            &format!("/v1/awaken/memory-stores/{store_id}/dream-policy"),
            Some(json!({
                "enabled": false,
                "interval_seconds": 3600,
                "min_new_sessions": 1,
                "max_sessions": 25,
                "model": {"id":"claude-sonnet-5","speed":"standard"},
                "instructions": "Retain verified project conventions."
            })),
        )
        .await;
        assert_eq!(configured_policy["max_sessions"], 25);
        assert!(configured_policy["next_due_at"].is_string());
        ok(
            &app,
            "POST",
            &format!("/v1/memory_stores/{store_id}/memories"),
            Some(json!({"path":"/MEMORY.md","content":"# Existing\n- Keep me.\n"})),
        )
        .await;

        let dream = ok(
            &app,
            "POST",
            "/v1/dreams?beta=true",
            Some(json!({
                "inputs":[
                    {"type":"memory_store","memory_store_id":store_id},
                    {"type":"sessions","session_ids":[session_id]}
                ],
                "model":"claude-sonnet-5",
                "instructions":"Retain verified project conventions."
            })),
        )
        .await;
        let terminal = terminal_dream(&app, dream["id"].as_str().unwrap()).await;
        assert_eq!(terminal["status"], "completed", "{terminal}");
        let output_id = terminal["outputs"][0]["memory_store_id"].as_str().unwrap();
        assert_ne!(output_id, store_id);

        let source = ok(
            &app,
            "GET",
            &format!("/v1/memory_stores/{store_id}/memories?view=full"),
            None,
        )
        .await;
        let output = ok(
            &app,
            "GET",
            &format!("/v1/memory_stores/{output_id}/memories?view=full"),
            None,
        )
        .await;
        let auxiliary_uri = format!(
            "/v1/sessions/{}/events",
            terminal["session_id"].as_str().unwrap()
        );
        let auxiliary_events = awaken_protocol_managed::test_support::wait_for_session_events(
            DurableFact::committed_event("agent.tool_use"),
            std::time::Duration::from_secs(10),
            "Dream auxiliary write",
            || async {
                let (status, events) = call(&app, "GET", &auxiliary_uri, None).await;
                (status.as_u16(), events)
            },
            |events| {
                events
                    .iter()
                    .any(|event| event["type"] == "agent.tool_use" && event["name"] == "write")
            },
        )
        .await;
        assert_eq!(source["data"][0]["content"], "# Existing\n- Keep me.\n");
        assert_eq!(
            output["data"][0]["content"],
            "# Dream\n- Consolidated by the Dream Agent.\n"
        );
        assert!(
            auxiliary_events["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| {
                    event["type"] == "agent.tool_use"
                        && event["name"] == "write"
                        && event["input"]["path"] == "/mnt/dream/output-memory/MEMORY.md"
                })
        );
        let write_result = auxiliary_events["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["type"] == "agent.tool_result")
            .expect("Dream write has a correlated tool result");
        assert_ne!(write_result["is_error"], true, "{write_result}");

        let auxiliary = ok(
            &app,
            "GET",
            &format!("/v1/sessions/{}", terminal["session_id"].as_str().unwrap()),
            None,
        )
        .await;
        assert_eq!(auxiliary["status"], "terminated");
        assert_eq!(auxiliary["metadata"]["awaken.session.origin"], "dream");

        let files = ok(&app, "GET", "/v1/files", None).await;
        let transcript_name = format!(
            "{}.jsonl",
            awaken_resource_contract::content_id(session_id.as_bytes())
        );
        let transcript = files["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| {
                file["filename"].as_str().is_some_and(|name| {
                    name == transcript_name || name == format!("{session_id}.jsonl")
                })
            })
            .cloned();
        assert!(
            transcript.is_none(),
            "Dream transcript inputs must not remain as workspace Files"
        );
        let all_records = host
            .file_catalog()
            .list_files_including_deleted("default", None)
            .await
            .expect("Dream File catalog remains readable");
        assert!(
            all_records
                .iter()
                .all(|file| file.filename != transcript_name
                    && file.filename != format!("{session_id}.jsonl")),
            "Dream must not create a duplicate transcript File, even if later deleted"
        );
    });
}
