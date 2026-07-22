//! Durable Memory extraction intent and repository port.
//!
//! Extraction is Session application work triggered by a terminal commit. It is
//! not part of the Memory resource aggregate and it is not an authorization
//! decision. The intent therefore carries only the already-selected Workspace,
//! MemoryStore identity/config version, secret-free extractor snapshot and input.
//! No principal, role, API key, policy, Project, WorkUnit or credential material
//! crosses this boundary.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use serde::{Deserialize, Serialize};

/// Frozen, secret-free extractor configuration used by every retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExtractorSnapshot {
    pub agent_id: String,
    pub model_ref: String,
    /// Configuration-publication output used to inject the same credential and
    /// endpoint on every retry. It contains references only, never secret bytes.
    pub inference_access: awaken_inference_contract::InferenceAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_prompt: Option<String>,
}

/// One deterministic Memory mutation proposed by the extractor.
///
/// `observed_sha256` is captured before storage. A retry may apply the mutation
/// only when the current head still equals that value, or treat it as already
/// applied when the current head equals `target_sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryExtractionMutation {
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_sha256: Option<String>,
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
    TerminalFailed,
}

impl MemoryExtractionStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::TerminalFailed)
    }
}

/// Recoverable application intent keyed by one terminal commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExtractionIntent {
    pub intent_id: String,
    pub idempotency_key: String,
    pub workspace_id: String,
    pub session_id: String,
    pub terminal_commit_id: String,
    pub memory_store_id: String,
    pub memory_config_version: u64,
    pub transcript: Vec<Message>,
    pub extractor: MemoryExtractorSnapshot,
    pub status: MemoryExtractionStatus,
    pub attempts: u32,
    pub revision: u64,
    pub claim_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub mutations: Vec<MemoryExtractionMutation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<MemoryExtractionReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryExtractionError {
    #[error("invalid Memory extraction intent: {0}")]
    Invalid(String),
    #[error("Memory extraction intent `{0}` was not found")]
    NotFound(String),
    #[error("Memory extraction idempotency key `{0}` has different content")]
    IdempotencyConflict(String),
    #[error("Memory extraction intent `{0}` changed concurrently")]
    RevisionConflict(String),
    #[error("Memory extraction intent is already claimed until {lease_expires_at_unix_ms}")]
    LeaseHeld { lease_expires_at_unix_ms: u64 },
    #[error("stale Memory extraction claim")]
    StaleClaim,
    #[error("invalid Memory extraction transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: MemoryExtractionStatus,
        to: MemoryExtractionStatus,
    },
    #[error("Memory extraction repository failure: {0}")]
    Storage(String),
}

impl MemoryExtractionIntent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        intent_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        workspace_id: impl Into<String>,
        session_id: impl Into<String>,
        terminal_commit_id: impl Into<String>,
        memory_store_id: impl Into<String>,
        memory_config_version: u64,
        transcript: Vec<Message>,
        extractor: MemoryExtractorSnapshot,
    ) -> Result<Self, MemoryExtractionError> {
        let intent = Self {
            intent_id: intent_id.into(),
            idempotency_key: idempotency_key.into(),
            workspace_id: workspace_id.into(),
            session_id: session_id.into(),
            terminal_commit_id: terminal_commit_id.into(),
            memory_store_id: memory_store_id.into(),
            memory_config_version,
            transcript,
            extractor,
            status: MemoryExtractionStatus::Pending,
            attempts: 0,
            revision: 0,
            claim_generation: 0,
            claim_owner: None,
            lease_expires_at_unix_ms: None,
            mutations: Vec::new(),
            receipt: None,
            last_error: None,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub fn validate(&self) -> Result<(), MemoryExtractionError> {
        for (name, value) in [
            ("intent_id", self.intent_id.as_str()),
            ("idempotency_key", self.idempotency_key.as_str()),
            ("workspace_id", self.workspace_id.as_str()),
            ("session_id", self.session_id.as_str()),
            ("terminal_commit_id", self.terminal_commit_id.as_str()),
            ("memory_store_id", self.memory_store_id.as_str()),
            ("extractor.agent_id", self.extractor.agent_id.as_str()),
            ("extractor.model_ref", self.extractor.model_ref.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(MemoryExtractionError::Invalid(format!(
                    "{name} must not be empty"
                )));
            }
        }
        if self.memory_config_version == 0 {
            return Err(MemoryExtractionError::Invalid(
                "memory_config_version must be positive".into(),
            ));
        }
        if self.mutations.iter().any(|mutation| {
            mutation.path.trim().is_empty() || mutation.target_sha256.trim().is_empty()
        }) {
            return Err(MemoryExtractionError::Invalid(
                "extraction mutations require a path and target hash".into(),
            ));
        }
        match self.status {
            MemoryExtractionStatus::Pending | MemoryExtractionStatus::Claimed
                if !self.mutations.is_empty() || self.receipt.is_some() =>
            {
                return Err(MemoryExtractionError::Invalid(
                    "pre-extraction intent cannot contain mutations or a receipt".into(),
                ));
            }
            MemoryExtractionStatus::Extracted if self.receipt.is_some() => {
                return Err(MemoryExtractionError::Invalid(
                    "extracted intent cannot contain a storage receipt".into(),
                ));
            }
            MemoryExtractionStatus::Stored | MemoryExtractionStatus::Completed
                if self.receipt.is_none() =>
            {
                return Err(MemoryExtractionError::Invalid(
                    "stored intent requires a storage receipt".into(),
                ));
            }
            _ => {}
        }
        if let Some(receipt) = &self.receipt {
            Self::validate_receipt(&self.mutations, receipt)?;
        }
        if self.status.is_terminal()
            && (self.claim_owner.is_some() || self.lease_expires_at_unix_ms.is_some())
        {
            return Err(MemoryExtractionError::Invalid(
                "terminal intent cannot retain a claim".into(),
            ));
        }
        Ok(())
    }

    fn validate_receipt(
        mutations: &[MemoryExtractionMutation],
        receipt: &MemoryExtractionReceipt,
    ) -> Result<(), MemoryExtractionError> {
        let matches = mutations.len() == receipt.mutations.len()
            && mutations
                .iter()
                .zip(&receipt.mutations)
                .all(|(mutation, stored)| {
                    mutation.path == stored.path && mutation.target_sha256 == stored.target_sha256
                });
        if matches {
            Ok(())
        } else {
            Err(MemoryExtractionError::Invalid(
                "receipt must match every proposed mutation".into(),
            ))
        }
    }

    /// Whether two values describe the same immutable request. Lifecycle fields
    /// deliberately do not participate: redelivery after an intent advanced must
    /// still resolve to `Existing`, not an idempotency conflict.
    #[must_use]
    pub fn same_request(&self, other: &Self) -> bool {
        self.intent_id == other.intent_id
            && self.idempotency_key == other.idempotency_key
            && self.workspace_id == other.workspace_id
            && self.session_id == other.session_id
            && self.terminal_commit_id == other.terminal_commit_id
            && self.memory_store_id == other.memory_store_id
            && self.memory_config_version == other.memory_config_version
            && self.transcript == other.transcript
            && self.extractor == other.extractor
    }

    /// Acquire or recover the lease for a non-terminal intent.
    pub fn claim(
        &mut self,
        owner: &str,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<u64, MemoryExtractionError> {
        if owner.trim().is_empty() || lease_ms == 0 {
            return Err(MemoryExtractionError::Invalid(
                "claim owner and positive lease are required".into(),
            ));
        }
        if self.status.is_terminal() {
            return Err(MemoryExtractionError::InvalidTransition {
                from: self.status,
                to: MemoryExtractionStatus::Claimed,
            });
        }
        if let Some(expires) = self.lease_expires_at_unix_ms
            && expires > now_unix_ms
        {
            return Err(MemoryExtractionError::LeaseHeld {
                lease_expires_at_unix_ms: expires,
            });
        }
        let expires = now_unix_ms
            .checked_add(lease_ms)
            .ok_or_else(|| MemoryExtractionError::Invalid("claim lease overflow".into()))?;
        self.claim_generation = self
            .claim_generation
            .checked_add(1)
            .ok_or_else(|| MemoryExtractionError::Invalid("claim generation exhausted".into()))?;
        self.attempts = self.attempts.saturating_add(1);
        if self.status == MemoryExtractionStatus::Pending {
            self.status = MemoryExtractionStatus::Claimed;
        }
        self.claim_owner = Some(owner.to_string());
        self.lease_expires_at_unix_ms = Some(expires);
        self.last_error = None;
        self.bump_revision()?;
        Ok(self.claim_generation)
    }

    pub fn mark_extracted(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        mutations: Vec<MemoryExtractionMutation>,
    ) -> Result<(), MemoryExtractionError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        self.require_status(
            MemoryExtractionStatus::Claimed,
            MemoryExtractionStatus::Extracted,
        )?;
        self.mutations = mutations;
        self.status = MemoryExtractionStatus::Extracted;
        self.bump_revision()
    }

    /// Extend the current fenced claim while a slow extractor is still running.
    /// Renewal keeps the same generation and attempt; only the lease/revision move.
    pub fn renew_claim(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<(), MemoryExtractionError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        if lease_ms == 0 {
            return Err(MemoryExtractionError::Invalid(
                "positive renewal lease is required".into(),
            ));
        }
        self.lease_expires_at_unix_ms = Some(
            now_unix_ms
                .checked_add(lease_ms)
                .ok_or_else(|| MemoryExtractionError::Invalid("claim lease overflow".into()))?,
        );
        self.bump_revision()
    }

    pub fn mark_stored(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        receipt: MemoryExtractionReceipt,
    ) -> Result<(), MemoryExtractionError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        self.require_status(
            MemoryExtractionStatus::Extracted,
            MemoryExtractionStatus::Stored,
        )?;
        Self::validate_receipt(&self.mutations, &receipt)?;
        self.receipt = Some(receipt);
        self.status = MemoryExtractionStatus::Stored;
        self.bump_revision()
    }

    pub fn complete(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
    ) -> Result<(), MemoryExtractionError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        self.require_status(
            MemoryExtractionStatus::Stored,
            MemoryExtractionStatus::Completed,
        )?;
        self.status = MemoryExtractionStatus::Completed;
        self.clear_claim();
        self.bump_revision()
    }

    /// Release a failed attempt without discarding completed work. A model failure
    /// returns `Claimed -> Pending`; storage/receipt failures retain Extracted or
    /// Stored so recovery resumes at the last durable stage.
    pub fn retry(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        error: impl Into<String>,
    ) -> Result<(), MemoryExtractionError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        if self.status == MemoryExtractionStatus::Claimed {
            self.status = MemoryExtractionStatus::Pending;
        }
        self.last_error = Some(error.into());
        self.clear_claim();
        self.bump_revision()
    }

    pub fn terminal_fail(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        error: impl Into<String>,
    ) -> Result<(), MemoryExtractionError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        self.status = MemoryExtractionStatus::TerminalFailed;
        self.last_error = Some(error.into());
        self.clear_claim();
        self.bump_revision()
    }

    fn require_claim(
        &self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
    ) -> Result<(), MemoryExtractionError> {
        let valid = self.claim_owner.as_deref() == Some(owner)
            && self.claim_generation == generation
            && self
                .lease_expires_at_unix_ms
                .is_some_and(|expires| expires > now_unix_ms);
        if valid {
            Ok(())
        } else {
            Err(MemoryExtractionError::StaleClaim)
        }
    }

    fn require_status(
        &self,
        expected: MemoryExtractionStatus,
        target: MemoryExtractionStatus,
    ) -> Result<(), MemoryExtractionError> {
        if self.status == expected {
            Ok(())
        } else {
            Err(MemoryExtractionError::InvalidTransition {
                from: self.status,
                to: target,
            })
        }
    }

    fn clear_claim(&mut self) {
        self.claim_owner = None;
        self.lease_expires_at_unix_ms = None;
    }

    fn bump_revision(&mut self) -> Result<(), MemoryExtractionError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| MemoryExtractionError::Invalid("intent revision exhausted".into()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutMemoryExtractionOutcome {
    Inserted,
    Existing,
}

/// Persistence port for the intent aggregate. Implementations enforce unique
/// `idempotency_key` and optimistic revision CAS; the application state machine
/// owns lease and transition semantics above this storage boundary.
#[async_trait]
pub trait MemoryExtractionRepository: Send + Sync {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError>;

    async fn get_extraction(
        &self,
        intent_id: &str,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError>;

    async fn recoverable_extractions(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError>;

    async fn compare_and_swap_extraction(
        &self,
        expected_revision: u64,
        intent: MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Role};

    fn intent() -> MemoryExtractionIntent {
        MemoryExtractionIntent::new(
            "extract-1",
            "run-7:terminal-3",
            "ws-a",
            "session-1",
            "terminal-3",
            "memory-1",
            2,
            vec![Message::text(Id("m1".into()), Role::User, "remember me")],
            MemoryExtractorSnapshot {
                agent_id: "memory-agent".into(),
                model_ref: "model-config-2".into(),
                inference_access: awaken_inference_contract::InferenceAccess::host_executor(
                    "model-config-2",
                ),
                instructions: None,
                extraction_prompt: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn state_machine_preserves_extracted_and_stored_work_across_retry() {
        let mut intent = intent();
        let generation = intent.claim("worker-a", 100, 50).unwrap();
        assert_eq!(intent.status, MemoryExtractionStatus::Claimed);
        assert_eq!(intent.attempts, 1);
        intent
            .mark_extracted(
                "worker-a",
                generation,
                120,
                vec![MemoryExtractionMutation {
                    path: "/customer.md".into(),
                    content: "Sunday 02:00-04:00".into(),
                    observed_sha256: None,
                    target_sha256: "target".into(),
                }],
            )
            .unwrap();
        intent
            .retry("worker-a", generation, 130, "store unavailable")
            .unwrap();
        assert_eq!(intent.status, MemoryExtractionStatus::Extracted);
        assert_eq!(intent.mutations.len(), 1);

        let generation = intent.claim("worker-b", 200, 50).unwrap();
        intent
            .mark_stored(
                "worker-b",
                generation,
                210,
                MemoryExtractionReceipt {
                    stored_at_unix_ms: 210,
                    mutations: vec![MemoryMutationReceipt {
                        path: "/customer.md".into(),
                        target_sha256: "target".into(),
                        already_applied: false,
                    }],
                },
            )
            .unwrap();
        intent
            .retry("worker-b", generation, 220, "receipt publish failed")
            .unwrap();
        assert_eq!(intent.status, MemoryExtractionStatus::Stored);
        assert!(intent.receipt.is_some());

        let generation = intent.claim("worker-c", 300, 50).unwrap();
        intent.complete("worker-c", generation, 310).unwrap();
        assert_eq!(intent.status, MemoryExtractionStatus::Completed);
        assert!(intent.claim_owner.is_none());
    }

    #[test]
    fn stale_or_live_claims_cannot_advance_the_intent() {
        let mut intent = intent();
        let generation = intent.claim("worker-a", 100, 50).unwrap();
        assert!(matches!(
            intent.claim("worker-b", 120, 50),
            Err(MemoryExtractionError::LeaseHeld { .. })
        ));
        assert_eq!(
            intent.mark_extracted("worker-b", generation, 130, Vec::new()),
            Err(MemoryExtractionError::StaleClaim)
        );
        assert_eq!(
            intent.mark_extracted("worker-a", generation, 151, Vec::new()),
            Err(MemoryExtractionError::StaleClaim)
        );
        let next_generation = intent.claim("worker-b", 151, 50).unwrap();
        assert!(next_generation > generation);
    }

    #[test]
    fn renewal_extends_only_the_current_fenced_claim() {
        let mut intent = intent();
        let generation = intent.claim("worker-a", 100, 50).unwrap();
        let attempts = intent.attempts;
        intent.renew_claim("worker-a", generation, 120, 80).unwrap();
        assert_eq!(intent.lease_expires_at_unix_ms, Some(200));
        assert_eq!(intent.claim_generation, generation);
        assert_eq!(intent.attempts, attempts);
        assert_eq!(
            intent.renew_claim("worker-b", generation, 130, 80),
            Err(MemoryExtractionError::StaleClaim)
        );
    }

    #[test]
    fn storage_receipt_must_match_each_planned_mutation() {
        let mut intent = intent();
        let generation = intent.claim("worker-a", 100, 50).unwrap();
        intent
            .mark_extracted(
                "worker-a",
                generation,
                110,
                vec![MemoryExtractionMutation {
                    path: "/customer.md".into(),
                    content: "remember".into(),
                    observed_sha256: None,
                    target_sha256: "expected".into(),
                }],
            )
            .unwrap();

        let error = intent
            .mark_stored(
                "worker-a",
                generation,
                120,
                MemoryExtractionReceipt {
                    stored_at_unix_ms: 120,
                    mutations: vec![MemoryMutationReceipt {
                        path: "/customer.md".into(),
                        target_sha256: "forged".into(),
                        already_applied: false,
                    }],
                },
            )
            .unwrap_err();
        assert_eq!(
            error,
            MemoryExtractionError::Invalid("receipt must match every proposed mutation".into())
        );
        assert_eq!(intent.status, MemoryExtractionStatus::Extracted);
        assert!(intent.receipt.is_none());
    }

    #[test]
    fn intent_is_secret_and_authorization_language_free_on_the_wire() {
        let value = serde_json::to_value(intent()).unwrap();
        let text = value.to_string();
        // `transcript[*].role` is the Agent message role, not an IAM role.
        for forbidden in [
            "principal",
            "api_key",
            "authorization_role",
            "iam_role",
            "policy",
            "credential",
        ] {
            assert!(!text.contains(forbidden));
        }
        assert_eq!(value["workspace_id"], "ws-a");
        assert_eq!(value["memory_config_version"], 2);
    }
}
