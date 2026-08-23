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

mod support;

/// A probe stub that always returns a fixed verdict (records nothing).
struct FixedProbe(ProbeStatus);
#[async_trait::async_trait]
impl CredentialProbe for FixedProbe {
    async fn probe(&self, _base_url: &str, _secret: &RedactedString, _model: &str) -> ProbeStatus {
        self.0
    }
}

struct TestRouter {
    app: Router,
    catalog: Arc<awaken_model_catalog::repo::InMemoryCatalogRepo>,
}

impl std::ops::Deref for TestRouter {
    type Target = Router;

    fn deref(&self) -> &Self::Target {
        &self.app
    }
}

fn router(probe: Option<Arc<dyn CredentialProbe>>) -> TestRouter {
    let catalog = Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new());
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_config_resolver::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new()),
        probe,
        model_discovery: None,
        brokered_catalog: None,
        availability: Arc::new(AvailabilityLedger::new()),
    });
    TestRouter { app, catalog }
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
async fn author_model(app: &TestRouter, provider: &str, dialect: &str, model: &str) {
    support::seed_model(&app.catalog, provider, dialect, model, "ep1").await;
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
    // Test design — Causes: valid positive context/output limits are authored
    // together through HTTP. Effects: both values are stored, each receives a
    // server-time manual provenance stamp, and the catalog exposes them without
    // requiring an Offering. Constraints: clients supply facts, never provenance;
    // the write is one atomic replacement. Decision rule A4=both valid=>all facts.
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
    assert_eq!(echoed["provenance"]["context_window"]["source"], "manual");
    assert_eq!(
        echoed["provenance"]["max_output_tokens"]["source"],
        "manual"
    );
    assert!(
        echoed["provenance"]["context_window"]["observed_at_unix_ms"]
            .as_u64()
            .is_some_and(|timestamp| timestamp > 0)
    );

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
    // Test design — Causes: C1 a prior context fact exists; C2 a later PUT changes
    // context while omitting output. Effects: context is replaced and both the
    // omitted output value and provenance are absent. Constraints: PUT owns full
    // replacement, not a merge that retains stale metadata. Decision rule A9:
    // C1+C2=>new context plus cleared omitted field/provenance.
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
    assert!(catalog["model_attributes"]["m"]["max_output_tokens"].is_null());
    assert!(
        catalog["model_attributes"]["m"]["provenance"]
            .get("max_output_tokens")
            .is_none()
    );
}

#[tokio::test]
async fn model_attribute_clients_cannot_forge_provenance() {
    // Test design — Cause: an HTTP client includes a provider_api provenance
    // object beside an otherwise valid value. Effect: JSON admission rejects the
    // request with 422 before repository mutation. Constraints: only the server
    // stamps trusted field authority/time. Decision rule A7=forged provenance=>reject.
    let app = router(None);
    let (status, _, _) = call(
        &app,
        "PUT",
        "/v1/config/model-attributes/m",
        Some(json!({
            "context_window": 128_000,
            "provenance": {
                "context_window": {"source":"provider_api", "observed_at_unix_ms":1}
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn model_attribute_token_limits_fail_closed_when_impossible() {
    // Test design — Causes: zero context, zero output, or output greater than
    // context. Effects: each returns 422 and no partial model-attribute row
    // exists. Constraints: known limits are positive and output<=context; one
    // invalid field masks the whole write. Decision rules A5/A6 enumerate the
    // two zero boundaries and the cross-field ordering violation.
    let app = router(None);
    for body in [
        json!({"context_window": 0}),
        json!({"max_output_tokens": 0}),
        json!({"context_window": 8_192, "max_output_tokens": 16_384}),
    ] {
        let (status, _, problem) =
            call(&app, "PUT", "/v1/config/model-attributes/m", Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    }
    let (_, _, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert!(catalog["model_attributes"].get("m").is_none());
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
            "target": {
                "model_id": "claude-opus-4-8",
                "provider_id": "anthropic",
                "protocol_endpoint_id": "ep1"
            },
            "binding": { "type": "exact", "credential_source_id": cred }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resolve view: {view}");
    assert_eq!(view["model_id"], "claude-opus-4-8");
    assert_eq!(view["provider_id"], "anthropic");
    assert_eq!(view["protocol_endpoint_id"], "ep1");
    assert_eq!(view["adapter_kind"], "anthropic");
    assert_eq!(view["credential_present"], true);

    // A `none` binding resolves the same model but reports no credential present.
    let (s, _, view) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "target": { "model_id": "claude-opus-4-8" },
            "binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["credential_present"], false);
}

#[tokio::test]
async fn resolve_target_rejects_ambiguity_and_the_retired_flat_shape() {
    // Test design — Causes: C1 structured target is underqualified and matches
    // twice; C2 request sends both target and legacy model_id; C3 sends neither.
    // Effects: C1 returns model_ambiguous and C2/C3 return 422 invalid shape.
    // Constraints: exactly one identity shape is admitted and validation precedes
    // resolution/materialization. Decision rules T3/T5/T6 map C1/C2/C3 exactly.
    let app = router(None);
    author_model(&app, "anthropic", "anthropic_messages", "same-model").await;
    support::seed_model(
        &app.catalog,
        "anthropic",
        "anthropic_messages",
        "same-model",
        "ep2",
    )
    .await;

    let (status, _, problem) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id":"ws", "target":{"model_id":"same-model"}, "binding":{"type":"none"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "model_ambiguous");

    let (status, _, problem) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id":"ws",
            "model_id":"same-model",
            "target":{"model_id":"same-model", "provider_id":"anthropic", "protocol_endpoint_id":"ep2"},
            "binding":{"type":"none"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");

    let (status, _, problem) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({"workspace_id":"ws", "binding":{"type":"none"}})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
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
            "target": { "model_id": "ghost-model" },
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
            "primary": { "target": { "model_id": "primary", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "exact", "credential_source_id": cred } },
            "fallbacks": [{ "target": { "model_id": "fallback", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "exact", "credential_source_id": cred } }],
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
