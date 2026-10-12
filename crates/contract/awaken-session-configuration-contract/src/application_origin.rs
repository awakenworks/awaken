//! Opaque application-origin evidence frozen by the existing Session owner.
//!
//! This value is not an authentication credential or an authorization grant.
//! Its issuing application/hosting adapter owns signature, identity, scope,
//! and current-policy verification. Awaken validates only the bounded carrier.

use std::fmt;

use serde::{Deserialize, Serialize};

const SOURCE_MAX_BYTES: usize = 128;
const EVIDENCE_MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "OriginDocument")]
pub struct ApplicationOriginBinding {
    source: String,
    context_fingerprint: String,
    evidence: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginDocument {
    source: String,
    context_fingerprint: String,
    evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationOriginBindingError {
    #[error("application origin source must be a bounded ASCII namespace")]
    InvalidSource,
    #[error("application origin context fingerprint must be lowercase SHA-256 hex")]
    InvalidContextFingerprint,
    #[error("application origin evidence must be bounded nonempty ASCII without whitespace")]
    InvalidEvidence,
}

impl ApplicationOriginBinding {
    pub fn new(
        source: String,
        context_fingerprint: String,
        evidence: String,
    ) -> Result<Self, ApplicationOriginBindingError> {
        if source.is_empty()
            || source.len() > SOURCE_MAX_BYTES
            || !source
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-:".contains(&byte))
        {
            return Err(ApplicationOriginBindingError::InvalidSource);
        }
        if context_fingerprint.len() != 64
            || !context_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ApplicationOriginBindingError::InvalidContextFingerprint);
        }
        if evidence.is_empty()
            || evidence.len() > EVIDENCE_MAX_BYTES
            || !evidence.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ApplicationOriginBindingError::InvalidEvidence);
        }
        Ok(Self {
            source,
            context_fingerprint,
            evidence,
        })
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn context_fingerprint(&self) -> &str {
        &self.context_fingerprint
    }

    /// Opaque evidence for the original issuing application, not model context.
    #[must_use]
    pub fn evidence(&self) -> &str {
        &self.evidence
    }
}

impl TryFrom<OriginDocument> for ApplicationOriginBinding {
    type Error = ApplicationOriginBindingError;

    fn try_from(document: OriginDocument) -> Result<Self, Self::Error> {
        Self::new(
            document.source,
            document.context_fingerprint,
            document.evidence,
        )
    }
}

impl fmt::Debug for ApplicationOriginBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationOriginBinding")
            .field("source", &self.source)
            .field("context_fingerprint", &self.context_fingerprint)
            .field("evidence", &"[opaque evidence omitted]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> serde_json::Value {
        serde_json::json!({
            "source": "application.interaction:v1",
            "context_fingerprint": "a".repeat(64),
            "evidence": "origin-proof.example.signature"
        })
    }

    #[test]
    fn origin_shape_decision_table_is_shared_by_constructor_and_wire() {
        // Causes: C1 source is bounded namespace ASCII; C2 fingerprint is
        // exactly 64 lowercase hex bytes; C3 evidence is bounded graphic ASCII;
        // C4 wire has exactly the three required fields. Effects: E1 accepted
        // carrier (not an identity/grant); E2 rejected before persistence.
        // R1 all=true => E1. R2-R5 flip each independent cause => E2. Boundary
        // witnesses cover min/max and max+1, empty, whitespace and non-ASCII.
        let original = document();
        let parsed: ApplicationOriginBinding =
            serde_json::from_value(original.clone()).expect("R1");
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
        assert_eq!(parsed.source(), "application.interaction:v1");
        assert_eq!(parsed.context_fingerprint(), "a".repeat(64));
        assert_eq!(parsed.evidence(), "origin-proof.example.signature");
        for (field, invalid) in [
            ("source", String::new()),
            ("source", "a".repeat(SOURCE_MAX_BYTES + 1)),
            ("source", "invalid source".into()),
            ("source", "来源".into()),
            ("context_fingerprint", "a".repeat(63)),
            ("context_fingerprint", "a".repeat(65)),
            ("context_fingerprint", "A".repeat(64)),
            ("context_fingerprint", "g".repeat(64)),
            ("evidence", String::new()),
            ("evidence", "a".repeat(EVIDENCE_MAX_BYTES + 1)),
            ("evidence", "line\nbreak".into()),
            ("evidence", "with space".into()),
            ("evidence", "证明".into()),
        ] {
            let mut candidate = original.clone();
            candidate[field] = invalid.into();
            assert!(
                serde_json::from_value::<ApplicationOriginBinding>(candidate.clone()).is_err(),
                "R2-R4 {field}"
            );
            assert!(
                ApplicationOriginBinding::new(
                    candidate["source"].as_str().unwrap().into(),
                    candidate["context_fingerprint"].as_str().unwrap().into(),
                    candidate["evidence"].as_str().unwrap().into(),
                )
                .is_err(),
                "constructor R2-R4 {field}"
            );
        }
        for field in ["source", "context_fingerprint", "evidence"] {
            let mut candidate = original.clone();
            candidate.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<ApplicationOriginBinding>(candidate).is_err(),
                "R5 missing {field}"
            );
        }
        let mut extra = original;
        extra["account_id"] = "caller-owned".into();
        assert!(
            serde_json::from_value::<ApplicationOriginBinding>(extra).is_err(),
            "R5 unknown"
        );
        for size in [1, SOURCE_MAX_BYTES] {
            assert!(
                ApplicationOriginBinding::new("a".repeat(size), "0".repeat(64), "x".into()).is_ok(),
                "R1 source boundary"
            );
        }
        for size in [1, EVIDENCE_MAX_BYTES] {
            assert!(
                ApplicationOriginBinding::new("a".into(), "0".repeat(64), "x".repeat(size)).is_ok(),
                "R1 evidence boundary"
            );
        }
    }

    #[test]
    fn debug_does_not_disclose_opaque_evidence() {
        // C1 origin evidence may contain application identities; C2 a caller
        // formats the carrier with Debug. R6 C1+C2 => evidence omitted, while
        // non-sensitive source/fingerprint remain useful for diagnosis.
        let origin: ApplicationOriginBinding = serde_json::from_value(document()).unwrap();
        let debug = format!("{origin:?}");
        assert!(!debug.contains(origin.evidence()));
        assert!(debug.contains(origin.source()));
        assert!(debug.contains("opaque evidence omitted"));
    }
}
