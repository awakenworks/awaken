//! Secret-free projection of the executable ACP catalog for Control publication.

use super::{BackendModelInterface, known_acp_clis};

/// Project the single executable catalog into the facts consumed by Control.
/// This is the only adapter-to-publication mapping.
#[must_use]
pub fn known_acp_publication_capabilities() -> Vec<awaken_acp_contract::AcpPublicationCapability> {
    known_acp_clis()
        .iter()
        .map(|cli| awaken_acp_contract::AcpPublicationCapability {
            backend_ref: format!("acp:{}", cli.id),
            model_api_dialects: cli
                .model_api_dialects
                .iter()
                .map(|dialect| (*dialect).to_string())
                .collect(),
            supports_exact_model_selection: cli.backend_model_interface
                != BackendModelInterface::Unsupported,
            model_delivery_credential_environments: cli.model_delivery.map(|delivery| {
                delivery
                    .credential_env
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect()
            }),
        })
        .collect()
}
