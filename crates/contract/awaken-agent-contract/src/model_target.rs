//! Secret-free authoring identity for selecting one model-catalog offering.

use serde::{Deserialize, Serialize};

/// Stable authoring identity used to select one catalog Offering.
///
/// `endpoint_name` is the human qualifier used by public model ids. Resolution
/// maps it to a concrete endpoint after dialect negotiation. Exact config and
/// profile authoring may instead use `protocol_endpoint_id`; admission rejects
/// a target containing both.
///
/// Every field of a target is a qualifier that narrows which Offering is
/// selected, so a field the reader does not model is not decoration: it is
/// authored intent that would be dropped, resolving to a *different* model than
/// the author asked for. `deny_unknown_fields` reports it instead — `endpoint`
/// written for `endpoint_name`, or `dialect` for `api_dialect`, is a rejected
/// document, never a target that quietly ignores the qualifier.
///
/// This is the hazard the wrapping `ModelSelectionWire` already documents for
/// its own variants. Serde's `deny_unknown_fields` does **not** recurse into a
/// field's type, so that wrapper never protected this struct; the guard has to
/// be stated on every wire type that models authored input, including this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Authored-qualifier admission. Causes: C1 every key is modelled; C2 a key
    /// is near-miss authoring of a modelled qualifier (`endpoint` for
    /// `endpoint_name`); C3 a key is unrelated. Effects: E1 accept with the
    /// qualifier bound; E2 reject naming the offending key.
    /// Rules: R1 = C1 => E1; R2 = C2 => E2; R3 = C3 => E2.
    ///
    /// R2 is the live one: dropping `endpoint` silently leaves
    /// `endpoint_name: None`, and resolution then picks the provider's default
    /// endpoint instead of the one the author named — a different model, with
    /// a 200 and no diagnostic anywhere.
    #[test]
    fn an_unmodelled_qualifier_is_rejected_not_dropped() {
        let accepted: ModelTarget = serde_json::from_str(
            r#"{"model_id":"gpt-5","provider_id":"openai",
                "api_dialect":"open_ai_responses","endpoint_name":"eu-primary"}"#,
        )
        .expect("R1: every key is modelled");
        assert_eq!(accepted.endpoint_name.as_deref(), Some("eu-primary"), "R1");

        for (rule, body) in [
            (
                "R2",
                r#"{"model_id":"gpt-5","provider_id":"openai",
                    "api_dialect":"open_ai_responses","endpoint":"eu-primary"}"#,
            ),
            ("R3", r#"{"model_id":"gpt-5","region":"eu"}"#),
        ] {
            let error = serde_json::from_str::<ModelTarget>(body)
                .expect_err("an unmodelled qualifier must be reported");
            assert!(
                error.to_string().contains("unknown field"),
                "{rule}: the rejection must name the unknown field, got `{error}`"
            );
        }
    }

    /// A target round-trips: `deny_unknown_fields` must not reject this type's
    /// own serialized form (the shape every store and API response emits).
    #[test]
    fn a_serialized_target_deserializes_back() {
        let target = ModelTarget {
            protocol_endpoint_id: Some("ep-1".into()),
            ..ModelTarget::unqualified("claude-sonnet-4")
        };
        let wire = serde_json::to_string(&target).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ModelTarget>(&wire).expect("round-trip"),
            target
        );
    }
}
