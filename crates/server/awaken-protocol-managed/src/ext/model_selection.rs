//! The awaken model/runtime selection extension.
//!
//! Claude's Managed Agents wire binds a session to an agent, not to a model or a
//! runtime — those are our concern. Rather than add non-SDK fields to the native
//! [`CreateSessionRequest`] (which would break 1:1 compatibility), the selection
//! travels in the Claude-native `metadata` bag under reserved `awaken.*` keys. This
//! module owns those keys and the accessor that reads them, keeping `types` a pure
//! projection of the SDK shapes.

use crate::types::CreateSessionRequest;

/// Metadata key for the awaken per-session model override (R2), carried on the
/// Claude-native `metadata` bag rather than the SDK `agent` object.
pub const AWAKEN_MODEL_META_KEY: &str = "awaken.model";
/// Metadata key for the awaken runtime selection (R3): `"awaken"` / `"acp:<cli>"`.
pub const AWAKEN_RUNTIME_META_KEY: &str = "awaken.runtime";

/// Reads the awaken selection extensions off a native create-session request. An
/// extension trait, so the wire type in `types` stays free of our vocabulary.
pub trait AwakenModelSelection {
    /// The awaken per-session model override (R2). Absent → the host default model.
    fn awaken_model(&self) -> Option<&str>;
    /// The awaken runtime selection (R3): `"awaken"` (native) or `"acp:<cli>"`.
    /// Absent → native.
    fn awaken_runtime(&self) -> Option<&str>;
}

impl AwakenModelSelection for CreateSessionRequest {
    fn awaken_model(&self) -> Option<&str> {
        self.metadata.get(AWAKEN_MODEL_META_KEY).map(String::as_str)
    }

    fn awaken_runtime(&self) -> Option<&str> {
        self.metadata
            .get(AWAKEN_RUNTIME_META_KEY)
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_request_reads_awaken_overrides_from_metadata() {
        let req: CreateSessionRequest = serde_json::from_str(
            r#"{"agent":"a","metadata":{"awaken.model":"m2","awaken.runtime":"acp:claude"}}"#,
        )
        .unwrap();
        assert_eq!(req.awaken_model(), Some("m2"));
        assert_eq!(req.awaken_runtime(), Some("acp:claude"));
        // Absent → None (host default / native).
        let bare: CreateSessionRequest = serde_json::from_str(r#"{"agent":"a"}"#).unwrap();
        assert_eq!(bare.awaken_model(), None);
        assert_eq!(bare.awaken_runtime(), None);
    }
}
