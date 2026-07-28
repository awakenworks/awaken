use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{
    ApiDialect, Offering, OfferingSource, OfferingStatus, ProtocolEndpoint, ProtocolEndpointId,
    Provider, ProviderId,
};

/// Seed catalog state below the HTTP adapter.
///
/// Product tests use ProviderConnection for authoring. Handler tests that focus
/// on unrelated resolve/credential behavior seed the domain repository directly,
/// so the removed provider/endpoint/offering HTTP commands cannot become a
/// hidden test-only compatibility path.
pub async fn seed_model(
    repo: &InMemoryCatalogRepo,
    provider: &str,
    dialect: &str,
    model: &str,
    endpoint: &str,
) {
    let dialect: ApiDialect =
        serde_json::from_value(serde_json::Value::String(dialect.to_string()))
            .expect("fixture dialect");
    repo.put_provider(Provider {
        id: ProviderId::new(provider),
        slug: provider.to_string(),
        display_name: provider.to_string(),
        version: 1,
    })
    .await
    .expect("seed provider");
    repo.put_endpoint(ProtocolEndpoint {
        id: ProtocolEndpointId::new(endpoint),
        provider_id: ProviderId::new(provider),
        dialect: dialect.clone(),
        base_url: Some("https://api.example.com/v1/".into()),
        timeout_secs: 300,
        display_name: "fixture".into(),
        version: 1,
    })
    .await
    .expect("seed endpoint");
    repo.put_offering(Offering {
        model_id: model.to_string(),
        provider_id: ProviderId::new(provider),
        protocol_endpoint_id: ProtocolEndpointId::new(endpoint),
        dialect,
        upstream_model: None,
        source: OfferingSource::Manual,
        status: OfferingStatus::Active,
        last_seen_at_unix_ms: None,
    })
    .await
    .expect("seed model");
}
