//! The awaken model/runtime selection extension.
//!
//! Claude's Managed Agents wire binds a session to an agent, not to a model or a
//! runtime — those are our concern. Rather than add non-SDK fields to the native
//! [`SessionCreateParams`] (which would break 1:1 compatibility), the selection
//! travels in the Claude-native `metadata` bag under reserved `awaken.*` keys. This
//! module owns that key and the accessor that reads it, keeping `types` a pure
//! projection of the SDK shapes. Execution backend selection is deliberately not
//! a Session extension: it comes only from the immutable Agent publication.

use crate::types::SessionCreateParams;

/// Metadata key for the awaken per-session model override (R2), carried on the
/// Claude-native `metadata` bag rather than the SDK `agent` object.
pub const AWAKEN_MODEL_META_KEY: &str = "awaken.model";
/// Reads the awaken selection extensions off a native create-session request. An
/// extension trait, so the wire type in `types` stays free of our vocabulary.
pub trait AwakenModelSelection {
    /// The awaken per-session model override (R2). Absent → the host default model.
    fn awaken_model(&self) -> Option<&str>;
}

impl AwakenModelSelection for SessionCreateParams {
    fn awaken_model(&self) -> Option<&str> {
        self.metadata.get(AWAKEN_MODEL_META_KEY).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_request_reads_only_the_model_override_from_metadata() {
        // Cause graph:
        // C1 `awaken.model` is present -> E1 the Session model selector reads it.
        // C2 `awaken.runtime` is present -> E2 it remains inert metadata; backend
        // authority stays in the Agent publication.
        //
        // Decision table:
        // | Rule | model key | runtime key | model result | backend effect |
        // | M1 | T | - | exact value | none |
        // | M2 | F | T | none | none |
        // | M3 | F | F | none | none |
        let req: SessionCreateParams = serde_json::from_str(
            r#"{"agent":"a","metadata":{"awaken.model":"m2","awaken.runtime":"acp:claude"}}"#,
        )
        .unwrap();
        assert_eq!(req.awaken_model(), Some("m2"), "M1");
        assert_eq!(
            req.metadata.get("awaken.runtime").map(String::as_str),
            Some("acp:claude"),
            "M2 wire metadata is retained but has no selection accessor"
        );
        // Absent → None (host default model).
        let bare: SessionCreateParams = serde_json::from_str(r#"{"agent":"a"}"#).unwrap();
        assert_eq!(bare.awaken_model(), None, "M3");
    }
}
