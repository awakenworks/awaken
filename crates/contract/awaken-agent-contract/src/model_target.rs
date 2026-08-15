//! Secret-free authoring identity for selecting one model-catalog offering.

use serde::{Deserialize, Serialize};

/// Stable authoring identity used to select one catalog Offering.
///
/// `endpoint_name` is the human qualifier used by public model ids. Resolution
/// maps it to a concrete endpoint after dialect negotiation. Exact config and
/// profile authoring may instead use `protocol_endpoint_id`; admission rejects
/// a target containing both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelTarget {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Public API-dialect qualifier (for example `anthropic_messages` or
    /// `open_ai_responses`). It narrows a provider's protocol surfaces without
    /// exposing the catalog's internal endpoint identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_dialect: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_endpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_name: Option<String>,
}

impl ModelTarget {
    #[must_use]
    pub fn unqualified(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            provider_id: None,
            api_dialect: None,
            protocol_endpoint_id: None,
            endpoint_name: None,
        }
    }
}
