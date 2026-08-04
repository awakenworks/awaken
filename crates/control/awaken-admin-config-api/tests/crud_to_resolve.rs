//! The admin config plane and resolver share one catalog/credential state. Model
//! setup is covered by the ProviderConnection suite; these tests seed that domain
//! prerequisite below HTTP and focus on credential write-only behavior plus the
//! resolver boundary.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_admin_config_api::{AdminState, admin_router};
use awaken_config_resolver::resolve_inference;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
use awaken_credential_vault::{
    CredentialBinding, CredentialSource, CredentialStatus, InMemorySecretStore, SecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

mod support;

struct Harness {
    app: Router,
    catalog: Arc<InMemoryCatalogRepo>,
    credentials: Arc<InMemoryCredentialRepo>,
    secrets: Arc<InMemorySecretStore>,
}

fn harness() -> Harness {
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(InMemorySecretStore::new());
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles: Arc::new(awaken_config_resolver::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new()),
        probe: None,
        model_discovery: None,
        brokered_catalog: None,
        availability: Default::default(),
    });
    Harness {
        app,
        catalog,
        credentials,
        secrets,
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn author_catalog_and_credential_then_resolve_a_run() {
    // End-to-end cause/effect decision rule E1:
    // valid config + write-only secret => sealed save + secret-free response;
    // exact active binding => materialize/use succeeds; archive => higher disabled
    // revision + reference removal + physical secret reclamation + future resolve
    // fails closed. This traces the complete configure→save→materialize→use→reclaim flow.
    let h = harness();
    support::seed_model(
        &h.catalog,
        "anthropic",
        "anthropic_messages",
        "claude-opus-4-8",
        "ep1",
    )
    .await;

    // Enter a credential; the response is secret-free (secret never echoed).
    let (s, cred) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "vault",
            "provider_id": "anthropic",
            "env_key": "ANTHROPIC_API_KEY",
            "secret": "sk-admin-secret" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let body_text = serde_json::to_string(&cred).unwrap();
    assert!(
        !body_text.contains("sk-admin-secret"),
        "secret leaked: {body_text}"
    );
    let cred_id = cred["id"].as_str().expect("credential id").to_string();

    // Resolve against the SAME stores the router wrote to — the admin plane and the
    // resolver share one catalog/credential state.
    let catalog = h.catalog.snapshot().await.unwrap();
    let source: CredentialSource = h
        .credentials
        .get(&CredentialSourceId(cred_id.clone()))
        .await
        .unwrap();
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    let old_ref = source.material_ref.clone().expect("Vault material ref");
    sources.insert(source.id.0.clone(), source);

    let resolved = resolve_inference(
        &catalog,
        "claude-opus-4-8",
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(cred_id.clone()),
        },
        &sources,
        &*h.secrets,
    )
    .await
    .expect("resolve against authored catalog");

    assert_eq!(resolved.adapter_kind, "anthropic");
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("https://api.example.com/v1/")
    );
    // The secret materializes only here, at the resolver seam.
    assert_eq!(
        resolved.credential.as_ref().unwrap().expose_secret(),
        "sk-admin-secret"
    );

    let (status, retired) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred_id}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retired["status"], "disabled");
    let durable = h
        .credentials
        .get(&CredentialSourceId(cred_id.clone()))
        .await
        .unwrap();
    assert_eq!(durable.status, CredentialStatus::Disabled);
    assert!(durable.material_ref.is_none());
    assert!(h.secrets.get(&old_ref).await.is_err());

    sources.insert(cred_id.clone(), durable);
    assert!(
        resolve_inference(
            &catalog,
            "claude-opus-4-8",
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(cred_id),
            },
            &sources,
            &*h.secrets,
        )
        .await
        .is_err()
    );
}
