//! End-to-end acceptance (ADR-0043) through the **production** management-plane
//! APIs — not hand-built structs. Mirrors the spec's full chain:
//!
//!   admin config (CatalogRepo) → 录入 credential (enter_credential) →
//!   Managed 建 agent (model string → decode_model_axis) →
//!   resolve_run → InferenceTriple + resolved credential → provider seam.
//!
//! The hermetic test proves the whole chain and the secret-free property; the
//! `#[ignore]` test makes the real model call when `ANTHROPIC_API_KEY` is set.

use std::collections::HashMap;

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::resolve_run;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialBinding, CredentialCreateParams, CredentialKind, CredentialSource,
    CredentialSourceId, InMemorySecretStore,
};
use awaken_managed_bridge::decode_model_axis;
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{
    ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// Admin config: register provider + endpoint + offering through the real repo.
async fn seed_catalog(base_url: &str, model_id: &str) -> InMemoryCatalogRepo {
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
        dialect: ApiDialect::AnthropicMessages,
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
        dialect: ApiDialect::AnthropicMessages,
        upstream_model: None,
    })
    .await
    .unwrap();
    repo
}

/// Agent-config compile stand-in: a snapshot whose model_binding pins `model_id`.
fn snapshot_for(model_id: &str) -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("snap".into()),
        root_agent_id: AgentId("agent".into()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: CatalogFingerprint("fp".into()),
            instructions: String::new(),
            max_steps: 8,
            model_binding: ModelBinding {
                provider_identity_ref: "anthropic".into(),
                model_ref: model_id.into(),
                backend_ref: "genai".into(),
            },
            tool_descriptors: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: Default::default(),
            tool_presentation: Default::default(),
        },
        fingerprint: CatalogFingerprint("fp".into()),
    }
}

async fn run_full_chain(
    base_url: &str,
    model_id: &str,
    secret: &str,
) -> awaken_config_resolver::RunInput {
    // 1. admin config: provider + endpoint + offering.
    let catalog_repo = seed_catalog(base_url, model_id).await;

    // 2. 录入 credential (secret sealed in the store, secret-free row persisted).
    let store = InMemorySecretStore::new();
    let cred_repo = InMemoryCredentialRepo::new();
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new(secret)),
            oauth_command: None,
        },
        &store,
        &cred_repo,
    )
    .await
    .unwrap();

    // 3. Managed 建 agent: the public `model` string is decoded to a model ref.
    let model_ref = decode_model_axis(Some(model_id), None).unwrap();
    assert_eq!(model_ref.model_id, model_id);
    let snapshot = snapshot_for(&model_ref.model_id);

    // 4. resolve: query the catalog + credential stores (single direction).
    let catalog = catalog_repo.snapshot().await.unwrap();
    let row = cred_repo.get(&source.id).await.unwrap();
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(row.id.0.clone(), row);

    resolve_run(
        snapshot,
        &catalog,
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
async fn managed_full_chain_resolves_secret_free_snapshot_and_credential() {
    let run = run_full_chain(
        "https://api.anthropic.com/v1/",
        "claude-opus-4-8",
        "sk-e2e-secret",
    )
    .await;

    // Resolved triple points at the configured provider/endpoint/dialect.
    assert_eq!(run.inference.triple.provider_id, "anthropic");
    assert_eq!(run.inference.triple.model_id, "claude-opus-4-8");
    assert_eq!(run.inference.adapter_kind, "anthropic");
    assert_eq!(
        run.inference.base_url.as_deref(),
        Some("https://api.anthropic.com/v1/")
    );

    // The persisted snapshot is secret-free; the credential rides in the inference half.
    assert!(
        !serde_json::to_string(&run.snapshot)
            .unwrap()
            .contains("sk-e2e-secret")
    );
    assert_eq!(
        run.inference.credential.as_ref().unwrap().expose_secret(),
        "sk-e2e-secret"
    );
}

#[tokio::test]
#[ignore = "requires network and ANTHROPIC_API_KEY"]
async fn managed_full_chain_calls_real_model() {
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ChatRole, LlmExecutor};
    use awaken_runtime_contract::resolved::ModelBinding;

    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
    let model = std::env::var("AWAKEN_ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".into());

    let run = run_full_chain("https://api.anthropic.com/v1/", &model, &api_key).await;

    // Injection seam: the resolved base_url + secret build the provider adapter.
    let executor = awaken_provider_genai::GenaiExecutor::anthropic_compatible(
        run.inference.base_url.clone().unwrap(),
        run.inference.credential.as_ref().unwrap().expose_secret(),
    );
    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: run.inference.triple.provider_id.clone(),
            model_ref: run.inference.triple.model_id.clone(),
            backend_ref: "genai".into(),
        },
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text("Reply with the single word: pong")],
        }],
        tools: Vec::new(),
    };
    let response = executor.infer(request).await.expect("live inference");
    assert!(!response.output.text_content().is_empty());
}
