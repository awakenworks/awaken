// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The process composition maps one authenticated Cloud catalog
// @formal reason: observation onto Control's existing model-catalog port.
//! Control-owned projection of the shared Cloud catalog transport.

use std::sync::Arc;

use awaken_admin_config_api::BrokeredCatalogDiscovery;
use awaken_credential_materializer::brokered_inference::{
    BrokeredModelCatalogClient, HttpBrokeredInferenceClient, fetch_brokered_catalog,
};
use awaken_lease_contract::{WallClock, WallClockExt};
use awaken_model_catalog::{ApiDialect, BrokeredCatalogProjection, BrokeredModelProjection};

/// The one Control port adapter for the process's authenticated Cloud client.
/// The transport remains shared with inference; Control alone names catalog
/// dialects and samples the successful observation on its process clock.
pub(super) struct BrokeredCatalogAdapter {
    client: Arc<HttpBrokeredInferenceClient>,
    clock: Arc<dyn WallClock>,
}

impl BrokeredCatalogAdapter {
    pub(super) fn new(client: Arc<HttpBrokeredInferenceClient>, clock: Arc<dyn WallClock>) -> Self {
        Self { client, clock }
    }
}

#[async_trait::async_trait]
impl BrokeredCatalogDiscovery for BrokeredCatalogAdapter {
    async fn projection(&self) -> Result<BrokeredCatalogProjection, String> {
        project_catalog(
            self.client.as_ref(),
            self.client.base_url(),
            self.clock.as_ref(),
        )
        .await
    }
}

async fn project_catalog(
    client: &dyn BrokeredModelCatalogClient,
    base_url: &str,
    clock: &dyn WallClock,
) -> Result<BrokeredCatalogProjection, String> {
    let models = fetch_brokered_catalog(client)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|model| {
            let dialect = match model.native_protocol.as_str() {
                "anthropic_messages" => ApiDialect::AnthropicMessages,
                "openai_chat_completions" => ApiDialect::OpenAiChat,
                "openai_responses" => ApiDialect::OpenAiResponses,
                protocol => {
                    return Err(format!(
                        "Cloud returned unsupported native protocol `{protocol}`"
                    ));
                }
            };
            Ok(BrokeredModelProjection {
                provider_id: model.provider,
                model_id: model.original_model_id,
                dialect,
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                publication_revision: model.route_publication_revision,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(BrokeredCatalogProjection {
        broker_id: "awaken-cloud".into(),
        control_base_url: base_url.into(),
        models,
        observed_at_unix_ms: clock.now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_materializer::brokered_inference::{
        BrokeredInferenceError, BrokeredModel,
    };
    use awaken_lease_testkit::ManualClock;
    use std::collections::BTreeSet;

    struct Catalog(Vec<BrokeredModel>);

    #[async_trait::async_trait]
    impl BrokeredModelCatalogClient for Catalog {
        async fn readiness(&self) -> Result<(), BrokeredInferenceError> {
            Ok(())
        }

        async fn list_models(&self) -> Result<Vec<BrokeredModel>, BrokeredInferenceError> {
            Ok(self.0.clone())
        }
    }

    fn model(native_protocol: &str, revision: u64) -> BrokeredModel {
        BrokeredModel {
            provider: "provider".into(),
            original_model_id: native_protocol.into(),
            native_protocol: native_protocol.into(),
            context_window: Some(32_000),
            max_output_tokens: Some(1_024),
            capabilities: BTreeSet::new(),
            route_publication_revision: revision,
        }
    }

    #[tokio::test]
    async fn supported_dialects_keep_exact_revisions_and_use_the_control_clock() {
        // Causes: C1 all three supported Cloud dialects with exact route
        // revisions; C2 an identical catalog read after the Control clock
        // advances. Effects: E1 every dialect and revision survives mapping;
        // E2 only observation time advances. Rows R1=C1 at t=1,000=>E1,
        // R2=C1+C2 at t=2,000=>E1+E2; no constructor/ambient time enters.
        let source = Catalog(vec![
            model("anthropic_messages", 1),
            model("openai_chat_completions", 2),
            model("openai_responses", 3),
        ]);
        let clock = ManualClock::new(1_000);
        let first = project_catalog(&source, "https://cloud.example", &clock)
            .await
            .unwrap();
        assert_eq!(first.observed_at_unix_ms, 1_000, "R1");
        assert_eq!(first.control_base_url, "https://cloud.example", "R1");
        assert_eq!(
            first
                .models
                .iter()
                .map(|model| (model.dialect, model.publication_revision))
                .collect::<Vec<_>>(),
            vec![
                (ApiDialect::AnthropicMessages, 1),
                (ApiDialect::OpenAiChat, 2),
                (ApiDialect::OpenAiResponses, 3),
            ],
            "R1"
        );
        clock.set(2_000);
        let second = project_catalog(&source, "https://cloud.example", &clock)
            .await
            .unwrap();
        assert_eq!(second.models, first.models, "R2");
        assert_eq!(second.observed_at_unix_ms, 2_000, "R2");
    }

    #[tokio::test]
    async fn unsupported_protocol_fails_before_a_catalog_observation_exists() {
        // Cause C3 Cloud names an unsupported protocol. Effect E3 the Control
        // adapter rejects the whole snapshot, rather than publishing a subset
        // or assigning an invented dialect. Decision row R3=C3=>E3.
        let clock = ManualClock::new(1_000);
        let error = project_catalog(
            &Catalog(vec![model("unknown", 9)]),
            "https://cloud.example",
            &clock,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("unsupported native protocol `unknown`"),
            "R3"
        );
    }
}
