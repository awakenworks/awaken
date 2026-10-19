// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: Original ledger CAS and fresh capture are fail closed;
// @formal reason: the cause-effect tests cover byte and replay scope, not universal physical closure.
//! Shared Create capture vocabulary and the original complete outward encoder.

use crate::RuntimeError;
use serde::Serialize;

pub const K8S_CREATE_WIRE_VERSION: u8 = 2;
pub const K8S_CREATE_CAPTURE_HEADER: &str = "x-awaken-create-capture";
const RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Transport reading policy, not effect authorization or a cleanup receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateReplyProfile {
    FreshCreate,
    WholeJoin,
    LegacyRetainedCreate,
}

impl CreateReplyProfile {
    #[must_use]
    pub fn header_value(self) -> &'static str {
        match self {
            Self::FreshCreate => "bounded-create-v1",
            Self::WholeJoin => "bounded-join-v1",
            Self::LegacyRetainedCreate => "legacy-retained-create-v1",
        }
    }

    #[must_use]
    pub fn from_header(value: &str) -> Option<Self> {
        match value {
            "bounded-create-v1" => Some(Self::FreshCreate),
            "bounded-join-v1" => Some(Self::WholeJoin),
            "legacy-retained-create-v1" => Some(Self::LegacyRetainedCreate),
            _ => None,
        }
    }

    #[must_use]
    pub fn body_limit(self) -> Option<usize> {
        match self {
            Self::FreshCreate | Self::WholeJoin => Some(RESPONSE_BYTES),
            Self::LegacyRetainedCreate => None,
        }
    }

    /// Encode the exact eventual response before committing a fresh capture.
    /// The router sends these bytes, rather than separately encoding an object.
    pub fn encode_body<T: Serialize>(self, value: &T) -> Result<Vec<u8>, RuntimeError> {
        let bytes =
            serde_json::to_vec(value).map_err(|error| RuntimeError::Backend(error.to_string()))?;
        if self.body_limit().is_some_and(|limit| bytes.len() > limit) {
            return Err(RuntimeError::Backend(
                "Create control response exceeds its complete capture policy".into(),
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_reply_range_counts_the_original_outer_response_codec() {
        // R5/R6/R10: full outgoing status/object or whole-join body, not a
        // single inner object/result. Exact B fits; +1 refuses while explicit
        // legacy encoding remains complete. No result/UID or JSON truncation.
        let overhead = serde_json::to_vec("").unwrap().len();
        for length in [RESPONSE_BYTES - 1, RESPONSE_BYTES, RESPONSE_BYTES + 1] {
            let value = "x".repeat(length - overhead);
            let bytes = CreateReplyProfile::FreshCreate.encode_body(&value);
            assert_eq!(bytes.is_ok(), length <= RESPONSE_BYTES);
            assert_eq!(
                CreateReplyProfile::LegacyRetainedCreate
                    .encode_body(&value)
                    .unwrap()
                    .len(),
                length
            );
        }
    }
}
