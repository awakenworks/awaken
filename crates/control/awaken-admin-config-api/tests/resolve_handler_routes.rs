//! Handler-level coverage for the dry-run resolve surface driven end-to-end
//! through the `axum` router over HTTP (the existing resolve tests call
//! `resolve_inference` / `resolve_profile*` directly and never touch the
//! handlers). Drives `resolve_route`, `resolve_profile_route`, the
//! `put_profile`/`get_profile` round-trip, and `resolve_profile_candidates_route`
//! — including its all-unresolvable FAIL-CLOSED path — plus the `validate_credential`
//! VC4 probe-returns-`invalid` arm the `router_cases` suite skips (it labels VC1/2/3/5).
//!
//! Every route is exercised through `oneshot` + `http-body-util`, reusing the same
//! store-injection harness idiom as `router_cases.rs`.

use std::sync::{Arc, Mutex};

use awaken_admin_config_api::{AdminState, CredentialProbe, ProbeStatus, admin_router};
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::AvailabilityLedger;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Records every probe call and answers with a fixed status (same double the
/// `router_cases` suite uses), so a test can prove the probe ran with the resolved
/// endpoint + materialized secret and still returned its configured verdict.
struct RecordingProbe {
    status: ProbeStatus,
    calls: Mutex<Vec<(String, String, String)>>,
}

impl RecordingProbe {
    fn new(status: ProbeStatus) -> Arc<Self> {
        Arc::new(Self {
            status,
            calls: Mutex::new(Vec::new()),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl CredentialProbe for RecordingProbe {
    async fn probe(&self, base_url: &str, secret: &RedactedString, model: &str) -> ProbeStatus {
        self.calls.lock().unwrap().push((
            base_url.to_string(),
            secret.expose_secret().to_string(),
            model.to_string(),
        ));
        self.status
    }
}

struct Harness {
    app: Router,
}

fn harness_with(probe: Option<Arc<dyn CredentialProbe>>) -> Harness {
    let app = admin_router(AdminState {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
        probe,
        availability: Arc::new(AvailabilityLedger::new()),
    });
    Harness { app }
}

fn harness() -> Harness {
    harness_with(None)
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

/// Author a resolvable model: provider + endpoint (`dialect`) + offering.
async fn author_model(app: &Router, provider: &str, dialect: &str, model: &str) {
    let (s, _) = call(
        app,
        "PUT",
        &format!("/v1/config/providers/{provider}"),
        Some(json!({ "id": provider, "slug": provider, "display_name": provider, "version": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        app,
        "PUT",
        "/v1/config/endpoints/ep1",
        Some(json!({
            "id": "ep1", "provider_id": provider, "dialect": dialect,
            "base_url": "https://api.example.com/v1/", "timeout_secs": 300,
            "display_name": "prod", "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        app,
        "POST",
        "/v1/config/offerings",
        Some(json!({
            "model_id": model, "provider_id": provider,
            "protocol_endpoint_id": "ep1", "dialect": dialect, "upstream_model": null
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

/// Enter a vault credential scoped to `provider` and return its id.
async fn enter_vault_cred(app: &Router, provider: Option<&str>, secret: &str) -> String {
    let (s, cred) = call(
        app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws", "kind": "vault", "provider_id": provider,
            "env_key": null, "secret": secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    cred["id"].as_str().expect("credential id").to_string()
}

// ---------------------------------------------------------------------------
// TASK 2 — resolve_route (POST /v1/config/inference/resolve) through the handler
// ---------------------------------------------------------------------------

/// resolve_route (success, None binding): a dry-run resolve returns the secret-free
/// view — the execution triple + adapter/endpoint — with `credential_present:false`
/// when the binding materializes no secret.
#[tokio::test]
async fn resolve_route_none_binding_is_secret_free_view() {
    let h = harness();
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;

    let (s, view) = call(
        &h.app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "claude-opus-4-8",
            "binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["model_id"], "claude-opus-4-8");
    assert_eq!(view["provider_id"], "anthropic");
    assert_eq!(view["protocol_endpoint_id"], "ep1");
    assert_eq!(view["adapter_kind"], "anthropic");
    assert_eq!(view["base_url"], "https://api.example.com/v1/");
    assert_eq!(view["credential_present"], false);
}

/// resolve_route (success, Exact binding): the same view reports
/// `credential_present:true` once a credential materializes, but never carries the
/// secret itself.
#[tokio::test]
async fn resolve_route_exact_binding_present_and_secret_free() {
    let h = harness();
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-resolve-route").await;

    let (s, view) = call(
        &h.app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "claude-opus-4-8",
            "binding": { "type": "exact", "credential_source_id": cred }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["credential_present"], true);
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("sk-resolve-route"),
        "secret leaked on resolve view"
    );
}

/// resolve_route (fail-closed): a model with no offering → the resolve problem
/// mapper answers 404 `model_unresolved`.
#[tokio::test]
async fn resolve_route_unknown_model_is_404_model_unresolved() {
    let h = harness();
    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "ghost-model",
            "binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "model_unresolved");
}

// ---------------------------------------------------------------------------
// TASK 2 — put_profile / get_profile round-trip + resolve_profile_route
// ---------------------------------------------------------------------------

/// put_profile / get_profile: authoring a profile round-trips through the store,
/// and the path id is what the profile is fetched under.
#[tokio::test]
async fn put_then_get_profile_round_trips() {
    let h = harness();

    // A fresh id is not found until authored.
    let (s, err) = call(&h.app, "GET", "/v1/config/inference-profiles/prof-1", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");

    let (s, put) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-1",
        Some(json!({
            "model_id": "claude-opus-4-8",
            "credential_binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(put["model_id"], "claude-opus-4-8");

    let (s, got) = call(&h.app, "GET", "/v1/config/inference-profiles/prof-1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["model_id"], "claude-opus-4-8");
    assert_eq!(got["credential_binding"]["type"], "none");
}

/// resolve_profile_route (success): a stored profile resolves its *primary* model
/// through the handler into the secret-free view.
#[tokio::test]
async fn resolve_profile_route_resolves_primary_model() {
    let h = harness();
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-1",
        Some(json!({
            "model_id": "claude-opus-4-8",
            "credential_binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, view) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-1/resolve",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["model_id"], "claude-opus-4-8");
    assert_eq!(view["adapter_kind"], "anthropic");
    assert_eq!(view["credential_present"], false);
}

/// resolve_profile_route (missing profile): an unauthored id → 404 `not_found`
/// (`profile_missing`).
#[tokio::test]
async fn resolve_profile_route_missing_profile_is_404() {
    let h = harness();
    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/nobody/resolve",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["status"], 404);
}

// ---------------------------------------------------------------------------
// TASK 1 — resolve_profile_candidates_route through the handler.
// `/v1/config/inference-profiles/{id}/resolve-candidates` is mounted by
// `admin_router` AND documented in `openapi::paths()` (response schema
// `ResolvedCandidatesView`), so the `openapi_contract` drift gate probes it; these
// tests exercise the handler behavior end-to-end.
// ---------------------------------------------------------------------------

/// resolve_profile_candidates_route (success): a multi-model profile resolves its
/// whole model axis into the ordered candidate list, one secret-free view per model
/// in failover order.
#[tokio::test]
async fn resolve_candidates_route_returns_ordered_axis() {
    let h = harness();
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    // A second offering under the same endpoint gives the fallback model a resolution.
    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/config/offerings",
        Some(json!({
            "model_id": "claude-haiku-4-8", "provider_id": "anthropic",
            "protocol_endpoint_id": "ep1", "dialect": "anthropic_messages", "upstream_model": null
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-multi",
        Some(json!({
            "model_id": "claude-opus-4-8",
            "model_fallbacks": ["claude-haiku-4-8"],
            "credential_binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, out) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-multi/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let candidates = out["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2);
    // Axis order is preserved: primary first, fallback second.
    assert_eq!(candidates[0]["model_id"], "claude-opus-4-8");
    assert_eq!(candidates[1]["model_id"], "claude-haiku-4-8");
}

/// resolve_profile_candidates_route (partial resolve is not terminal): an
/// unresolvable fallback (no offering) is *skipped*, so the primary still yields a
/// one-element candidate list — one bad model does not sink the profile.
#[tokio::test]
async fn resolve_candidates_route_skips_unresolvable_fallback() {
    let h = harness();
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;

    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-partial",
        Some(json!({
            "model_id": "claude-opus-4-8",
            "model_fallbacks": ["ghost-model"],
            "credential_binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, out) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-partial/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let candidates = out["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["model_id"], "claude-opus-4-8");
}

/// resolve_profile_candidates_route (ALL-UNRESOLVABLE fail-closed): a profile whose
/// every model has no offering resolves to *no* candidate, and the handler fails
/// closed on the last reason — 404 `model_unresolved`, never an empty-list success.
#[tokio::test]
async fn resolve_candidates_route_all_unresolvable_is_fail_closed_404() {
    let h = harness();
    // No catalog authored at all → both models are unresolvable.
    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-dead",
        Some(json!({
            "model_id": "ghost-primary",
            "model_fallbacks": ["ghost-fallback"],
            "credential_binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-dead/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "model_unresolved");
    // Fail-closed: no candidate list ever leaks on the error body.
    assert!(err.get("candidates").is_none());
}

/// resolve_profile_candidates_route (missing profile): an unauthored profile id →
/// 404 `not_found` (`profile_missing`), before any catalog snapshot.
#[tokio::test]
async fn resolve_candidates_route_missing_profile_is_404() {
    let h = harness();
    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/nobody/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["status"], 404);
}

// ---------------------------------------------------------------------------
// TASK 2 — validate_credential VC4: probe runs and returns `invalid`
// (router_cases labels VC1 valid / VC2 no-probe / VC3 non-anthropic / VC5 resolve
// failure, but never the probe-returns-invalid arm).
// ---------------------------------------------------------------------------

/// VC4: probe wired + anthropic adapter + materialized secret → the real probe runs
/// and its `invalid` verdict is returned end-to-end, with the resolved endpoint +
/// materialized secret + model fed to it, and no secret on the wire response.
#[tokio::test]
async fn vc4_probe_invalid_is_returned_end_to_end() {
    let probe = RecordingProbe::new(ProbeStatus::Invalid);
    let h = harness_with(Some(probe.clone()));
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-live-invalid").await;

    let (s, body) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "claude-opus-4-8" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "invalid");
    assert_eq!(body["adapter_kind"], "anthropic");

    // The probe ran exactly once, with the resolved endpoint + materialized secret.
    assert_eq!(probe.call_count(), 1);
    let calls = probe.calls.lock().unwrap();
    assert_eq!(calls[0].0, "https://api.example.com/v1/");
    assert_eq!(calls[0].1, "sk-live-invalid");
    assert_eq!(calls[0].2, "claude-opus-4-8");
    assert!(
        !serde_json::to_string(&body)
            .unwrap()
            .contains("sk-live-invalid"),
        "secret leaked on validate response"
    );
}
