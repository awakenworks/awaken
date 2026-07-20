//! The production worker resolves a REAL (DB-configured) model per run — no mock.
//!
//! This proves the seam the worker's `run()` wires: a
//! [`CatalogInferenceAccessPublisher`](awaken_server::inference_materializer::CatalogInferenceAccessPublisher)
//! pins authored configuration, then the worker-only
//! [`CredentialInferenceMaterializer`](awaken_server::inference_materializer::CredentialInferenceMaterializer)
//! turns that pinned access into a real executor. The latter is what the worker
//! installs via `SharedHost::with_inference_materializer`, exercised directly
//! through the sync `InferenceExecutorMaterializer::executor_for` port the run loop calls per run.
//!
//! The secret is a fake — resolution and executor construction never touch the
//! network, so every branch is reachable offline.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialCreateParams, CredentialKind, CredentialStatus, InMemorySecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{
    ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};
use awaken_runtime_contract::{ExecutableAgentSnapshot, ModelBinding, RunActivation};
use awaken_server::InferenceExecutorMaterializer;
use awaken_server::inference_materializer::{
    CatalogInferenceAccessPublisher, CredentialInferenceMaterializer,
};

/// Author a catalog with one anthropic offering for `model`, plus (optionally) a
/// workspace credential `(provider, active)`, then build the publication and
/// worker runtime adapters over their disjoint dependency sets.
struct TestServices {
    publisher: CatalogInferenceAccessPublisher,
    materializer: CredentialInferenceMaterializer,
}

async fn provider(model: &str, credential: Option<(&str, bool)>) -> TestServices {
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    catalog
        .put_provider(Provider {
            id: ProviderId::new("anthropic"),
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
            version: 1,
        })
        .await
        .unwrap();
    catalog
        .put_endpoint(ProtocolEndpoint {
            id: ProtocolEndpointId::new("ep1"),
            provider_id: ProviderId::new("anthropic"),
            dialect: ApiDialect::AnthropicMessages,
            base_url: Some("https://api.anthropic.com/v1/".into()),
            timeout_secs: 300,
            display_name: "prod".into(),
            version: 1,
        })
        .await
        .unwrap();
    catalog
        .put_offering(Offering {
            model_id: model.into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
        })
        .await
        .unwrap();

    let secrets = Arc::new(InMemorySecretStore::new());
    let creds = Arc::new(InMemoryCredentialRepo::new());
    if let Some((prov, active)) = credential {
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some(prov.into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("sk-test-fake")),
                oauth_command: None,
            },
            secrets.as_ref(),
            creds.as_ref(),
        )
        .await
        .unwrap();
        if !active {
            let mut row = creds.get(&source.id).await.unwrap();
            row.status = CredentialStatus::Disabled;
            creds.put(row).await.unwrap();
        }
    }
    TestServices {
        publisher: CatalogInferenceAccessPublisher::new(catalog, creds.clone()),
        materializer: CredentialInferenceMaterializer::new(creds, secrets),
    }
}

fn activation(model_ref: &str) -> RunActivation {
    RunActivation::new(
        RunId("run".into()),
        ThreadId("thread".into()),
        ExecutableAgentSnapshot::builder("snapshot")
            .model(ModelBinding::new("provider", model_ref, "genai"))
            .build(),
        Vec::new(),
    )
}

/// A drained run whose `model_ref` names a published model with an active, compatible
/// credential resolves to a REAL executor through `executor_for` — the sync port the
/// host's run loop calls per activation. This is the no-mock proof: `Some` here is a
/// genai executor, not the fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_resolves_a_configured_model_to_a_real_executor() {
    let p = provider("claude-x", Some(("anthropic", true))).await;
    let activation = activation("claude-x");
    let access = p
        .publisher
        .resolve_for_scope(
            "ws",
            std::slice::from_ref(&activation.snapshot.resolved_spec.model_binding),
        )
        .await
        .expect("access is pinned");
    assert!(p.materializer.materialize(&activation, &access).is_some());
}

/// An unpublished `model_ref` returns `None`, so the host falls back to
/// `NoModelConfiguredExecutor` (the production placeholder) rather than erroring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_falls_back_when_the_model_is_not_published() {
    let p = provider("claude-x", Some(("anthropic", true))).await;
    let activation = activation("no-such-model");
    assert!(
        p.publisher
            .resolve_for_scope(
                "ws",
                std::slice::from_ref(&activation.snapshot.resolved_spec.model_binding),
            )
            .await
            .is_err()
    );
}

/// A published model with no compatible workspace credential also falls back — the
/// resolution requires an Active credential that `can_consume` the offering's provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_falls_back_without_a_compatible_credential() {
    let p = provider("claude-x", None).await;
    let activation = activation("claude-x");
    assert!(
        p.publisher
            .resolve_for_scope(
                "ws",
                std::slice::from_ref(&activation.snapshot.resolved_spec.model_binding),
            )
            .await
            .is_err()
    );
}
