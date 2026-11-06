// @formal evidence sandbox.docker_absence_requires_explicit_backend_not_found
//! Decision table for the actual Bollard HTTP deletion/observation boundary.
use super::*;
use awaken_container_contract::RuntimeError;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct Reply {
    method: Method,
    suffix: String,
    status: StatusCode,
    body: Value,
    force: Option<bool>,
}
struct Engine {
    replies: Mutex<VecDeque<Reply>>,
    repeat: Option<Reply>,
    calls: Mutex<Vec<(Method, String)>>,
    removing: Semaphore,
}

async fn respond(
    State(engine): State<Arc<Engine>>,
    method: Method,
    uri: Uri,
) -> axum::response::Response {
    engine
        .calls
        .lock()
        .unwrap()
        .push((method.clone(), uri.path().into()));
    let reply = engine
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .or_else(|| engine.repeat.clone())
        .expect("only planned Engine calls");
    assert_eq!(method, reply.method);
    assert!(
        uri.path().ends_with(&reply.suffix),
        "exact immutable target: {uri}"
    );
    if let Some(force) = reply.force {
        assert!(
            uri.query()
                .unwrap_or_default()
                .split('&')
                .any(|pair| pair == format!("force={force}")),
            "the caller's force policy remains exact: {uri}"
        );
    }
    if reply.body["State"]["Status"] == "removing" {
        engine.removing.add_permits(1);
    }
    (reply.status, Json(reply.body)).into_response()
}

struct Fixture {
    runtime: Arc<DockerRuntime>,
    engine: Arc<Engine>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(replies: Vec<Reply>, repeat: Option<Reply>) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let engine = Arc::new(Engine {
        replies: Mutex::new(replies.into()),
        repeat,
        calls: Mutex::new(Vec::new()),
        removing: Semaphore::new(0),
    });
    let app = Router::new().fallback(respond).with_state(engine.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let docker =
        Docker::connect_with_http(&format!("http://{addr}"), 5, bollard::API_DEFAULT_VERSION)
            .unwrap();
    Fixture {
        runtime: Arc::new(DockerRuntime::with_client(docker, 8080)),
        engine,
        server,
    }
}
fn delete(status: u16) -> Reply {
    Reply {
        method: Method::DELETE,
        suffix: "/containers/original-id".into(),
        status: StatusCode::from_u16(status).unwrap(),
        body: json!({"message":"planned removal response"}),
        force: Some(true),
    }
}
fn inspect(id: &str, status: Option<&str>, response: u16) -> Reply {
    Reply {
        method: Method::GET,
        suffix: "/containers/original-id/json".into(),
        status: StatusCode::from_u16(response).unwrap(),
        body: json!({"Id":id,"State":{"Status":status}}),
        force: None,
    }
}

#[tokio::test]
async fn removal_http_decision_table_never_promotes_conflict_or_unknown_to_completion() {
    // Causes: C1 DELETE ack/404/409/refusal/outage; C2 exact ID observation;
    // C3 Removing/running/absent/unknown state. Effects: E1 exact deletion
    // completion, E2 typed refusal/unknown with dependencies still owned.
    // R1 ack; R2 exact-ID 404; R3 409+Removing then absence => E1.
    // R4 409+running/missing state => Conflict; R5 observation outage => Backend;
    // R6 foreign observed ID => Backend; R7 401/403/500 preserve their classes.
    // Constraints: only the original immutable ID is ever queried, one DELETE,
    // no mutable-name retry, and both positive and negative rows must execute IO.
    for (rule, replies, expected) in [
        ("R1", vec![delete(204)], "ok"),
        ("R2", vec![delete(404)], "ok"),
        (
            "R3",
            vec![
                delete(409),
                inspect("original-id", Some("removing"), 200),
                inspect("original-id", Some("removing"), 200),
                inspect("original-id", None, 404),
            ],
            "ok",
        ),
        (
            "R3 gone before observation",
            vec![delete(409), inspect("original-id", None, 404)],
            "ok",
        ),
        (
            "R4 running",
            vec![delete(409), inspect("original-id", Some("running"), 200)],
            "conflict",
        ),
        (
            "R4 missing state",
            vec![delete(409), inspect("original-id", None, 200)],
            "conflict",
        ),
        (
            "R5",
            vec![delete(409), inspect("original-id", None, 503)],
            "backend",
        ),
        (
            "R6",
            vec![
                delete(409),
                inspect("replacement-id", Some("removing"), 200),
            ],
            "backend",
        ),
        ("R7 unauthorized", vec![delete(401)], "unauthorized"),
        ("R7 forbidden", vec![delete(403)], "unauthorized"),
        ("R7 unavailable", vec![delete(500)], "backend"),
    ] {
        let expected_calls = replies.len();
        let fixture = fixture(replies, None).await;
        let result = fixture.runtime.remove("original-id").await;
        let actual = match result {
            Ok(()) => "ok",
            Err(RuntimeError::Conflict(_)) => "conflict",
            Err(RuntimeError::Unauthorized(_)) => "unauthorized",
            Err(RuntimeError::Backend(_)) => "backend",
            other => panic!("{rule}: unexpected {other:?}"),
        };
        assert_eq!(actual, expected, "{rule}");
        let calls = fixture.engine.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            expected_calls,
            "{rule} complete observation trace"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == Method::DELETE)
                .count(),
            1,
            "{rule} unique deletion"
        );
    }
}

#[tokio::test]
async fn pending_removal_keeps_ownership_until_its_bounded_unknown_outcome() {
    // R8 409 + indefinitely Removing: after an actual first observation the
    // operation remains pending; crossing the one production deadline returns
    // MayHaveCommitted, never completion or a forged exit status. The controlled
    // clock advances only after the real HTTP boundary has been reached.
    let fixture = fixture(
        vec![delete(409)],
        Some(inspect("original-id", Some("removing"), 200)),
    )
    .await;
    let runtime = fixture.runtime.clone();
    let task = tokio::spawn(async move { runtime.remove("original-id").await });
    tokio::time::timeout(Duration::from_secs(2), fixture.engine.removing.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(
        !task.is_finished(),
        "R8 no success while Engine owns the object"
    );
    tokio::time::pause();
    tokio::time::advance(REMOVAL_TIMEOUT + Duration::from_secs(1)).await;
    assert!(
        matches!(task.await.unwrap(), Err(RuntimeError::MayHaveCommitted(_))),
        "R8 exact unknown outcome"
    );
    assert_eq!(
        fixture
            .engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == Method::DELETE)
            .count(),
        1
    );
}

#[tokio::test]
async fn unreceipted_name_is_frozen_to_one_immutable_id_before_removal() {
    // R9 a closed unreceipted scope has a mutable-name observation naming A.
    // Delete/join may touch only A, even if that name is reused for B; every
    // subsequent API path is asserted by the Engine fixture. R10 a name 404
    // is the existing closed-creation absence rule and performs no deletion.
    let fixture = fixture(vec![], None).await;
    let name =
        runtime_container_name(fixture.runtime.realization_namespace.as_str(), "scope").unwrap();
    fixture.engine.replies.lock().unwrap().extend([
        Reply {
            method: Method::GET,
            suffix: format!("/containers/{name}/json"),
            status: StatusCode::OK,
            body: json!({"Id":"original-id"}),
            force: None,
        },
        delete(409),
        inspect("original-id", Some("removing"), 200),
        inspect("original-id", None, 404),
    ]);
    fixture
        .runtime
        .remove_unreceipted_scope("scope")
        .await
        .unwrap();
    assert_eq!(
        fixture.engine.calls.lock().unwrap().len(),
        4,
        "R9 one name resolution followed by exact-ID calls"
    );
}

#[tokio::test]
async fn unreceipted_absence_and_missing_identity_are_distinct() {
    // R10 exact closed-scope 404 => completion with zero delete; R11 a present
    // object lacking immutable ID => Backend with zero delete. An unavailable
    // Engine response likewise cannot be translated into the R10 absence row.
    for (response, body, expected) in [
        (StatusCode::NOT_FOUND, json!({"message":"gone"}), "ok"),
        (StatusCode::OK, json!({}), "backend"),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"message":"unknown"}),
            "backend",
        ),
    ] {
        let fixture = fixture(vec![], None).await;
        let name = runtime_container_name(fixture.runtime.realization_namespace.as_str(), "scope")
            .unwrap();
        fixture.engine.replies.lock().unwrap().push_back(Reply {
            method: Method::GET,
            suffix: format!("/containers/{name}/json"),
            status: response,
            body,
            force: None,
        });
        let actual = match fixture.runtime.remove_unreceipted_scope("scope").await {
            Ok(()) => "ok",
            Err(RuntimeError::Backend(_)) => "backend",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(actual, expected);
        assert_eq!(
            fixture.engine.calls.lock().unwrap().len(),
            1,
            "R10/R11 no deletion"
        );
    }
}

#[tokio::test]
async fn empty_removal_identity_is_refused_before_engine_io() {
    // R12 no immutable target => Backend and zero Engine calls. A malformed
    // locator's possible 404 cannot impersonate an exact-incarnation receipt.
    let fixture = fixture(vec![], None).await;
    assert!(matches!(
        fixture.runtime.remove("").await,
        Err(RuntimeError::Backend(_))
    ));
    assert!(fixture.engine.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn terminal_replacement_keeps_its_non_forced_deletion_policy() {
    // R13 a terminal exact replacement uses force=false; ordinary disposal
    // table rows require force=true. Consolidating the transport must preserve
    // both caller policies, not silently adopt the SDK's false default.
    let mut reply = delete(204);
    reply.force = Some(false);
    let fixture = fixture(vec![reply], None).await;
    fixture
        .runtime
        .remove_confirmed_incarnation("original-id", false)
        .await
        .unwrap();
    assert_eq!(fixture.engine.calls.lock().unwrap().len(), 1);
}
