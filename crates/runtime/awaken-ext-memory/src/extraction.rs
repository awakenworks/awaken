//! Durable Memory extraction aggregate and repository port.
//!
//! Extraction is Memory extension work triggered by a committed terminal Run. It
//! is not Session protocol state, part of the Memory resource aggregate, or an
//! authorization decision. The intent therefore carries only the already-selected
//! Workspace, MemoryStore identity/config version, secret-free extractor snapshot
//! and input.
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
    pub inference_access: awaken_runtime_contract::InferenceAccess,
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

/// Runtime environment used by the Memory extension to perform side effects.
///
/// The extension owns ordering, leases, retries, and durable transitions. An
/// embedding Runtime supplies only the concrete extractor and governed content
/// store operations; it cannot alter the extraction state machine.
#[async_trait]
pub trait MemoryExtractionDriver: Send + Sync {
    /// Whether this driver owns the frozen binding carried by `intent`.
    fn accepts(&self, intent: &MemoryExtractionIntent) -> bool;

    /// Revalidate the resource binding immediately before external IO.
    async fn validate_binding(&self, intent: &MemoryExtractionIntent) -> Result<(), String>;

    /// Execute the pinned Extractor Agent and return deterministic proposed writes.
    async fn extract(
        &self,
        intent: &MemoryExtractionIntent,
    ) -> Result<Vec<MemoryExtractionMutation>, String>;

    /// Apply one proposed write using the mutation's optimistic hashes.
    async fn apply(
        &self,
        intent: &MemoryExtractionIntent,
        mutation: &MemoryExtractionMutation,
    ) -> Result<MemoryMutationReceipt, String>;
}

/// Operational policy for the at-least-once extraction worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryExtractionPolicy {
    pub lease_ms: u64,
    pub heartbeat_ms: u64,
    pub max_attempts: u32,
    pub retry_base_ms: u64,
}

impl Default for MemoryExtractionPolicy {
    fn default() -> Self {
        Self {
            lease_ms: 3_000,
            heartbeat_ms: 1_000,
            max_attempts: 5,
            retry_base_ms: 25,
        }
    }
}

/// Memory-owned application service that advances durable extraction intents.
///
/// A controller is cheap to construct. Correctness lives in the repository CAS
/// and frozen intent, so multiple processes may drive the same binding safely.
pub struct MemoryExtractionController {
    repository: std::sync::Arc<dyn MemoryExtractionRepository>,
    owner: String,
    policy: MemoryExtractionPolicy,
}

impl MemoryExtractionController {
    #[must_use]
    pub fn new(
        repository: std::sync::Arc<dyn MemoryExtractionRepository>,
        owner: impl Into<String>,
    ) -> Self {
        Self {
            repository,
            owner: owner.into(),
            policy: MemoryExtractionPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: MemoryExtractionPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// CAS-create an intent. Redelivery is accepted only for the same immutable
    /// request; a reused stable identity with different content fails closed.
    pub async fn enqueue(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        if let Some(existing) = self.repository.get_extraction(&intent.intent_id).await? {
            return if existing.same_request(&intent) {
                Ok(PutMemoryExtractionOutcome::Existing)
            } else {
                Err(MemoryExtractionError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        self.repository.put_extraction_if_absent(intent).await
    }

    /// Drive every recoverable intent accepted by one frozen binding.
    pub async fn drive_recoverable(&self, driver: &dyn MemoryExtractionDriver) {
        loop {
            let Ok(candidates) = self.repository.recoverable_extractions(64).await else {
                return;
            };
            let Some(mut intent) = candidates.into_iter().find(|intent| driver.accepts(intent))
            else {
                return;
            };
            let now = unix_ms();
            let expected_revision = intent.revision;
            let generation = match intent.claim(&self.owner, now, self.policy.lease_ms) {
                Ok(generation) => generation,
                Err(MemoryExtractionError::LeaseHeld {
                    lease_expires_at_unix_ms,
                }) => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        lease_expires_at_unix_ms
                            .saturating_sub(now)
                            .saturating_add(1),
                    ))
                    .await;
                    continue;
                }
                Err(_) => return,
            };
            if self
                .repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .is_err()
            {
                continue;
            }

            let result = self.advance_claimed(driver, &mut intent, generation).await;
            if let Err((error, terminal)) = result {
                let Ok(Some(current)) = self.repository.get_extraction(&intent.intent_id).await
                else {
                    return;
                };
                if current.revision != intent.revision
                    || current.claim_owner.as_deref() != Some(self.owner.as_str())
                    || current.claim_generation != generation
                {
                    continue;
                }
                let now = unix_ms();
                let expected_revision = intent.revision;
                let transition = if terminal || intent.attempts >= self.policy.max_attempts {
                    intent.terminal_fail(&self.owner, generation, now, error)
                } else {
                    intent.retry(&self.owner, generation, now, error)
                };
                if transition.is_ok() {
                    let _ = self
                        .repository
                        .compare_and_swap_extraction(expected_revision, intent.clone())
                        .await;
                }
                if !terminal && intent.attempts < self.policy.max_attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        self.policy.retry_base_ms * u64::from(intent.attempts.max(1)),
                    ))
                    .await;
                    continue;
                }
            }
        }
    }

    async fn advance_claimed(
        &self,
        driver: &dyn MemoryExtractionDriver,
        intent: &mut MemoryExtractionIntent,
        generation: u64,
    ) -> Result<(), (String, bool)> {
        driver
            .validate_binding(intent)
            .await
            .map_err(|error| (error, true))?;
        if intent.status == MemoryExtractionStatus::Claimed {
            let extraction_input = intent.clone();
            let extraction = driver.extract(&extraction_input);
            tokio::pin!(extraction);
            let mutations = loop {
                tokio::select! {
                    result = &mut extraction => break result.map_err(|error| (error, false))?,
                    () = tokio::time::sleep(std::time::Duration::from_millis(self.policy.heartbeat_ms)) => {
                        let expected_revision = intent.revision;
                        intent
                            .renew_claim(
                                &self.owner,
                                generation,
                                unix_ms(),
                                self.policy.lease_ms,
                            )
                            .map_err(|error| (error.to_string(), false))?;
                        self.repository
                            .compare_and_swap_extraction(expected_revision, intent.clone())
                            .await
                            .map_err(|error| (error.to_string(), false))?;
                    }
                }
            };
            let expected_revision = intent.revision;
            intent
                .mark_extracted(&self.owner, generation, unix_ms(), mutations)
                .map_err(|error| (error.to_string(), false))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        if intent.status == MemoryExtractionStatus::Extracted {
            driver
                .validate_binding(intent)
                .await
                .map_err(|error| (error, true))?;
            let mut receipts = Vec::with_capacity(intent.mutations.len());
            for mutation in &intent.mutations {
                receipts.push(
                    driver
                        .apply(intent, mutation)
                        .await
                        .map_err(|error| (error, false))?,
                );
            }
            let expected_revision = intent.revision;
            intent
                .mark_stored(
                    &self.owner,
                    generation,
                    unix_ms(),
                    MemoryExtractionReceipt {
                        stored_at_unix_ms: unix_ms(),
                        mutations: receipts,
                    },
                )
                .map_err(|error| (error.to_string(), false))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        if intent.status == MemoryExtractionStatus::Stored {
            let expected_revision = intent.revision;
            intent
                .complete(&self.owner, generation, unix_ms())
                .map_err(|error| (error.to_string(), false))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        Ok(())
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Role};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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
                inference_access: awaken_runtime_contract::InferenceAccess::host_executor(
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

    #[derive(Default)]
    struct TestRepository(Mutex<Option<MemoryExtractionIntent>>);

    #[async_trait]
    impl MemoryExtractionRepository for TestRepository {
        async fn put_extraction_if_absent(
            &self,
            intent: MemoryExtractionIntent,
        ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
            let mut stored = self.0.lock().unwrap();
            match stored.as_ref() {
                Some(existing) if existing.same_request(&intent) => {
                    Ok(PutMemoryExtractionOutcome::Existing)
                }
                Some(_) => Err(MemoryExtractionError::IdempotencyConflict(
                    intent.idempotency_key,
                )),
                None => {
                    *stored = Some(intent);
                    Ok(PutMemoryExtractionOutcome::Inserted)
                }
            }
        }

        async fn get_extraction(
            &self,
            intent_id: &str,
        ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .as_ref()
                .filter(|intent| intent.intent_id == intent_id)
                .cloned())
        }

        async fn recoverable_extractions(
            &self,
            limit: usize,
        ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .as_ref()
                .filter(|intent| !intent.status.is_terminal() && limit > 0)
                .cloned()
                .into_iter()
                .collect())
        }

        async fn compare_and_swap_extraction(
            &self,
            expected_revision: u64,
            intent: MemoryExtractionIntent,
        ) -> Result<(), MemoryExtractionError> {
            let mut stored = self.0.lock().unwrap();
            let Some(current) = stored.as_ref() else {
                return Err(MemoryExtractionError::NotFound(intent.intent_id));
            };
            if current.revision != expected_revision {
                return Err(MemoryExtractionError::RevisionConflict(intent.intent_id));
            }
            *stored = Some(intent);
            Ok(())
        }
    }

    struct TestDriver {
        extraction_calls: AtomicUsize,
        fail_first_extraction: bool,
        binding_valid: bool,
    }

    #[async_trait]
    impl MemoryExtractionDriver for TestDriver {
        fn accepts(&self, intent: &MemoryExtractionIntent) -> bool {
            intent.session_id == "session-1"
        }

        async fn validate_binding(&self, _intent: &MemoryExtractionIntent) -> Result<(), String> {
            self.binding_valid
                .then_some(())
                .ok_or_else(|| "binding revoked".to_string())
        }

        async fn extract(
            &self,
            _intent: &MemoryExtractionIntent,
        ) -> Result<Vec<MemoryExtractionMutation>, String> {
            let call = self.extraction_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first_extraction && call == 0 {
                return Err("temporary model failure".into());
            }
            Ok(vec![MemoryExtractionMutation {
                path: "/customer.md".into(),
                content: "remember".into(),
                observed_sha256: None,
                target_sha256: "target".into(),
            }])
        }

        async fn apply(
            &self,
            _intent: &MemoryExtractionIntent,
            mutation: &MemoryExtractionMutation,
        ) -> Result<MemoryMutationReceipt, String> {
            Ok(MemoryMutationReceipt {
                path: mutation.path.clone(),
                target_sha256: mutation.target_sha256.clone(),
                already_applied: false,
            })
        }
    }

    #[tokio::test]
    async fn controller_owns_retry_receipt_and_completion_ordering() {
        let repository = Arc::new(TestRepository::default());
        let controller = MemoryExtractionController::new(repository.clone(), "worker-a")
            .with_policy(MemoryExtractionPolicy {
                lease_ms: 1_000,
                heartbeat_ms: 100,
                max_attempts: 3,
                retry_base_ms: 0,
            });
        assert_eq!(
            controller.enqueue(intent()).await.unwrap(),
            PutMemoryExtractionOutcome::Inserted
        );
        assert_eq!(
            controller.enqueue(intent()).await.unwrap(),
            PutMemoryExtractionOutcome::Existing
        );
        let driver = TestDriver {
            extraction_calls: AtomicUsize::new(0),
            fail_first_extraction: true,
            binding_valid: true,
        };

        controller.drive_recoverable(&driver).await;

        let completed = repository
            .get_extraction("extract-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, MemoryExtractionStatus::Completed);
        assert_eq!(completed.attempts, 2);
        assert_eq!(completed.receipt.unwrap().mutations.len(), 1);
        assert_eq!(driver.extraction_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn revoked_binding_fails_terminally_without_running_extractor() {
        let repository = Arc::new(TestRepository::default());
        let controller = MemoryExtractionController::new(repository.clone(), "worker-a");
        controller.enqueue(intent()).await.unwrap();
        let driver = TestDriver {
            extraction_calls: AtomicUsize::new(0),
            fail_first_extraction: false,
            binding_valid: false,
        };

        controller.drive_recoverable(&driver).await;

        let failed = repository
            .get_extraction("extract-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, MemoryExtractionStatus::TerminalFailed);
        assert_eq!(failed.last_error.as_deref(), Some("binding revoked"));
        assert_eq!(driver.extraction_calls.load(Ordering::SeqCst), 0);
    }
}
