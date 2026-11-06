//! Request-bound private views share native Session truth and hide other owners.

mod support;

use std::sync::Arc;

use awaken_protocol_managed::{AdmittedPrivateSessionRequest, ManagedState, router};
use awaken_protocol_managed_testkit::RehydrateFake;
use awaken_session_contract::{SessionPrivateBinding, SessionRepositoryReads};
use awaken_session_types::SessionId;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
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
            assert_eq!(
                page.get("next_page"),
                Some(&Value::Null),
                "other owners cannot affect a page boundary"
            );
        }
    }
}
