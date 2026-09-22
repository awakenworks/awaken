//! Real router + SQLite registry. Credential decisions are metadata fixtures;
//! this suite does not claim a Git network fetch or model execution.

use awaken_protocol_managed::repositories_router;
use awaken_resource_contract::{RepositoryCredentialValidator, ResourceRegistryError};
use awaken_tenancy::WorkspaceScope;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
mod support;

struct Admission;
#[async_trait::async_trait]
impl RepositoryCredentialValidator for Admission {
    async fn validate(
        &self,
        workspace: &str,
        source: &str,
        _: &str,
    ) -> Result<(), ResourceRegistryError> {
        match (workspace, source) {
            ("test", "github-source") => Ok(()),
            (_, "outage") => Err(ResourceRegistryError::AuthorityUnavailable {
                detail: "private database location".into(),
            }),
            _ => Err(ResourceRegistryError::AdmissionRejected {
                detail: "Repository authorization is unavailable".into(),
            }),
        }
    }
}

fn setup() -> (Router, awaken_resource_application::ResourcesApplication) {
    let resources = support::resources::ephemeral_resources();
    (
        repositories_router(resources.repositories(Arc::new(Admission))),
        resources,
    )
}

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    workspace: Option<&str>,
    key: Option<&str>,
    body: Value,
) -> support::Call<serde_json::Value> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    let mut request = builder
        .body(if method == "GET" {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    if let Some(workspace) = workspace {
        request
            .extensions_mut()
            .insert(WorkspaceScope(workspace.into()));
    }
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    support::Call::new(status, serde_json::from_slice(&bytes).unwrap())
}

const PATH: &str = "/v1/awaken/repositories";
fn body() -> Value {
    json!({"name": "Workshop", "remote_url": "https://github.com/example/workshop.git", "initial_branch": "main"})
}

#[tokio::test]
async fn committed_response_loss_replays_one_repository_and_workspace_isolation_holds() {
    let (router, _) = setup();
    let first = call(&router, "POST", PATH, Some("test"), Some("request"), body())
        .await
        .expect_for(StatusCode::CREATED, "expected status");
    let retry = call(&router, "POST", PATH, Some("test"), Some("request"), body()).await;
    assert_eq!(
        retry.expect(StatusCode::CREATED),
        first,
        "discarded committed response can be recovered"
    );
    assert!(first.get("creation_request_sha256").is_none());
    assert_eq!(first["config"]["remote_url"], body()["remote_url"]);
    let id = first["id"].as_str().unwrap();
    let retrieved = call(
        &router,
        "GET",
        &format!("{PATH}/{id}"),
        Some("test"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(retrieved.expect(StatusCode::OK), first);
    let _foreign = call(
        &router,
        "GET",
        &format!("{PATH}/{id}"),
        Some("other"),
        None,
        Value::Null,
    )
    .await
    .expect(StatusCode::NOT_FOUND);
    let second_workspace = call(
        &router,
        "POST",
        PATH,
        Some("other"),
        Some("request"),
        body(),
    )
    .await
    .expect(StatusCode::CREATED);
    assert_ne!(second_workspace["id"], first["id"]);
    let list = call(&router, "GET", PATH, Some("test"), None, Value::Null)
        .await
        .expect(StatusCode::OK);
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let mut changed = body();
    changed["initial_branch"] = json!("release");
    let conflict = call(
        &router,
        "POST",
        PATH,
        Some("test"),
        Some("request"),
        changed,
    )
    .await
    .expect(StatusCode::CONFLICT);
    assert_eq!(conflict["error"]["type"], "conflict_error");
}

#[tokio::test]
async fn authoring_requires_trusted_workspace_key_and_secret_free_body() {
    let (router, _) = setup();
    for workspace in [None, Some("")] {
        assert_eq!(
            call(&router, "POST", PATH, workspace, Some("key"), body())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&router, "GET", PATH, workspace, None, Value::Null)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    for key in [None, Some(""), Some("has space")] {
        assert_eq!(
            call(&router, "POST", PATH, Some("test"), key, body())
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    for (field, value) in [
        ("token", json!("do-not-echo-secret")),
        ("workspace_id", json!("spoofed")),
        (
            "remote_url",
            json!("https://user:do-not-echo-secret@github.com/org/repo"),
        ),
        ("initial_commit", json!("a".repeat(40))),
    ] {
        let mut request = body();
        request[field] = value;
        let response = call(&router, "POST", PATH, Some("test"), Some("key"), request)
            .await
            .expect_for(StatusCode::BAD_REQUEST, "expected status");
        assert!(!response.to_string().contains("do-not-echo-secret"));
    }
    let list = call(&router, "GET", PATH, Some("test"), None, Value::Null)
        .await
        .expect(StatusCode::OK);
    assert!(list["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn private_admission_and_outage_are_distinct_and_failed_attempts_do_not_reserve_identity() {
    let (router, _) = setup();
    for (source, status) in [
        ("foreign-source", StatusCode::BAD_REQUEST),
        ("outage", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let mut request = body();
        request["credential_binding"] = json!(source);
        let result = call(
            &router,
            "POST",
            PATH,
            Some("test"),
            Some("private"),
            request,
        )
        .await
        .expect(status);
        assert!(!result.to_string().contains("private database location"));
    }
    let mut request = body();
    request["credential_binding"] = json!("github-source");
    let result = call(
        &router,
        "POST",
        PATH,
        Some("test"),
        Some("private"),
        request,
    )
    .await;
    let result = result.expect(StatusCode::CREATED);
    assert_eq!(result["config"]["credential_binding"], "github-source");
}

#[tokio::test]
async fn pagination_is_complete_and_retired_repository_cannot_be_recreated() {
    use awaken_resource_contract::{ChangeRepositoryState, ResourceState};
    let (router, resources) = setup();
    let mut ids = Vec::new();
    for key in ["one", "two", "three"] {
        let result = call(&router, "POST", PATH, Some("test"), Some(key), body())
            .await
            .expect(StatusCode::CREATED);
        ids.push(result["id"].as_str().unwrap().to_owned());
    }
    let page = call(
        &router,
        "GET",
        &format!("{PATH}?limit=2"),
        Some("test"),
        None,
        Value::Null,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    let next = page["next_page"].as_str().unwrap();
    let last = call(
        &router,
        "GET",
        &format!("{PATH}?limit=2&page={next}"),
        Some("test"),
        None,
        Value::Null,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(last["data"].as_array().unwrap().len(), 1);
    assert!(last["next_page"].is_null());
    resources
        .authorities()
        .resource_registry()
        .change_repository_state(ChangeRepositoryState {
            workspace_id: "test".into(),
            id: ids[0].clone().into(),
            state: ResourceState::Deleted,
        })
        .await
        .unwrap();
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("{PATH}/{}", ids[0]),
            Some("test"),
            None,
            Value::Null
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&router, "POST", PATH, Some("test"), Some("one"), body())
            .await
            .status(),
        StatusCode::CONFLICT
    );
}
