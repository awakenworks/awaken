//! Composition-root wiring for the management plane (ADR-0043): build a provider +
//! endpoint + offering and enter a credential through the **production** APIs, resolve
//! the run's inference, then let `build_resolved_router` turn that `ResolvedInference`
//! into the host's model executor and mount the protocol adapters over it.
//!
//! This closes the `config → resolve → run` chain at the server's composition root —
//! the same seam the binary uses — rather than building an executor straight from env
//! (as `real_model.rs` does). The hermetic tests prove the seam is fail-closed and
//! selects the right adapter/base_url/credential; the `#[ignore]` test drives a real
//! `message:send` end-to-end when a live key is set.

use std::collections::HashMap;

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::resolve_inference;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialBinding, CredentialCreateParams, CredentialKind, CredentialSource,
    CredentialSourceId, InMemorySecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{
    ModelApiCompat, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
    ProviderId,
};
use awaken_server_local::{ResolvedExecutorError, build_resolved_router, executor_from_resolved};

/// Register provider + endpoint + offering through the real admin repo.
async fn seed_catalog(base_url: &str, model_id: &str) -> ProviderCatalog {
    let repo = InMemoryCatalogRepo::new();
    repo.put_provider(Provider {
        id: ProviderId::new("anthropic"),
        slug: "anthropic".into(),
        display_name: "Anthropic".into(),
        version: 1,
    })
    .await
    .unwrap();
    repo.put_endpoint(ProtocolEndpoint {
        id: ProtocolEndpointId::new("ep1"),
        provider_id: ProviderId::new("anthropic"),
        flavor: ModelApiCompat::AnthropicMessages,
        base_url: Some(base_url.into()),
        timeout_secs: 300,
        display_name: "prod".into(),
        version: 1,
    })
    .await
    .unwrap();
    repo.put_offering(Offering {
        model_id: model_id.into(),
        provider_id: ProviderId::new("anthropic"),
        protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
        flavor: ModelApiCompat::AnthropicMessages,
        upstream_model: None,
    })
    .await
    .unwrap();
    repo.snapshot().await.unwrap()
}

/// Enter a credential and resolve `model_id` against a seeded catalog — the exact
/// chain the composition root performs before it can build an executor.
async fn resolve(
    base_url: &str,
    model_id: &str,
    secret: &str,
) -> awaken_config_resolver::ResolvedInference {
    let catalog = seed_catalog(base_url, model_id).await;

    let store = InMemorySecretStore::new();
    let cred_repo = InMemoryCredentialRepo::new();
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new(secret)),
        },
        &store,
        &cred_repo,
    )
    .await
    .unwrap();

    let row = cred_repo.get(&source.id).await.unwrap();
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(row.id.0.clone(), row);

    resolve_inference(
        &catalog,
        model_id,
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(source.id.0.clone()),
        },
        &sources,
        &store,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn resolved_inference_builds_a_router_at_the_composition_root() {
    let inference = resolve(
        "https://api.anthropic.com/v1/",
        "claude-opus-4-8",
        "sk-secret",
    )
    .await;
    // The seam accepts the anthropic adapter and produces an executor + router.
    assert!(executor_from_resolved(&inference).is_ok());
    assert!(build_resolved_router(&inference).is_ok());
}

#[tokio::test]
async fn seam_fails_closed_without_a_credential() {
    // An unauthenticated binding resolves to a credential-free inference; the seam
    // refuses to build an executor rather than call the model unauthenticated.
    let catalog = seed_catalog("https://api.anthropic.com/v1/", "claude-opus-4-8").await;
    let store = InMemorySecretStore::new();
    let sources: HashMap<String, CredentialSource> = HashMap::new();
    let inference = resolve_inference(
        &catalog,
        "claude-opus-4-8",
        &CredentialBinding::None,
        &sources,
        &store,
    )
    .await
    .unwrap();
    assert!(inference.credential.is_none());
    assert!(matches!(
        executor_from_resolved(&inference),
        Err(ResolvedExecutorError::MissingCredential)
    ));
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with ANTHROPIC_BASE_URL/KEY/MODEL set and --ignored"]
async fn message_send_over_a_resolved_live_model() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY (or KIMI_API_KEY) to run this test");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string());

    // Full chain: admin config → credential entry → resolve → build the executor
    // from the resolved inference → mount the server. Nothing here names the raw
    // key or base URL to the router; they arrive only through the resolver.
    let inference = resolve(&base, &model, &key).await;
    let app = build_resolved_router(&inference).expect("resolved router");

    // One A2A turn against the live model, steered to a short, tool-free reply.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/a2a/message:send")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "message": {
                "messageId": "m1",
                "contextId": "resolved-real-1",
                "role": "user",
                "parts": [{ "kind": "text", "text": "Reply with the single word: pong" }],
            }}))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        text.contains("completed") || text.to_lowercase().contains("pong"),
        "{text}"
    );
}
