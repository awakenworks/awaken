// @formal entrypoint memory_extraction.unknown_execution_never_reenters_retry
//! Value objects and terminal outcomes for durable Memory extraction.

use awaken_lease_contract::fence::Epoch;
use awaken_runtime_contract::CandidateFingerprint;
use serde::{Deserialize, Serialize};

/// Frozen, secret-free extractor configuration used by every retry.
///
/// Durable intents require the complete executable Agent snapshot. Partial
/// historical shapes are rejected instead of being reinterpreted at recovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExtractorSnapshot {
    /// Complete ordinary Agent publication used on every retry. It contains
    /// model/tool/plugin references only, never secret bytes.
    pub agent: awaken_runtime_contract::ExecutableAgentSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_prompt: Option<String>,
}

impl MemoryExtractorSnapshot {
    /// Construct the explicit host-executor form used by embedded compositions
    /// and persistence-adapter tests. Provider-backed publications carry their
    /// complete candidate from the executable snapshot instead. The three
    /// coordinates are admitted by [`ModelBinding::parse`], so an extractor
    /// snapshot that the durable wire would refuse cannot be built.
    ///
    /// [`ModelBinding::parse`]: awaken_runtime_contract::resolved::ModelBinding::parse
    pub fn host_executor(
        agent_id: impl Into<String>,
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Result<Self, awaken_runtime_contract::resolved::InvalidModelBinding> {
        let agent_id = agent_id.into();
        let binding = awaken_runtime_contract::resolved::ModelBinding::parse(
            provider_identity_ref,
            model_ref,
            backend_ref,
        )?;
        Ok(Self {
            agent: crate::memory_agent(
                &agent_id,
                awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding),
                crate::DEFAULT_MEMORY_INSTRUCTIONS,
            ),
            extraction_prompt: None,
        })
    }
}

/// The Memory head the extractor observed before storage, as an opaque token.
///
/// The SDK carries it and never interprets it: the store that issued the token
/// is the only party that decodes and compares it. It serializes exactly as the
/// value it wraps, so a persisted mutation keeps its wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryHeadToken(serde_json::Value);

impl MemoryHeadToken {
    /// Wrap the store's own encoding of the head it observed.
    #[must_use]
    pub fn new(encoded: serde_json::Value) -> Self {
        Self(encoded)
    }

    /// The store's encoding, for the store to decode.
    #[must_use]
    pub fn encoded(&self) -> &serde_json::Value {
        &self.0
    }
}

/// A Platform credential realization receipt carried by an extraction intent,
/// as an opaque token.
///
/// The extraction claim records which credential effects its attempt
/// completed. The receipt is Platform vocabulary (ADR-0086 §7); the SDK reads
/// only the two values it fences on, the claim epoch and the model candidate,
/// and carries the Platform's encoding unchanged otherwise. It serializes
/// exactly as the receipt it wraps, so a persisted intent keeps its wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRealizationToken {
    claim_epoch: Epoch,
    candidate_fingerprint: CandidateFingerprint,
    encoded: serde_json::Value,
}

/// The two receipt fields the SDK reads; every other field stays opaque.
#[derive(Deserialize)]
struct CredentialRealizationCoordinates {
    claim_epoch: Epoch,
    candidate_fingerprint: CandidateFingerprint,
}

impl CredentialRealizationToken {
    /// Wrap the Platform's encoding of one receipt. An encoding without a
    /// nonzero claim epoch and a candidate fingerprint is not a receipt and is
    /// refused.
    pub fn new(encoded: serde_json::Value) -> Result<Self, serde_json::Error> {
        let coordinates = CredentialRealizationCoordinates::deserialize(&encoded)?;
        Ok(Self {
            claim_epoch: coordinates.claim_epoch,
            candidate_fingerprint: coordinates.candidate_fingerprint,
            encoded,
        })
    }

    /// The extraction claim epoch the receipt was recorded under.
    #[must_use]
    pub fn claim_epoch(&self) -> u64 {
        self.claim_epoch.get()
    }

    /// The model candidate whose credential the receipt realized.
    #[must_use]
    pub fn candidate_fingerprint(&self) -> &CandidateFingerprint {
        &self.candidate_fingerprint
    }

    /// The Platform's encoding, for the Platform to decode.
    #[must_use]
    pub fn encoded(&self) -> &serde_json::Value {
        &self.encoded
    }
}

impl Serialize for CredentialRealizationToken {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.encoded.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CredentialRealizationToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(serde_json::Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// One deterministic Memory mutation proposed by the extractor.
///
/// `observed_generation` is captured before storage. A retry may apply the mutation
/// only when the current head still equals that token, or treat it as already
/// applied when the current head equals `target_sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExtractionMutation {
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<MemoryHeadToken>,
    pub target_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryMutationReceipt {
    pub path: String,
    pub target_sha256: String,
    /// `true` when a prior attempt had already committed this exact target.
    pub already_applied: bool,
}

/// Durable proof that every proposed mutation reached the governed store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExtractionReceipt {
    pub stored_at_unix_ms: u64,
    pub mutations: Vec<MemoryMutationReceipt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryExtractionStatus {
    Pending,
    Claimed,
    Extracted,
    Stored,
    Completed,
    ExecutionIndeterminate,
    TerminalFailed,
}

impl MemoryExtractionStatus {
    /// The complete fail-closed scheduling set. Repository adapters bind these
    /// names instead of maintaining their own terminal-state exclusions.
    pub const RECOVERABLE: [Self; 4] =
        [Self::Pending, Self::Claimed, Self::Extracted, Self::Stored];

    #[must_use]
    pub const fn storage_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Extracted => "extracted",
            Self::Stored => "stored",
            Self::Completed => "completed",
            Self::ExecutionIndeterminate => "execution_indeterminate",
            Self::TerminalFailed => "terminal_failed",
        }
    }

    #[must_use]
    pub const fn recoverable_storage_names() -> [&'static str; 4] {
        [
            Self::RECOVERABLE[0].storage_name(),
            Self::RECOVERABLE[1].storage_name(),
            Self::RECOVERABLE[2].storage_name(),
            Self::RECOVERABLE[3].storage_name(),
        ]
    }

    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Claimed | Self::Extracted | Self::Stored
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !self.is_recoverable()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryExtractionError {
    Invalid(String),
    NotFound(String),
    IdempotencyConflict(String),
    RevisionConflict(String),
    LeaseHeld {
        lease_expires_at_unix_ms: u64,
    },
    StaleClaim,
    InvalidTransition {
        from: MemoryExtractionStatus,
        to: MemoryExtractionStatus,
    },
    Storage(String),
}

impl std::fmt::Display for MemoryExtractionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => {
                write!(formatter, "invalid Memory extraction intent: {message}")
            }
            Self::NotFound(id) => {
                write!(formatter, "Memory extraction intent `{id}` was not found")
            }
            Self::IdempotencyConflict(key) => write!(
                formatter,
                "Memory extraction idempotency key `{key}` has different content"
            ),
            Self::RevisionConflict(id) => {
                write!(
                    formatter,
                    "Memory extraction intent `{id}` changed concurrently"
                )
            }
            Self::LeaseHeld {
                lease_expires_at_unix_ms,
            } => write!(
                formatter,
                "Memory extraction intent is already claimed until {lease_expires_at_unix_ms}"
            ),
            Self::StaleClaim => formatter.write_str("stale Memory extraction claim"),
            Self::InvalidTransition { from, to } => write!(
                formatter,
                "invalid Memory extraction transition from {from:?} to {to:?}"
            ),
            Self::Storage(message) => {
                write!(formatter, "Memory extraction repository failure: {message}")
            }
        }
    }
}

impl std::error::Error for MemoryExtractionError {}

/// Domain result of observing the stable auxiliary Agent Run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryExtractionOutcome {
    Mutations(Vec<MemoryExtractionMutation>),
    Retryable {
        reason: String,
    },
    ExecutionIndeterminate {
        auxiliary_run_id: String,
        reason: String,
    },
    PermanentFailure {
        reason: String,
    },
}

#[cfg(test)]
mod head_token_tests {
    use super::*;

    #[test]
    fn memory_head_token_keeps_the_wire_shape_of_the_value_it_wraps() {
        // The SDK carries the store's encoding unchanged: a persisted mutation
        // written before the token existed decodes to the same value, and a new
        // one serializes to exactly the store's encoding.
        let head = serde_json::json!({"memory_id": "m", "version": 3, "content_sha256": "abc"});
        let mutation = MemoryExtractionMutation {
            path: "/a.md".into(),
            content: "x".into(),
            observed_generation: Some(MemoryHeadToken::new(head.clone())),
            target_sha256: "t".into(),
        };
        let wire = serde_json::to_value(&mutation).unwrap();
        assert_eq!(wire["observed_generation"], head);
        let back: MemoryExtractionMutation = serde_json::from_value(wire).unwrap();
        assert_eq!(back, mutation);
        assert_eq!(back.observed_generation.unwrap().encoded(), &head);
    }

    #[test]
    fn credential_realization_token_keeps_the_wire_shape_of_the_receipt_it_wraps() {
        // The SDK carries the Platform's receipt encoding unchanged and reads
        // only the claim epoch and the candidate. An encoding without either
        // is refused instead of becoming an unfenced receipt.
        let receipt = serde_json::json!({
            "candidate_fingerprint": "sha256:candidate",
            "credential": {"id": "credential-a", "revision": 3},
            "selected_plaintext_holder": {"boundary": "worker", "trust_domain": "awaken.worker"},
            "actual_realization_kind": "worker_provider_adapter",
            "claim_epoch": 4,
            "receipt_fingerprint": "sha256:receipt"
        });
        let token = CredentialRealizationToken::new(receipt.clone()).unwrap();
        assert_eq!(token.claim_epoch(), 4);
        assert_eq!(
            token.candidate_fingerprint(),
            &CandidateFingerprint("sha256:candidate".into())
        );
        assert_eq!(serde_json::to_value(&token).unwrap(), receipt);
        let back: CredentialRealizationToken = serde_json::from_value(receipt.clone()).unwrap();
        assert_eq!(back, token);
        assert_eq!(back.encoded(), &receipt);
        let mut unminted = receipt.clone();
        unminted["claim_epoch"] = serde_json::json!(0);
        assert!(CredentialRealizationToken::new(unminted).is_err());
        let mut unfenced = receipt;
        unfenced.as_object_mut().unwrap().remove("claim_epoch");
        assert!(CredentialRealizationToken::new(unfenced.clone()).is_err());
        assert!(serde_json::from_value::<CredentialRealizationToken>(unfenced).is_err());
    }
}
