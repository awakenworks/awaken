//! Conformance suite for [`CatalogRepo`]: the same behavioral assertions run
//! against the in-memory repo and (feature `sqlite`) the sqlite repo, so both
//! backends keep identical semantics; plus sqlite-only reopen-from-file tests.

use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo, RepoError};
use awaken_model_catalog::{
    ModelApiCompat, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};

fn provider(id: &str) -> Provider {
    Provider {
        id: ProviderId::new(id),
        slug: id.into(),
        display_name: id.into(),
        version: 1,
    }
}

fn endpoint(id: &str, provider: &str, flavor: ModelApiCompat) -> ProtocolEndpoint {
    ProtocolEndpoint {
        id: ProtocolEndpointId::new(id),
        provider_id: ProviderId::new(provider),
        flavor,
        base_url: None,
        timeout_secs: 300,
        display_name: id.into(),
        version: 1,
    }
}

fn offering(model: &str, ep: &str, flavor: ModelApiCompat) -> Offering {
    Offering {
        model_id: model.into(),
        provider_id: ProviderId::new("anthropic"),
        protocol_endpoint_id: ProtocolEndpointId::new(ep),
        flavor,
        upstream_model: None,
    }
}

async fn crud_round_trip_and_snapshot(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    repo.put_endpoint(endpoint(
        "ep1",
        "anthropic",
        ModelApiCompat::AnthropicMessages,
    ))
    .await
    .unwrap();
    repo.put_offering(offering(
        "claude-opus-4-8",
        "ep1",
        ModelApiCompat::AnthropicMessages,
    ))
    .await
    .unwrap();

    assert_eq!(
        repo.get_provider(&ProviderId::new("anthropic"))
            .await
            .unwrap()
            .slug,
        "anthropic"
    );
    assert_eq!(
        repo.get_endpoint(&ProtocolEndpointId::new("ep1"))
            .await
            .unwrap()
            .timeout_secs,
        300
    );
    let snap = repo.snapshot().await.unwrap();
    assert_eq!(snap.providers.len(), 1);
    assert_eq!(snap.endpoints.len(), 1);
    assert_eq!(snap.offerings.len(), 1);
    assert!(
        snap.resolve_offering("claude-opus-4-8", ModelApiCompat::AnthropicMessages)
            .is_some()
    );
}

async fn missing_rows_are_not_found(repo: &dyn CatalogRepo) {
    assert!(matches!(
        repo.get_provider(&ProviderId::new("ghost")).await,
        Err(RepoError::ProviderNotFound(id)) if id == "ghost"
    ));
    assert!(matches!(
        repo.get_endpoint(&ProtocolEndpointId::new("ghost")).await,
        Err(RepoError::EndpointNotFound(id)) if id == "ghost"
    ));
}

async fn endpoint_needs_existing_provider(repo: &dyn CatalogRepo) {
    assert!(matches!(
        repo.put_endpoint(endpoint("ep1", "ghost", ModelApiCompat::OpenAiChat))
            .await,
        Err(RepoError::ProviderNotFound(id)) if id == "ghost"
    ));
}

async fn offering_needs_existing_endpoint(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    assert!(matches!(
        repo.put_offering(offering("m", "ghost", ModelApiCompat::AnthropicMessages))
            .await,
        Err(RepoError::EndpointNotFound(id)) if id == "ghost"
    ));
}

async fn rejected_offering_leaves_no_trace(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    repo.put_endpoint(endpoint(
        "ep1",
        "anthropic",
        ModelApiCompat::AnthropicMessages,
    ))
    .await
    .unwrap();
    // Flavor mismatch with the endpoint → fail-closed…
    let bad = offering("m", "ep1", ModelApiCompat::OpenAiChat);
    assert!(matches!(
        repo.put_offering(bad).await,
        Err(RepoError::Invariant(_))
    ));
    // …and the rejected write is fully rolled back: later snapshots stay valid.
    let snap = repo.snapshot().await.unwrap();
    assert_eq!(snap.offerings.len(), 0);
}

async fn put_is_upsert(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    let mut v2 = provider("anthropic");
    v2.display_name = "Anthropic".into();
    v2.version = 2;
    repo.put_provider(v2).await.unwrap();
    let got = repo
        .get_provider(&ProviderId::new("anthropic"))
        .await
        .unwrap();
    assert_eq!(got.display_name, "Anthropic");
    assert_eq!(got.version, 2);
    assert_eq!(repo.snapshot().await.unwrap().providers.len(), 1);

    repo.put_endpoint(endpoint(
        "ep1",
        "anthropic",
        ModelApiCompat::AnthropicMessages,
    ))
    .await
    .unwrap();
    let mut ep2 = endpoint("ep1", "anthropic", ModelApiCompat::AnthropicMessages);
    ep2.timeout_secs = 60;
    ep2.version = 2;
    repo.put_endpoint(ep2).await.unwrap();
    let got = repo
        .get_endpoint(&ProtocolEndpointId::new("ep1"))
        .await
        .unwrap();
    assert_eq!(got.timeout_secs, 60);
    assert_eq!(repo.snapshot().await.unwrap().endpoints.len(), 1);
}

/// Run every suite, each on a fresh repo from `make`.
async fn run_all(make: impl Fn() -> Box<dyn CatalogRepo>) {
    crud_round_trip_and_snapshot(&*make()).await;
    missing_rows_are_not_found(&*make()).await;
    endpoint_needs_existing_provider(&*make()).await;
    offering_needs_existing_endpoint(&*make()).await;
    rejected_offering_leaves_no_trace(&*make()).await;
    put_is_upsert(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    run_all(|| Box::new(InMemoryCatalogRepo::new())).await;
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use awaken_model_catalog::sqlite::SqliteCatalogRepo;

    #[tokio::test]
    async fn sqlite_repo_conforms() {
        run_all(|| Box::new(SqliteCatalogRepo::open_in_memory().unwrap())).await;
    }

    #[tokio::test]
    async fn sqlite_rows_survive_a_reopen_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let path = path.to_str().unwrap();
        {
            let repo = SqliteCatalogRepo::open(path).unwrap();
            repo.put_provider(provider("anthropic")).await.unwrap();
            repo.put_endpoint(endpoint(
                "ep1",
                "anthropic",
                ModelApiCompat::AnthropicMessages,
            ))
            .await
            .unwrap();
            repo.put_offering(offering(
                "claude-opus-4-8",
                "ep1",
                ModelApiCompat::AnthropicMessages,
            ))
            .await
            .unwrap();
        }
        // A fresh handle on the same file sees the identical validated catalog.
        let repo = SqliteCatalogRepo::open(path).unwrap();
        let snap = repo.snapshot().await.unwrap();
        assert_eq!(snap.providers.len(), 1);
        assert_eq!(snap.endpoints.len(), 1);
        assert_eq!(snap.offerings.len(), 1);
        assert_eq!(
            repo.get_provider(&ProviderId::new("anthropic"))
                .await
                .unwrap()
                .slug,
            "anthropic"
        );
    }
}
