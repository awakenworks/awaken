//! Worker-side realization of a publication-pinned model candidate.
//!
//! Configuration publication is covered in the configuration/server bounded
//! context. These tests deliberately start with a complete, secret-free candidate:
//! the worker may open only its exact persisted credential pin and cannot import
//! authoring stores or re-resolve provider/catalog facts.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialCreateParams, CredentialKind, CredentialSourceId, CredentialStatus,
    InMemorySecretStore,
};
use awaken_runtime_contract::resolved::ResolvedModelCandidate;
use awaken_runtime_contract::{
    CredentialAccess, CredentialInjectionKind, CredentialRef, CredentialUsage,
    ExecutableAgentSnapshot, InferenceEndpoint, ModelBinding, RunActivation,
};
use awaken_server::InferenceExecutorMaterializer;
use awaken_server::inference_materializer::CredentialInferenceMaterializer;

struct TestServices {
    materializer: CredentialInferenceMaterializer,
    credentials: Arc<InMemoryCredentialRepo>,
    candidate: ResolvedModelCandidate,
}

async fn services() -> TestServices {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: None,
            secret: Some(RedactedString::new("sk-test-fake")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .expect("persist test credential");
    let row = credentials.get(&source.id).await.expect("credential row");
    let revision = u64::try_from(row.version).expect("non-negative revision");
    let candidate = ResolvedModelCandidate::provider(
        ModelBinding::new("anthropic", "claude-x", "genai"),
        "anthropic@1",
        "ep1@1",
        "ws",
        Some(CredentialAccess {
            credential: CredentialRef {
                id: source.id.0,
                revision,
            },
            injection: CredentialInjectionKind::Reference,
            usage: CredentialUsage::ProviderAdapter,
        }),
        InferenceEndpoint {
            adapter_kind: "anthropic".into(),
            base_url: "https://api.anthropic.com/v1/".into(),
            upstream_model: "claude-x".into(),
        },
    );
    TestServices {
        materializer: CredentialInferenceMaterializer::new(credentials.clone(), secrets),
        credentials,
        candidate,
    }
}

fn activation(candidate: ResolvedModelCandidate) -> RunActivation {
    let mut snapshot = ExecutableAgentSnapshot::builder("snapshot")
        .model(candidate.binding.clone())
        .build();
    snapshot.resolved_spec.model_binding = candidate;
    RunActivation::new(
        RunId("run".into()),
        ThreadId("thread".into()),
        snapshot,
        Vec::new(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_materializes_the_exact_published_candidate() {
    let services = services().await;
    let activation = activation(services.candidate);
    assert!(services.materializer.materialize(&activation).is_some());
}

#[tokio::test]
async fn missing_published_credential_fails_closed() {
    let services = services().await;
    let mut candidate = services.candidate;
    let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
        credential: Some(access),
        ..
    } = &mut candidate.provisioning
    else {
        panic!("provider candidate has a credential pin")
    };
    access.credential.id = "missing".into();
    assert!(
        services
            .materializer
            .materialize_candidate(&candidate)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn revoked_published_credential_fails_closed() {
    let services = services().await;
    let credential_id = match &services.candidate.provisioning {
        awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(access),
            ..
        } => CredentialSourceId(access.credential.id.clone()),
        _ => panic!("provider candidate has a credential pin"),
    };
    let mut row = services
        .credentials
        .get(&credential_id)
        .await
        .expect("credential row");
    row.status = CredentialStatus::Disabled;
    services
        .credentials
        .put(row)
        .await
        .expect("disable credential");
    assert!(
        services
            .materializer
            .materialize_candidate(&services.candidate)
            .await
            .is_none()
    );
}
