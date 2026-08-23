//! Canonical durable observation of one logical model request.

use serde::{Deserialize, Serialize};

use crate::audit::{kind::Kind, record::Record};

/// Neutral token counters reported by a model provider. Missing provider
/// counters remain zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

impl TokenUsage {
    /// Field-wise saturating sum.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_add(other.cache_creation_tokens),
        }
    }
}

/// One completed logical model request. Provider retries belong to this same
/// observation; continuations and model-pool failover calls produce another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelRequestObservation {
    /// Whether the logical request exhausted recovery and returned an error.
    pub is_error: bool,
    /// Per-request usage. Providers that omit usage leave all fields at zero.
    pub usage: TokenUsage,
    /// Transparent provider retries performed inside this logical request.
    #[serde(default)]
    pub retry_count: u32,
}

impl ModelRequestObservation {
    /// Decode the one canonical persisted payload. Non-model audit records are
    /// deliberately ignored so projectors can scan a mixed committed stream.
    pub fn from_record(record: &Record) -> Result<Option<Self>, serde_json::Error> {
        if record.kind != Kind::ModelRequestCompleted {
            return Ok(None);
        }
        serde_json::from_value(record.payload.clone()).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::run::Id as RunId;

    use super::*;

    #[test]
    fn decoder_accepts_the_model_kind_and_ignores_other_audit_facts() {
        // Causes: C1 a model completion record carries the canonical shape; C2
        // a mixed stream also carries another audit kind. Effects: E1 C1
        // decodes losslessly; E2 C2 is ignored. Rules: R1=C1=>E1; R2=C2=>E2.
        // Constraint/invariant: `Kind` is the discriminator; a projector must
        // never interpret another committed audit payload as model usage.
        let observation = ModelRequestObservation {
            is_error: false,
            usage: TokenUsage {
                prompt_tokens: 4,
                completion_tokens: 2,
                ..Default::default()
            },
            retry_count: 1,
        };
        let model = Record {
            sequence: 1,
            run_id: RunId("run".into()),
            kind: Kind::ModelRequestCompleted,
            payload: serde_json::to_value(observation).unwrap(),
        };
        assert_eq!(
            ModelRequestObservation::from_record(&model).unwrap(),
            Some(observation),
            "R1/E1"
        );

        let other = Record {
            kind: Kind::StateChanged,
            ..model
        };
        assert_eq!(
            ModelRequestObservation::from_record(&other).unwrap(),
            None,
            "R2/E2"
        );
    }
}
