//! P0 full-chain acceptance (ADR-0043): config (catalog + credential) → resolve →
//! the provider at the injection seam. The hermetic test proves the resolved
//! secret reaches `GenaiExecutor` (the runtime provider adapter) and that the
//! runtime is handed only a `RedactedString` (D6/D9). The `#[ignore]` test does a
//! real model call when `ANTHROPIC_API_KEY` is set.

use std::collections::HashMap;

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::resolve_inference;
use awaken_credential_vault::{
    CredentialBinding, CredentialCreateParams, CredentialKind, CredentialSource,
    CredentialSourceId, InMemorySecretStore, create_source,
};
use awaken_model_catalog::{
    ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
    ProviderId,
};

fn catalog(model_id: &str, base_url: &str) -> ProviderCatalog {
    let mut c = ProviderCatalog::default();
    c.providers.insert(
        "anthropic".into(),
        Provider {
            id: ProviderId::new("anthropic"),
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
            version: 1,
        },
    );
    c.endpoints.insert(
        "ep1".into(),
        ProtocolEndpoint {
            id: ProtocolEndpointId::new("ep1"),
            provider_id: ProviderId::new("anthropic"),
            dialect: ApiDialect::AnthropicMessages,
            base_url: Some(base_url.into()),
            timeout_secs: 300,
            display_name: "prod".into(),
            version: 1,
        },
    );
    c.offerings.push(Offering {
        model_id: model_id.into(),
        provider_id: ProviderId::new("anthropic"),
        protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
        dialect: ApiDialect::AnthropicMessages,
        upstream_model: None,
    });
    c
}

async fn config_and_resolve(
    model_id: &str,
    base_url: &str,
    secret: &str,
) -> (
    awaken_config_resolver::ResolvedInference,
    InMemorySecretStore,
) {
    let store = InMemorySecretStore::new();
    let source = create_source(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new(secret)),
            oauth_command: None,
        },
        &store,
    )
    .await
    .unwrap();
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(source.id.0.clone(), source.clone());

    let resolved = resolve_inference(
        &catalog(model_id, base_url),
        model_id,
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(source.id.0.clone()),
        },
        &sources,
        &store,
    )
    .await
    .unwrap();
    (resolved, store)
}

#[tokio::test]
async fn full_chain_resolves_credential_to_provider_seam() {
    let (resolved, _store) = config_and_resolve(
        "claude-opus-4-8",
        "https://api.anthropic.com/v1/",
        "sk-secret-123",
    )
    .await;

    // The resolved binding is correct and points the adapter at the endpoint.
    assert_eq!(resolved.triple.model_id, "claude-opus-4-8");
    assert_eq!(resolved.triple.provider_id, "anthropic");
    assert_eq!(resolved.adapter_kind, "anthropic");
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("https://api.anthropic.com/v1/")
    );

    // The resolved credential — an already-resolved RedactedString — is what
    // reaches the provider adapter's injection seam (D6/D9: no handle/resolver).
    let credential = resolved.credential.as_ref().expect("resolved credential");
    let _executor = awaken_provider_genai::GenaiExecutor::anthropic_compatible(
        resolved.base_url.clone().unwrap(),
        credential.expose_secret(),
    );
    // Construction with the exposed secret is the seam; the plaintext exists only
    // here and is zeroized on drop.
    assert_eq!(credential.expose_secret(), "sk-secret-123");
}

#[tokio::test]
#[ignore = "requires network and ANTHROPIC_API_KEY"]
async fn full_chain_live_model_call() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
    let model = std::env::var("AWAKEN_ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".into());

    // config → resolve: the operator-entered credential is materialized here.
    let (resolved, _store) =
        config_and_resolve(&model, "https://api.anthropic.com/v1/", &api_key).await;

    // Injection seam: hand the runtime adapter the resolved base_url + secret.
    let executor = awaken_provider_genai::GenaiExecutor::anthropic_compatible(
        resolved.base_url.clone().unwrap(),
        resolved.credential.as_ref().unwrap().expose_secret(),
    );

    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Role;
    use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, LlmExecutor};
    use awaken_runtime_contract::resolved::ModelBinding;

    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: resolved.triple.provider_id.clone(),
            model_ref: resolved.triple.model_id.clone(),
            backend_ref: "genai".to_string(),
        },
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::text("Reply with the single word: pong")],
        }],
        tools: Vec::new(),
    };

    let response = executor.infer(request).await.expect("live inference");
    assert!(
        !response.output.text_content().is_empty(),
        "expected a model reply"
    );
}
