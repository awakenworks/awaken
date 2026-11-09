//! Request-bound private views share native Session truth and hide other owners.

mod support;

use std::sync::Arc;

use awaken_protocol_managed::{AdmittedPrivateSessionRequest, ManagedState, router};
use awaken_protocol_managed_testkit::RehydrateFake;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use awaken_session_contract::{SessionPrivateBinding, SessionRepositoryReads};
use awaken_session_types::SessionId;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn binding(account: &str) -> SessionPrivateBinding {
    SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        account.into(),
        "ws-a".into(),
        1,
    )
    .unwrap()
}

fn admitted(base: &Arc<ManagedState>, binding: SessionPrivateBinding) -> Router {
    let workspace = binding.workspace_id().to_owned();
    router(base.clone())
        .layer(Extension(AdmittedPrivateSessionRequest::from_admitted(
            binding,
        )))
        .layer(Extension(WorkspaceScope(workspace)))
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Value,
    key: Option<&str>,
) -> support::Call<Value> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    let request = request
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    support::Call::new(
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn create(app: &Router, key: Option<&str>) -> String {
    let body = call(app, "POST", "/v1/sessions", json!({
        "agent": "coder", "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
    }), key).await.expect_for(StatusCode::OK, "native private create");
    body["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn private_session_interrupt_retains_the_native_original_event_command() {
    let repository = support::session_repository("private original interrupt").await;
    let base = support::shared_state_over(RehydrateFake::default(), repository).await;
    let alice = admitted(&base, binding("alice"));
    let bob = admitted(&base, binding("bob"));
    let id = create(&alice, Some("original-session")).await;
    let path = format!("/v1/sessions/{id}/events");
    let stop = json!({"events":[{"type":"user.interrupt"}]});
    let key = Some("original-interrupt-command");
    assert_eq!(
        call(&bob, "POST", &path, stop.clone(), key).await.status(),
        StatusCode::NOT_FOUND
    );
    let original = call(&alice, "POST", &path, stop.clone(), key)
        .await
        .expect_for(StatusCode::OK, "admitted interrupt");
    let replay = call(&alice, "POST", &path, stop, key)
        .await
        .expect_for(StatusCode::OK, "exact original interrupt replay");
    assert_eq!(replay, original);
    let events = call(&alice, "GET", &path, Value::Null, None)
        .await
        .expect_for(StatusCode::OK, "current private history");
    assert_eq!(
        events["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "user.interrupt")
            .count(),
        1
    );
}

#[tokio::test]
async fn private_http_creation_and_original_key_replay_preserve_the_complete_native_binding() {
    let repository = support::session_repository("private request creation").await;
    let base = support::shared_state_over(RehydrateFake::default(), repository.clone()).await;
    let alice = admitted(&base, binding("alice"));
    let bob = admitted(&base, binding("bob"));
    let original = create(&alice, Some("same-client-key")).await;
    assert_eq!(create(&alice, Some("same-client-key")).await, original);
    let other = create(&bob, Some("same-client-key")).await;
    assert_ne!(
        original, other,
        "one Workspace and key cannot merge two private owners"
    );
    for (id, owner) in [(&original, "alice"), (&other, "bob")] {
        let root = repository.get(&SessionId::from(id.as_str())).await.unwrap();
        assert_eq!(SessionPrivateBinding::of(&root), Some(&binding(owner)));
    }
    let cold = support::shared_state_over(RehydrateFake::default(), repository.clone()).await;
    assert_eq!(
        create(&admitted(&cold, binding("alice")), Some("same-client-key")).await,
        original
    );
    let ordinary = router(base.clone()).layer(Extension(WorkspaceScope("ws-a".into())));
    assert_ne!(create(&ordinary, Some("same-client-key")).await, original);
    assert!(
        base.list_sessions_scoped("ws-a")
            .unwrap()
            .iter()
            .all(|row| row.id != original && row.id != other)
    );
}

#[tokio::test]
async fn every_private_coordinate_is_checked_before_any_addressed_handler_on_warm_and_cold_routers()
{
    let repository = support::session_repository("private request addresses").await;
    let base = support::shared_state_over(RehydrateFake::default(), repository.clone()).await;
    let alice = admitted(&base, binding("alice"));
    let id = create(&alice, None).await;
    let address = format!("/v1/sessions/{id}");
    assert_eq!(
        call(&alice, "GET", &address, Value::Null, None)
            .await
            .status(),
        StatusCode::OK
    );
    let original = repository.get(&SessionId::from(id.as_str())).await.unwrap();
    for base in [
        base,
        support::shared_state_over(RehydrateFake::default(), repository.clone()).await,
    ] {
        for field in [
            "org_id",
            "application_id",
            "account_id",
            "workspace_id",
            "application_binding_revision",
        ] {
            let mut changed = serde_json::to_value(binding("alice")).unwrap();
            changed[field] = if field == "application_binding_revision" {
                json!(2)
            } else {
                json!("different")
            };
            let denied = admitted(&base, serde_json::from_value(changed).unwrap());
            for (method, suffix) in [
                ("GET", ""),
                ("POST", ""),
                ("DELETE", ""),
                ("POST", "/archive"),
                ("GET", "/events"),
                ("POST", "/events"),
                ("GET", "/events/stream"),
                ("GET", "/threads"),
                ("GET", "/threads/missing/events"),
                ("GET", "/threads/missing/stream"),
                ("GET", "/resources"),
                ("POST", "/resources"),
            ] {
                assert_eq!(
                    call(
                        &denied,
                        method,
                        &format!("{address}{suffix}"),
                        json!({"invalid": true}),
                        None
                    )
                    .await
                    .status(),
                    StatusCode::NOT_FOUND,
                    "{field}: {method} {suffix}"
                );
            }
        }
        let ordinary = router(base).layer(Extension(WorkspaceScope("ws-a".into())));
        assert_eq!(
            call(&ordinary, "GET", &address, Value::Null, None)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        repository.get(&SessionId::from(id.as_str())).await.unwrap(),
        original
    );
}

#[tokio::test]
async fn private_collections_filter_before_pagination_and_never_widen_shared_cached_lists() {
    let repository = support::session_repository("private request collections").await;
    let base = support::shared_state_over(RehydrateFake::default(), repository.clone()).await;
    let ordinary = router(base.clone()).layer(Extension(WorkspaceScope("ws-a".into())));
    let ordinary_id = create(&ordinary, None).await;
    let alice_id = create(&admitted(&base, binding("alice")), None).await;
    let bob_id = create(&admitted(&base, binding("bob")), None).await;
    for base in [
        base,
        support::shared_state_over(RehydrateFake::default(), repository).await,
    ] {
        for (app, expected) in [
            (admitted(&base, binding("alice")), &alice_id),
            (admitted(&base, binding("bob")), &bob_id),
            (
                router(base.clone()).layer(Extension(WorkspaceScope("ws-a".into()))),
                &ordinary_id,
            ),
        ] {
            let page = call(&app, "GET", "/v1/sessions?limit=1", Value::Null, None)
                .await
                .expect_for(StatusCode::OK, "private and ordinary list");
            assert_eq!(page["data"].as_array().unwrap().len(), 1);
            assert_eq!(page["data"][0]["id"], *expected);
            if expected == &ordinary_id {
                assert!(
                    page["data"][0]["agent"].get("model").is_some(),
                    "a private view cannot poison the shared ordinary DTO projection"
                );
            } else {
                assert!(page["data"][0]["agent"].get("model").is_none());
            }
            assert_eq!(
                page.get("next_page"),
                Some(&Value::Null),
                "other owners cannot affect a page boundary"
            );
        }
    }
}

#[tokio::test]
async fn private_native_dtos_history_and_streams_expose_manifests_without_changing_committed_truth()
{
    let repository = support::session_repository("private definition presentation").await;
    let runtime = RehydrateFake::default().with_tool_replacement_recording();
    let replacements = runtime.tool_replacements.clone().unwrap();
    let base = support::shared_state_over(runtime, repository.clone()).await;
    let alice = admitted(&base, binding("alice"));
    let id = create(&alice, Some("private-projection-create")).await;
    let address = format!("/v1/sessions/{id}");
    let response = alice
        .clone()
        .oneshot(
            Request::builder()
                .uri(&address)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let original: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        original["agent"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["id", "name", "type", "version"])
    );
    let tools = json!([{"type":"custom","name":"private-client-tool","description":"PRIVATE TOOL DEFINITION","input_schema":{"type":"object","properties":{}}}]);
    let response = alice
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&address)
                .header("content-type", "application/json")
                .header("if-match", etag)
                .header("idempotency-key", "private-projection-update")
                .body(Body::from(json!({"agent":{"tools":tools}}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let updated: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(updated["agent"], original["agent"]);
    {
        let recorded = replacements.lock();
        assert_eq!(
            recorded.len(),
            1,
            "one native disposable runtime projection"
        );
        assert!(
            recorded[0]
                .1
                .client_tools
                .iter()
                .any(|tool| tool.name == "private-client-tool"
                    && tool.description == "PRIVATE TOOL DEFINITION")
        );
    }
    for state in [
        base,
        support::shared_state_over(RehydrateFake::default(), repository).await,
    ] {
        let private = admitted(&state, binding("alice"));
        let detail = call(&private, "GET", &address, Value::Null, None)
            .await
            .expect_for(StatusCode::OK, "private manifest detail");
        assert_eq!(detail["agent"], original["agent"]);
        for api_date in ["2023-06-01", "2026-10-21"] {
            for time in [None, Some("2")] {
                for path in [address.as_str(), "/v1/sessions"] {
                    let mut request = Request::builder()
                        .uri(path)
                        .header("anthropic-version", api_date);
                    if let Some(time) = time {
                        request = request.header("x-awaken-session-time-version", time);
                    }
                    let response = private
                        .clone()
                        .oneshot(request.body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    let value: Value = serde_json::from_slice(
                        &response.into_body().collect().await.unwrap().to_bytes(),
                    )
                    .unwrap();
                    let agent = if path == address.as_str() {
                        &value["agent"]
                    } else {
                        &value["data"][0]["agent"]
                    };
                    let mut expected = original["agent"].clone();
                    if api_date == "2026-10-21" {
                        expected["execution_identity"] = json!({"type":"service_account"});
                    }
                    assert_eq!(
                        agent, &expected,
                        "API-date and native history time selection both preserve the private manifest"
                    );
                }
            }
        }

        let view = awaken_protocol_managed::admitted_private_session_view(&state, binding("alice"));
        let native = view.get_session(&id).unwrap();
        assert!(
            serde_json::to_value(native.agent)
                .unwrap()
                .to_string()
                .contains("PRIVATE TOOL DEFINITION"),
            "native frozen definition is unchanged by wire presentation"
        );

        let page = call(&private, "GET", "/v1/sessions", Value::Null, None)
            .await
            .expect_for(StatusCode::OK, "private manifest page");
        assert_eq!(page["data"][0]["agent"], original["agent"]);
        let threads = call(
            &private,
            "GET",
            &format!("{address}/threads"),
            Value::Null,
            None,
        )
        .await
        .expect_for(StatusCode::OK, "private manifest Threads");
        for thread in threads["data"].as_array().unwrap() {
            assert!(
                thread["agent"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|key| matches!(
                        key.as_str(),
                        "id" | "name" | "type" | "version" | "execution_identity"
                    ))
            );
            let item = call(
                &private,
                "GET",
                &format!("{address}/threads/{}", thread["id"].as_str().unwrap()),
                Value::Null,
                None,
            )
            .await
            .expect_for(StatusCode::OK, "private manifest Thread");
            assert_eq!(item["agent"], thread["agent"]);
        }
        let history = call(
            &private,
            "GET",
            &format!("{address}/events"),
            Value::Null,
            None,
        )
        .await
        .expect_for(StatusCode::OK, "private manifest history");
        let event = history["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "session.updated" && e.get("agent").is_some())
            .unwrap()
            .clone();
        assert_eq!(event["agent"], original["agent"]);
        assert!(!history.to_string().contains("PRIVATE TOOL DEFINITION"));
        let (observed, text) = stream_event(
            &private,
            &format!("{address}/events/stream"),
            event["id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            observed, event,
            "the original committed id has exactly the same private history/SSE payload"
        );
        assert!(!text.contains("PRIVATE TOOL DEFINITION"));
    }
}

async fn stream_event(app: &Router, path: &str, id: &str) -> (Value, String) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut stream = response.into_body();
    let mut text = String::new();
    for _ in 0..64 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), stream.frame())
            .await
            .expect("native committed SSE frame is available");
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.unwrap();
        if let Some(data) = frame.data_ref() {
            text.push_str(std::str::from_utf8(data).unwrap());
        }
        if let Some(event) = text
            .split("\n\n")
            .flat_map(|frame| frame.lines().filter_map(|line| line.strip_prefix("data: ")))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|e| e["id"] == id)
        {
            return (event, text);
        }
    }
    panic!("native SSE did not expose the committed event id {id}");
}
#[tokio::test]
async fn canonical_event_definition_omission_preserves_full_official_dtos_and_exact_history_sse_events()
 {
    let repository = support::session_repository("canonical event definition omission").await;
    let runtime = RehydrateFake::default().with_tool_replacement_recording();
    let replacements = runtime.tool_replacements.clone().unwrap();
    let base = support::shared_state_over(runtime, repository).await;
    let app = router(base)
        .layer(Extension(WorkspaceScope("ws-a".into())))
        .layer(Extension(
            awaken_protocol_managed::HideSessionEventDefinitions,
        ));
    let id = create(&app, Some("canonical-omission-create")).await;
    let path = format!("/v1/sessions/{id}");
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let full: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        full["agent"].get("model").is_some(),
        "official full DTO required fields are not fabricated or removed"
    );
    let body = json!({"agent":{"tools":[{"type":"custom","name":"canonical-private-tool","description":"CANONICAL PRIVATE DEFINITION","input_schema":{"type":"object","properties":{}}}]}});
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&path)
                .header("content-type", "application/json")
                .header("if-match", etag)
                .header("idempotency-key", "canonical-omission-update")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let full: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        full["agent"]
            .to_string()
            .contains("CANONICAL PRIVATE DEFINITION")
    );
    {
        let recorded = replacements.lock();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0]
                .1
                .client_tools
                .iter()
                .any(|tool| tool.name == "canonical-private-tool"
                    && tool.description == "CANONICAL PRIVATE DEFINITION")
        );
    }
    let history = call(&app, "GET", &format!("{path}/events"), Value::Null, None)
        .await
        .expect_for(StatusCode::OK, "canonical event omission");
    let event = history["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.updated")
        .unwrap()
        .clone();
    assert!(event.get("agent").is_none());
    assert!(!history.to_string().contains("CANONICAL PRIVATE DEFINITION"));
    let (streamed, text) = stream_event(
        &app,
        &format!("{path}/events/stream"),
        event["id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(streamed, event);
    assert!(!text.contains("CANONICAL PRIVATE DEFINITION"));
}
