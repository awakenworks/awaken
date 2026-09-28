//! Vault and credential timestamps are real, durable instants sampled from the
//! owner's injected clock — never a fixed placeholder.
//!
//! Cause/effect table (the clock is a `ManualClock`, so every value is exact):
//! T1 create           -> `created_at == updated_at ==` the create instant.
//! T2 later reads      -> retrieve/list return the persisted instants unchanged.
//! T3 changing update  -> `updated_at` advances to the update instant;
//!                        `created_at` is unchanged.
//! T4 no-op update     -> nothing is written; `updated_at` does not move.
//! T5 archive          -> `archived_at == updated_at ==` the archive instant.
//! T6 delete           -> the durable deletion request / credential tombstone
//!                        carries the delete instant.
//! T7 validate         -> `validated_at` is the instant of that validation;
//!                        a later validation reports its own later instant.
mod support;

use std::sync::Arc;

use awaken_credential_vault::InMemorySecretStore;
use awaken_credential_vault::catalog::{ManagedCredentialLifecycle, ManagedVaultRepo};
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_lease_testkit::ManualClock;
use awaken_protocol_managed::{VaultState, vault_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const CREATED_MS: u64 = 1_789_000_000_000;
const CREATED_AT: &str = "2026-09-10T00:26:40Z";
const UPDATED_MS: u64 = 1_789_000_060_500;
const UPDATED_AT: &str = "2026-09-10T00:27:40.5Z";
const ARCHIVED_MS: u64 = 1_789_000_120_250;
const ARCHIVED_AT: &str = "2026-09-10T00:28:40.25Z";
const DELETED_MS: u64 = 1_789_000_180_000;
const DELETED_AT: &str = "2026-09-10T00:29:40Z";
const LATER_MS: u64 = 1_789_000_240_000;
const LATER_AT: &str = "2026-09-10T00:30:40Z";

struct Harness {
    app: Router,
    clock: Arc<ManualClock>,
    credentials: Arc<InMemoryCredentialRepo>,
}

fn harness() -> Harness {
    let clock = Arc::new(ManualClock::new(CREATED_MS));
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let state = VaultState::new(Arc::new(InMemorySecretStore::new()), credentials.clone())
        .with_clock(clock.clone());
    Harness {
        app: vault_router(Arc::new(state)),
        clock,
        credentials,
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let mut request = builder.body(body).unwrap();
    request
        .extensions_mut()
        .insert(awaken_tenancy::WorkspaceScope("default".to_string()));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    support::Call::new(status, value)
}

fn times(object: &Value) -> (&str, &str) {
    (
        object["created_at"]
            .as_str()
            .expect("created_at is a string"),
        object["updated_at"]
            .as_str()
            .expect("updated_at is a string"),
    )
}

#[tokio::test]
async fn vault_timestamps_are_durable_clock_instants() {
    let h = harness();
    let created = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "timed" })),
    )
    .await
    .expect_for(StatusCode::OK, "T1");
    assert_eq!(times(&created), (CREATED_AT, CREATED_AT), "T1");
    assert_eq!(created["archived_at"], Value::Null, "T1");
    let id = created["id"].as_str().unwrap().to_owned();
    let uri = format!("/v1/vaults/{id}");

    h.clock.set(LATER_MS);
    let read = call(&h.app, "GET", &uri, None)
        .await
        .expect_for(StatusCode::OK, "T2");
    assert_eq!(times(&read), (CREATED_AT, CREATED_AT), "T2 retrieve");
    let listed = call(&h.app, "GET", "/v1/vaults", None)
        .await
        .expect_for(StatusCode::OK, "T2");
    assert_eq!(
        times(&listed["data"][0]),
        (CREATED_AT, CREATED_AT),
        "T2 list"
    );

    h.clock.set(UPDATED_MS);
    let updated = call(
        &h.app,
        "POST",
        &uri,
        Some(json!({ "display_name": "renamed" })),
    )
    .await
    .expect_for(StatusCode::OK, "T3");
    assert_eq!(times(&updated), (CREATED_AT, UPDATED_AT), "T3");

    h.clock.set(LATER_MS);
    let unchanged = call(
        &h.app,
        "POST",
        &uri,
        Some(json!({ "display_name": "renamed" })),
    )
    .await
    .expect_for(StatusCode::OK, "T4");
    assert_eq!(times(&unchanged), (CREATED_AT, UPDATED_AT), "T4");
    let reread = call(&h.app, "GET", &uri, None)
        .await
        .expect_for(StatusCode::OK, "T2/T3");
    assert_eq!(times(&reread), (CREATED_AT, UPDATED_AT), "T2 after update");

    h.clock.set(ARCHIVED_MS);
    let archived = call(&h.app, "POST", &format!("{uri}/archive"), None)
        .await
        .expect_for(StatusCode::OK, "T5");
    assert_eq!(archived["archived_at"], ARCHIVED_AT, "T5");
    assert_eq!(times(&archived), (CREATED_AT, ARCHIVED_AT), "T5");

    h.clock.set(DELETED_MS);
    let _ = call(&h.app, "DELETE", &uri, None)
        .await
        .expect_for(StatusCode::OK, "T6");
    let durable = h
        .credentials
        .get_vault("default", &id)
        .await
        .unwrap()
        .expect("the deleted vault keeps its durable tombstone");
    let deletion = durable.deletion.expect("T6 deletion is recorded");
    assert_eq!(deletion.requested_at, DELETED_AT, "T6");
    assert_eq!(
        durable.created_at, CREATED_AT,
        "T6 creation is never re-dated"
    );
    assert_eq!(
        durable.archived_at.as_deref(),
        Some(ARCHIVED_AT),
        "T6 an earlier archive instant is kept"
    );
}

#[tokio::test]
async fn credential_timestamps_are_durable_clock_instants() {
    let h = harness();
    let vault = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "holder" })),
    )
    .await
    .expect(StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_owned();
    let credentials_uri = format!("/v1/vaults/{vault_id}/credentials");

    let created = call(
        &h.app,
        "POST",
        &credentials_uri,
        Some(json!({
            "type": "mcp_oauth",
            "mcp_server_url": "https://mcp.example.test/mcp",
            "access_token": "at-timestamps", // awaken-allow: secret
        })),
    )
    .await
    .expect_for(StatusCode::OK, "T1");
    assert_eq!(times(&created), (CREATED_AT, CREATED_AT), "T1");
    let id = created["id"].as_str().unwrap().to_owned();
    let uri = format!("{credentials_uri}/{id}");

    h.clock.set(LATER_MS);
    let read = call(&h.app, "GET", &uri, None)
        .await
        .expect_for(StatusCode::OK, "T2");
    assert_eq!(times(&read), (CREATED_AT, CREATED_AT), "T2 retrieve");
    let listed = call(&h.app, "GET", &credentials_uri, None)
        .await
        .expect_for(StatusCode::OK, "T2");
    assert_eq!(
        times(&listed["data"][0]),
        (CREATED_AT, CREATED_AT),
        "T2 list"
    );

    h.clock.set(UPDATED_MS);
    let updated = call(
        &h.app,
        "POST",
        &uri,
        Some(json!({ "display_name": "renamed" })),
    )
    .await
    .expect_for(StatusCode::OK, "T3");
    assert_eq!(times(&updated), (CREATED_AT, UPDATED_AT), "T3");

    h.clock.set(LATER_MS);
    let unchanged = call(
        &h.app,
        "POST",
        &uri,
        Some(json!({ "display_name": "renamed" })),
    )
    .await
    .expect_for(StatusCode::OK, "T4");
    assert_eq!(times(&unchanged), (CREATED_AT, UPDATED_AT), "T4");

    let validated = call(&h.app, "POST", &format!("{uri}/mcp_oauth_validate"), None)
        .await
        .expect_for(StatusCode::OK, "T7");
    assert_eq!(validated["validated_at"], LATER_AT, "T7");
    h.clock.set(ARCHIVED_MS - 1);
    let revalidated = call(&h.app, "POST", &format!("{uri}/mcp_oauth_validate"), None)
        .await
        .expect_for(StatusCode::OK, "T7");
    assert_eq!(
        revalidated["validated_at"], "2026-09-10T00:28:40.249Z",
        "T7 each validation reports its own instant"
    );

    h.clock.set(ARCHIVED_MS);
    let archived = call(&h.app, "POST", &format!("{uri}/archive"), None)
        .await
        .expect_for(StatusCode::OK, "T5");
    assert_eq!(archived["archived_at"], ARCHIVED_AT, "T5");
    assert_eq!(times(&archived), (CREATED_AT, ARCHIVED_AT), "T5");
    let archived_read = call(&h.app, "GET", &uri, None)
        .await
        .expect_for(StatusCode::OK, "T2");
    assert_eq!(times(&archived_read), (CREATED_AT, ARCHIVED_AT), "T2");

    h.clock.set(DELETED_MS);
    let _ = call(&h.app, "DELETE", &uri, None)
        .await
        .expect_for(StatusCode::OK, "T6");
    let tombstone = h
        .credentials
        .get_vault_credential("default", &id)
        .await
        .unwrap()
        .expect("the deleted credential keeps its absorbing tombstone");
    assert_eq!(
        tombstone.lifecycle,
        ManagedCredentialLifecycle::Deleted {
            at: DELETED_AT.into()
        },
        "T6"
    );
    assert_eq!(tombstone.updated_at, DELETED_AT, "T6");
    assert_eq!(tombstone.created_at, CREATED_AT, "T6");
}

/// A retried `Idempotency-Key` create arrives at a later instant. The replay
/// returns the one durable child, dated by the first create, rather than
/// rejecting the retry as different content.
#[tokio::test]
async fn idempotent_credential_replay_keeps_the_first_creation_instant() {
    let h = harness();
    let vault = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "replay" })),
    )
    .await
    .expect(StatusCode::OK);
    let uri = format!("/v1/vaults/{}/credentials", vault["id"].as_str().unwrap());
    let body = json!({
        "type": "environment_variable",
        "secret_name": "REPLAY_KEY",
        "secret_value": "sk-replay", // awaken-allow: secret
        "networking": { "type": "unrestricted" }
    });
    let send = |app: Router, body: Value| {
        let uri = uri.clone();
        async move {
            let mut request = Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("idempotency-key", "timestamps-replay")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            request
                .extensions_mut()
                .insert(awaken_tenancy::WorkspaceScope("default".to_string()));
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let (status, first) = send(h.app.clone(), body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    h.clock.set(LATER_MS);
    let (status, replayed) = send(h.app.clone(), body).await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["id"], first["id"]);
    assert_eq!(times(&replayed), (CREATED_AT, CREATED_AT));
}
