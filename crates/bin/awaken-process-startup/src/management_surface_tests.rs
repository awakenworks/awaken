// @formal evidence session_resources.repository_rotation_requires_original_live_generation
//! The canonical product management composition over its own quiescent
//! lifecycle and borrowed Session authority, without a public fixture API.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

// Multi-thread flavor matches production `awaken all-in-one`; model publication reads
// the catalog and credential repositories asynchronously.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_and_vault_surfaces_are_served_together() {
    // Cause/effect table: C1 canonical AllInOne management/Vault assembly;
    // C2 Repository generation is still pending with no realization attempt;
    // C3 raw replacement material enters the write-only route. R1=C1+C2+C3
    // => HTTP 200, exact credential revision 2 in the same durable root, no
    // secret in responses. This is an ingress test, not a physical-readiness
    // test: create accepts a complete root and the production supervisor may
    // start its attempt before rotation. R2 attempted=>409 before Vault and R3
    // active=>200 remain covered by repository_authorization_is_sealed_pinned_
    // and_rotated_without_echo; its attempt-race rule covers the async CAS gap.
    // Quiesce only this fixture's own process tasks through their canonical
    // owner before creation, so R1 never relies on being faster than recovery.
    let (process, sessions) = super::prepare_ephemeral_all_in_one_process(std::sync::Arc::new(
        crate::colocated_worker::WorkdirExecution,
    ))
    .await;
    process
        .service_lifecycle
        .shutdown(std::time::Duration::from_secs(5))
        .await
        .expect("quiesce this management-only fixture");
    let app = process.public_router;

    // Admin config CRUD: author model metadata, read the catalog back.
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/model-attributes/test-model",
        Some(json!({ "context_window": 4096 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        catalog["model_attributes"]["test-model"]["context_window"],
        4096
    );

    // Managed vault front door on the same server: create a vault + credential.
    let (s, vault) = call(
        &app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "Prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let (s, cred) = call(
        &app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "environment_variable",
            "secret_name": "ANTHROPIC_API_KEY",
            "secret_value": "sk-mgmt-secret", // awaken-allow(secret):
            "networking": { "type": "unrestricted" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cred["type"], "vault_credential");
    assert!(
        !serde_json::to_string(&cred)
            .unwrap()
            .contains("sk-mgmt-secret")
    );

    // Session surface is still mounted (the managed sessions route responds).
    let (s, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "default",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The product composition, rather than only the protocol fixture, wires the
    // local Vault as the write ingress for Managed repository credentials.  A
    // cloud Environment keeps this assertion at the management boundary: no
    // local Git checkout is needed to prove sealing, pinning, and rotation.
    let (s, environment) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "repository ingress", "config": { "type": "cloud" } })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{environment}");
    let (s, repository_session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "assistant",
            "environment_id": environment["id"],
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/managed-compat.git",
                "authorization_token": "initial-repository-token" // awaken-allow(secret):
            }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{repository_session}");
    assert!(
        !repository_session
            .to_string()
            .contains("initial-repository-token")
    );
    let session_id = repository_session["id"].as_str().unwrap();
    let resource_id = repository_session["resources"][0]["id"].as_str().unwrap();
    let durable_before = sessions
        .get(&awaken_session_types::SessionId::from(session_id))
        .await
        .expect("read this composition's accepted root");
    assert!(durable_before.resources.pending.is_some(), "R1 pending");
    assert!(
        durable_before
            .resources
            .activations
            .iter()
            .all(|activation| activation.attempts == 0),
        "R1 no realization attempt has consumed the desired generation"
    );
    let (s, rotated) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({ "authorization_token": "rotated-repository-token" })), // awaken-allow(secret):
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{rotated}");
    assert!(!rotated.to_string().contains("rotated-repository-token"));
    let durable_after = sessions
        .get(&awaken_session_types::SessionId::from(session_id))
        .await
        .expect("read the committed repin from the same Session authority");
    assert_eq!(
        durable_after.resources.desired_generation().0,
        durable_before.resources.desired_generation().0,
        "R1 rotation amends this exact unattempted generation"
    );
    let awaken_session_configuration_contract::resource::ResolvedInputSource::Repository {
        credential: Some(credential),
        ..
    } = &durable_after.resources.desired().inputs()[0].source
    else {
        panic!("R1 one exact Repository credential pin");
    };
    assert_eq!(credential.access.credential.revision, 2, "R1 durable pin");
}
