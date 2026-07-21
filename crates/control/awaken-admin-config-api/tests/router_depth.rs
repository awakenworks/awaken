//! Depth coverage (P1-#10) for admin-config routes that were mounted but thinly
//! or never driven through the live HTTP surface: `put_model_attributes` (no test
//! existed), the `quota`-without-`retry_after` default-window branch, the
//! `get_pool_eligible` missing-pool 404, the `resolve` / `resolve-candidates`
//! routes end-to-end (previously only the library resolver was exercised), and the
//! `validate` `Invalid` verdict (the VC suite only fed `Valid`).
//!
//! Same store-injection + `oneshot` harness as `router_cases.rs`.

use std::sync::Arc;

use awaken_admin_config_api::{AdminState, CredentialProbe, ProbeStatus, admin_router};
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::AvailabilityLedger;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header::CONTENT_TYPE};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A probe stub that always returns a fixed verdict (records nothing).
struct FixedProbe(ProbeStatus);
#[async_trait::async_trait]
impl CredentialProbe for FixedProbe {
    async fn probe(&self, _base_url: &str, _secret: &RedactedString, _model: &str) -> ProbeStatus {
        self.0
    }
}

fn router(probe: Option<Arc<dyn CredentialProbe>>) -> Router {
    admin_router(AdminState {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
        probe,
        availability: Arc::new(AvailabilityLedger::new()),
    })
}

/// Issue a request; return `(status, content_type, json_body)`.
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
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
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, content_type, value)
}

/// Author a resolvable model on `provider` (idempotent per provider/endpoint).
async fn author_model(app: &Router, provider: &str, dialect: &str, model: &str) {
    let (s, _, _) = call(
        app,
        "PUT",
        &format!("/v1/config/providers/{provider}"),
        Some(json!({ "id": provider, "slug": provider, "display_name": provider, "version": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = call(
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
    let (s, _, _) = call(
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

/// Enter a vault credential scoped to `provider`; return its id.
async fn enter_vault_cred(app: &Router, provider: &str, secret: &str) -> String {
    let (s, _, cred) = call(
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

// ── put_model_attributes (previously zero coverage) ──────────────────────────

#[tokio::test]
async fn model_attributes_publish_and_surface_in_the_catalog() {
    let app = router(None);
    let (s, _, echoed) = call(
        &app,
        "PUT",
        "/v1/config/model-attributes/kimi-k2",
        Some(json!({ "context_window": 1_000_000, "max_output_tokens": 8192 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(echoed["context_window"], 1_000_000);
    assert_eq!(echoed["max_output_tokens"], 8192);

    // They publish independently of any offering and land in the catalog snapshot.
    let (s, _, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        catalog["model_attributes"]["kimi-k2"]["context_window"], 1_000_000,
        "authored attributes surface in the catalog: {catalog}"
    );
}

#[tokio::test]
async fn model_attributes_upsert_replaces_the_prior_value() {
    let app = router(None);
    for window in [128_000, 262_144] {
        let (s, _, _) = call(
            &app,
            "PUT",
            "/v1/config/model-attributes/m",
            Some(json!({ "context_window": window })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
    let (_, _, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(catalog["model_attributes"]["m"]["context_window"], 262_144);
}

// ── cooldown: the quota default-window branch (CD1 always passed retry_after) ──

#[tokio::test]
async fn quota_cooldown_without_a_retry_after_uses_the_default_window() {
    let app = router(None);
    // No `retry_after_secs` → the `None` branch of `cooldown_deadline` (a default
    // window), the arm CD1 never exercises because it always passes 3600.
    let (s, _, state) = call(
        &app,
        "POST",
        "/v1/config/credentials/src-default/cooldown",
        Some(json!({ "kind": "quota" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(state["state"], "cooled_down");
    assert!(
        state["retry_at_ms"].as_u64().unwrap_or(0) > 0,
        "a default-window quota cooldown still sets a resume deadline: {state}"
    );
}

// ── get_pool_eligible: the missing-pool 404 arm ───────────────────────────────

#[tokio::test]
async fn pool_eligible_on_a_missing_pool_is_a_domain_404() {
    let app = router(None);
    let (s, content_type, body) = call(
        &app,
        "GET",
        "/v1/config/credential-pools/ghost/eligible",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "missing pool is a 404: {body}");
    assert_eq!(
        content_type, "application/problem+json",
        "a domain 404 speaks RFC 9457, not a bare routing miss"
    );
    assert!(body["code"].is_string(), "problem carries a code: {body}");
}

// ── resolve routes over HTTP (previously only the library resolver) ───────────

#[tokio::test]
async fn resolve_over_http_reports_the_binding_and_credential_presence() {
    let app = router(None);
    author_model(&app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&app, "anthropic", "sk-resolve").await;

    // Exact binding to a real vault credential → credential_present, anthropic adapter.
    let (s, _, view) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "claude-opus-4-8",
            "binding": { "type": "exact", "credential_source_id": cred }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resolve view: {view}");
    assert_eq!(view["model_id"], "claude-opus-4-8");
    assert_eq!(view["provider_id"], "anthropic");
    assert_eq!(view["adapter_kind"], "anthropic");
    assert_eq!(view["credential_present"], true);

    // A `none` binding resolves the same model but reports no credential present.
    let (s, _, view) = call(
        &app,
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
    assert_eq!(view["credential_present"], false);
}

#[tokio::test]
async fn resolve_over_http_fails_closed_for_an_unknown_model() {
    let app = router(None);
    // No offering authored for this model → the resolver fails closed with a problem.
    let (s, content_type, body) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "ghost-model",
            "binding": { "type": "none" }
        })),
    )
    .await;
    assert!(
        s.is_client_error() || s.is_server_error(),
        "an unresolvable model is not a 2xx: {s} {body}"
    );
    assert_eq!(content_type, "application/problem+json");
}

#[tokio::test]
async fn resolve_candidates_over_http_lists_the_failover_order() {
    let app = router(None);
    author_model(&app, "anthropic", "anthropic_messages", "primary").await;
    author_model(&app, "anthropic", "anthropic_messages", "fallback").await;
    let cred = enter_vault_cred(&app, "anthropic", "sk-cand").await;

    // A profile whose model axis is primary → fallback, bound to the vault credential.
    let (s, _, _) = call(
        &app,
        "PUT",
        "/v1/config/inference-profiles/prof",
        Some(json!({
            "model_id": "primary",
            "model_fallbacks": ["fallback"],
            "credential_binding": { "type": "exact", "credential_source_id": cred },
            "disabled_endpoint_ids": []
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, _, view) = call(
        &app,
        "POST",
        "/v1/config/inference-profiles/prof/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "candidates: {view}");
    let candidates = view["candidates"].as_array().expect("candidates array");
    let models: Vec<&str> = candidates
        .iter()
        .filter_map(|c| c["model_id"].as_str())
        .collect();
    assert_eq!(
        models,
        vec!["primary", "fallback"],
        "candidates are in failover order"
    );
    assert!(
        candidates
            .iter()
            .all(|c| c["credential_present"] == json!(true)),
        "each candidate carries the bound credential: {view}"
    );
}

#[tokio::test]
async fn resolve_candidates_on_a_missing_profile_is_a_404() {
    let app = router(None);
    let (s, content_type, _) = call(
        &app,
        "POST",
        "/v1/config/inference-profiles/ghost/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(content_type, "application/problem+json");
}

// ── validate: the Invalid verdict (VC suite only fed Valid) ───────────────────

#[tokio::test]
async fn validate_surfaces_an_invalid_verdict_from_the_probe() {
    let app = router(Some(Arc::new(FixedProbe(ProbeStatus::Invalid))));
    author_model(&app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&app, "anthropic", "sk-bad").await;

    let (s, _, body) = call(
        &app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "claude-opus-4-8" })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "validate returns a verdict, not an error"
    );
    assert_eq!(
        body["status"], "invalid",
        "the probe's Invalid verdict reaches the wire: {body}"
    );
    assert_eq!(body["adapter_kind"], "anthropic");
}
