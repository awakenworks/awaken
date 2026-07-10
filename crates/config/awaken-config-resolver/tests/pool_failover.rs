//! `OneOfCredentialPool` selection + failover (ADR-0043 / oversight-next account
//! grouping): the resolver walks a pool's selection order and returns the first
//! member it can materialize, skipping disabled members, absent sources, and
//! sources that fail to materialize — and fails closed only when none work.

use std::collections::HashMap;

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{
    InferenceProfile, ResolveError, SourceLookup, resolve_inference, resolve_profile,
};
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialBinding, CredentialCreateParams, CredentialKind, CredentialPool, CredentialPoolId,
    CredentialPoolMember, CredentialSource, CredentialSourceId, CredentialStatus,
    InMemorySecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{
    ModelApiCompat, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
    ProviderId,
};

/// A lookup exposing both individual sources and one pool.
struct PoolCtx {
    sources: HashMap<String, CredentialSource>,
    pool: CredentialPool,
}

impl SourceLookup for PoolCtx {
    fn get(&self, id: &str) -> Option<&CredentialSource> {
        self.sources.get(id)
    }
    fn get_pool(&self, id: &str) -> Option<&CredentialPool> {
        (self.pool.id.0 == id).then_some(&self.pool)
    }
}

async fn catalog() -> ProviderCatalog {
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
        base_url: Some("https://api.anthropic.com/v1/".into()),
        timeout_secs: 300,
        display_name: "prod".into(),
        version: 1,
    })
    .await
    .unwrap();
    repo.put_offering(Offering {
        model_id: "claude-opus-4-8".into(),
        provider_id: ProviderId::new("anthropic"),
        protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
        flavor: ModelApiCompat::AnthropicMessages,
        upstream_model: None,
    })
    .await
    .unwrap();
    repo.snapshot().await.unwrap()
}

fn disabled_source(id: &str) -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId(id.into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: None,
        env_key: None,
        material_ref: None,
        oauth_command: None,
        status: CredentialStatus::Disabled,
        version: 1,
    }
}

async fn enter(store: &InMemorySecretStore, repo: &InMemoryCredentialRepo, secret: &str) -> String {
    let src = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new(secret)),
        },
        store,
        repo,
    )
    .await
    .unwrap();
    src.id.0
}

#[tokio::test]
async fn pool_skips_disabled_absent_and_unhealthy_then_uses_the_first_good_member() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let good = enter(&store, &repo, "sk-good").await;

    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(
        good.clone(),
        repo.get(&CredentialSourceId(good.clone())).await.unwrap(),
    );
    // A source that exists but is Disabled -> materialize fails, must fail over.
    sources.insert("src_disabled".into(), disabled_source("src_disabled"));

    let pool = CredentialPool {
        id: CredentialPoolId("pool1".into()),
        workspace_id: "ws".into(),
        members: vec![
            // ordinal 0 but disabled *in the pool* -> skipped before lookup.
            CredentialPoolMember {
                credential_source_id: CredentialSourceId(good.clone()),
                ordinal: 0,
                enabled: false,
                selection_weight: 0,
            },
            // ordinal 1, absent source -> skipped.
            CredentialPoolMember {
                credential_source_id: CredentialSourceId("src_missing".into()),
                ordinal: 1,
                enabled: true,
                selection_weight: 0,
            },
            // ordinal 2, present but Disabled -> materialize fails, skipped.
            CredentialPoolMember {
                credential_source_id: CredentialSourceId("src_disabled".into()),
                ordinal: 2,
                enabled: true,
                selection_weight: 0,
            },
            // ordinal 3, the healthy source -> chosen.
            CredentialPoolMember {
                credential_source_id: CredentialSourceId(good.clone()),
                ordinal: 3,
                enabled: true,
                selection_weight: 0,
            },
        ],
    };
    let ctx = PoolCtx { sources, pool };

    let resolved = resolve_inference(
        &catalog().await,
        "claude-opus-4-8",
        &CredentialBinding::OneOfCredentialPool {
            credential_pool_id: CredentialPoolId("pool1".into()),
        },
        &ctx,
        &store,
    )
    .await
    .expect("pool resolves to the first healthy member");
    assert_eq!(
        resolved.credential.as_ref().unwrap().expose_secret(),
        "sk-good"
    );
}

#[tokio::test]
async fn pool_with_no_usable_member_fails_closed() {
    let store = InMemorySecretStore::new();
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert("src_disabled".into(), disabled_source("src_disabled"));

    let pool = CredentialPool {
        id: CredentialPoolId("pool1".into()),
        workspace_id: "ws".into(),
        members: vec![CredentialPoolMember {
            credential_source_id: CredentialSourceId("src_disabled".into()),
            ordinal: 0,
            enabled: true,
            selection_weight: 0,
        }],
    };
    let ctx = PoolCtx { sources, pool };

    let err = resolve_inference(
        &catalog().await,
        "claude-opus-4-8",
        &CredentialBinding::OneOfCredentialPool {
            credential_pool_id: CredentialPoolId("pool1".into()),
        },
        &ctx,
        &store,
    )
    .await
    .expect_err("no usable member");
    assert!(matches!(err, ResolveError::PoolExhausted(_)));
}

/// A catalog with two endpoints (primary `ep1`, backup `ep2`) both offering the
/// same model, so a profile can toggle one off and select the other.
async fn dual_endpoint_catalog() -> ProviderCatalog {
    let repo = InMemoryCatalogRepo::new();
    repo.put_provider(Provider {
        id: ProviderId::new("anthropic"),
        slug: "anthropic".into(),
        display_name: "Anthropic".into(),
        version: 1,
    })
    .await
    .unwrap();
    for (ep, url) in [
        ("ep1", "https://primary/v1/"),
        ("ep2", "https://backup/v1/"),
    ] {
        repo.put_endpoint(ProtocolEndpoint {
            id: ProtocolEndpointId::new(ep),
            provider_id: ProviderId::new("anthropic"),
            flavor: ModelApiCompat::AnthropicMessages,
            base_url: Some(url.into()),
            timeout_secs: 300,
            display_name: ep.into(),
            version: 1,
        })
        .await
        .unwrap();
        repo.put_offering(Offering {
            model_id: "claude-opus-4-8".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new(ep),
            flavor: ModelApiCompat::AnthropicMessages,
            upstream_model: None,
        })
        .await
        .unwrap();
    }
    repo.snapshot().await.unwrap()
}

#[tokio::test]
async fn profile_skips_a_disabled_endpoint_and_selects_the_next() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let good = enter(&store, &repo, "sk-good").await;
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(
        good.clone(),
        repo.get(&CredentialSourceId(good.clone())).await.unwrap(),
    );
    let ctx = PoolCtx {
        sources,
        pool: CredentialPool {
            id: CredentialPoolId("unused".into()),
            workspace_id: "ws".into(),
            members: vec![],
        },
    };

    let profile = InferenceProfile {
        model_id: "claude-opus-4-8".into(),
        credential_binding: CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(good.clone()),
        },
        disabled_endpoint_ids: vec!["ep1".into()],
    };
    let resolved = resolve_profile(&dual_endpoint_catalog().await, &profile, &ctx, &store)
        .await
        .expect("profile resolves via the enabled endpoint");
    // ep1 is toggled off, so the backup ep2 is selected.
    assert_eq!(resolved.triple.protocol_endpoint_id, "ep2");
    assert_eq!(resolved.base_url.as_deref(), Some("https://backup/v1/"));
}

#[tokio::test]
async fn missing_pool_fails_closed() {
    let store = InMemorySecretStore::new();
    let ctx = PoolCtx {
        sources: HashMap::new(),
        pool: CredentialPool {
            id: CredentialPoolId("other".into()),
            workspace_id: "ws".into(),
            members: vec![],
        },
    };
    let err = resolve_inference(
        &catalog().await,
        "claude-opus-4-8",
        &CredentialBinding::OneOfCredentialPool {
            credential_pool_id: CredentialPoolId("pool1".into()),
        },
        &ctx,
        &store,
    )
    .await
    .expect_err("unknown pool");
    assert!(matches!(err, ResolveError::PoolMissing(_)));
}
