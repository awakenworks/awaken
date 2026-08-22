//! Durable Memory extraction aggregate and repository port.
//!
//! Extraction is Memory extension work triggered by a committed terminal Run. It
//! is not Session protocol state, part of the Memory resource aggregate, or an
//! authorization decision. The intent therefore carries only the already-selected
//! Workspace, MemoryStore identity/config version, secret-free extractor snapshot
//! and input.
//! No authorization principal, API key, Project/WorkUnit identity, or raw
//! credential material crosses this boundary. The complete executable snapshot
//! may carry secret-free permission policy and credential references.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::thread::read::transcript::{
    TranscriptRange, TranscriptSliceSpec, TranscriptSnapshot, TranscriptSnapshotRef,
};
use serde::{Deserialize, Serialize};

/// Frozen, secret-free extractor configuration used by every retry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryExtractorSnapshot {
    /// Complete ordinary Agent publication used on every retry. It contains
    /// model/tool/plugin references only, never secret bytes.
    pub agent: awaken_runtime_contract::ExecutableAgentSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_prompt: Option<String>,
}

impl<'de> Deserialize<'de> for MemoryExtractorSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Current {
                agent: Box<awaken_runtime_contract::ExecutableAgentSnapshot>,
                #[serde(default)]
                extraction_prompt: Option<String>,
            },
            Legacy {
                agent_id: String,
                model: Box<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
                #[serde(default)]
                instructions: Option<String>,
                #[serde(default)]
                extraction_prompt: Option<String>,
            },
        }

        Ok(match Stored::deserialize(deserializer)? {
            Stored::Current {
                agent,
                extraction_prompt,
            } => Self {
                agent: *agent,
                extraction_prompt,
            },
            Stored::Legacy {
                agent_id,
                model,
                instructions,
                extraction_prompt,
            } => Self {
                agent: crate::memory_agent(
                    &agent_id,
                    *model,
                    instructions
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(crate::DEFAULT_MEMORY_INSTRUCTIONS),
                ),
                extraction_prompt,
            },
        })
    }
}

impl MemoryExtractorSnapshot {
    /// Construct the explicit host-executor form used by embedded compositions
    /// and persistence-adapter tests. Provider-backed publications carry their
    /// complete candidate from the executable snapshot instead.
    #[must_use]
    pub fn host_executor(
        agent_id: impl Into<String>,
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Self {
        let agent_id = agent_id.into();
        Self {
            agent: crate::memory_agent(
                &agent_id,
                awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    awaken_runtime_contract::resolved::ModelBinding::new(
                        provider_identity_ref,
                        model_ref,
                        backend_ref,
                    ),
                ),
                crate::DEFAULT_MEMORY_INSTRUCTIONS,
            ),
            extraction_prompt: None,
        }
    }
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
    /// Half-open committed transcript range owned by this intent.
    #[serde(default)]
    pub transcript_start: usize,
    #[serde(default)]
    pub transcript_end: usize,
    /// Immutable identity of the committed Thread prefix this intent observed.
    /// Legacy intents omit it and retain only the materialized transcript below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_snapshot: Option<TranscriptSnapshotRef>,
    /// Domain-selected half-open windows over `transcript_snapshot`.
    #[serde(default)]
    pub transcript_ranges: Vec<TranscriptRange>,
    /// Materialized window cache. The snapshot/ranges are authoritative for new
    /// intents; retaining the selected messages makes extraction independent of
    /// cache eviction and keeps legacy serialized intents recoverable.
    pub transcript: Vec<Message>,
    /// Stable ordinary Agent Run identity used by every retry.
    #[serde(default)]
    pub auxiliary_thread_id: String,
    #[serde(default)]
    pub auxiliary_run_id: String,
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
    /// Secret-free proof that the current claim realized its frozen extractor
    /// credential through the selected mechanism. This belongs to the same
    /// durable aggregate as the extraction claim; a relay-local/process-local
    /// receipt would not survive recovery or fence a stale claimant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_realizations: Vec<awaken_runtime_contract::CredentialRealizationReceipt>,
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
    pub fn new_range(
        intent_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        workspace_id: impl Into<String>,
        session_id: impl Into<String>,
        terminal_commit_id: impl Into<String>,
        memory_store_id: impl Into<String>,
        memory_config_version: u64,
        transcript_start: usize,
        transcript_end: usize,
        transcript: Vec<Message>,
        extractor: MemoryExtractorSnapshot,
    ) -> Result<Self, MemoryExtractionError> {
        let intent_id = intent_id.into();
        let intent = Self {
            intent_id: intent_id.clone(),
            idempotency_key: idempotency_key.into(),
            workspace_id: workspace_id.into(),
            session_id: session_id.into(),
            terminal_commit_id: terminal_commit_id.into(),
            memory_store_id: memory_store_id.into(),
            memory_config_version,
            transcript_start,
            transcript_end,
            transcript_snapshot: None,
            transcript_ranges: Vec::new(),
            transcript,
            auxiliary_thread_id: format!("{intent_id}/agent"),
            auxiliary_run_id: format!("{intent_id}/agent/run"),
            extractor,
            status: MemoryExtractionStatus::Pending,
            attempts: 0,
            revision: 0,
            claim_generation: 0,
            claim_owner: None,
            lease_expires_at_unix_ms: None,
            mutations: Vec::new(),
            receipt: None,
            credential_realizations: Vec::new(),
            last_error: None,
        };
        intent.validate()?;
        Ok(intent)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_snapshot(
        intent_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        workspace_id: impl Into<String>,
        session_id: impl Into<String>,
        terminal_commit_id: impl Into<String>,
        memory_store_id: impl Into<String>,
        memory_config_version: u64,
        snapshot: TranscriptSnapshotRef,
        ranges: Vec<TranscriptRange>,
        transcript: Vec<Message>,
        extractor: MemoryExtractorSnapshot,
    ) -> Result<Self, MemoryExtractionError> {
        let intent_id = intent_id.into();
        let start = ranges.first().map_or(snapshot.end_seq, |range| range.start);
        let end = snapshot.end_seq;
        let transcript_start = usize::try_from(start).map_err(|_| {
            MemoryExtractionError::Invalid("transcript range start exceeds usize".into())
        })?;
        let transcript_end = usize::try_from(end).map_err(|_| {
            MemoryExtractionError::Invalid("transcript range end exceeds usize".into())
        })?;
        let cached_len = transcript.len();
        let mut intent = Self::new_range(
            intent_id.clone(),
            idempotency_key,
            workspace_id,
            session_id,
            terminal_commit_id,
            memory_store_id,
            memory_config_version,
            0,
            cached_len,
            transcript,
            extractor,
        )?;
        intent.transcript_start = transcript_start;
        intent.transcript_end = transcript_end;
        intent.transcript_snapshot = Some(snapshot);
        intent.transcript_ranges = ranges;
        intent.auxiliary_thread_id = format!("{intent_id}/agent");
        intent.auxiliary_run_id = format!("{intent_id}/agent/run");
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
            (
                "extractor.agent_id",
                self.extractor.agent.root_agent_id.0.as_str(),
            ),
            (
                "extractor.model_ref",
                self.extractor
                    .agent
                    .resolved_spec
                    .model_binding
                    .binding()
                    .model_ref
                    .as_str(),
            ),
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
        let legacy_range =
            self.transcript_start == 0 && self.transcript_end == 0 && !self.transcript.is_empty();
        if self.transcript_snapshot.is_none()
            && !legacy_range
            && (self.transcript_end < self.transcript_start
                || self.transcript_end - self.transcript_start != self.transcript.len())
        {
            return Err(MemoryExtractionError::Invalid(
                "transcript range must match the captured messages".into(),
            ));
        }
        if let Some(snapshot) = &self.transcript_snapshot {
            if snapshot.thread_id.0 != self.session_id {
                return Err(MemoryExtractionError::Invalid(
                    "transcript snapshot must belong to the extraction Session".into(),
                ));
            }
            TranscriptSliceSpec {
                snapshot: snapshot.clone(),
                ranges: self.transcript_ranges.clone(),
            }
            .validate()
            .map_err(|error| MemoryExtractionError::Invalid(error.to_string()))?;
            let selected = self
                .transcript_ranges
                .iter()
                .try_fold(0_u64, |total, range| {
                    total.checked_add(range.end - range.start)
                });
            let Some(selected) = selected else {
                return Err(MemoryExtractionError::Invalid(
                    "selected transcript length overflow".into(),
                ));
            };
            if usize::try_from(selected).ok() != Some(self.transcript.len()) {
                return Err(MemoryExtractionError::Invalid(
                    "materialized transcript cache must match the selected ranges".into(),
                ));
            }
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
        for receipt in &self.credential_realizations {
            if receipt.claim_epoch != self.claim_generation {
                return Err(MemoryExtractionError::Invalid(
                    "credential realization must belong to the current extraction claim".into(),
                ));
            }
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
            && self.transcript_start == other.transcript_start
            && self.transcript_cursor() == other.transcript_cursor()
            && self.transcript_snapshot == other.transcript_snapshot
            && self.transcript_ranges == other.transcript_ranges
            && self.transcript == other.transcript
            && self.auxiliary_thread_id() == other.auxiliary_thread_id()
            && self.auxiliary_run_id() == other.auxiliary_run_id()
            && self.extractor == other.extractor
    }

    /// Durable cursor after this intent's captured transcript. Legacy serialized
    /// intents predate explicit ranges and therefore own their full transcript.
    #[must_use]
    pub fn transcript_cursor(&self) -> usize {
        if self.transcript_end == 0 && !self.transcript.is_empty() {
            self.transcript.len()
        } else {
            self.transcript_end
        }
    }

    /// Stable auxiliary Thread identity, derived for legacy intents that predate
    /// the explicit serialized field.
    #[must_use]
    pub fn auxiliary_thread_id(&self) -> String {
        if self.auxiliary_thread_id.trim().is_empty() {
            format!("{}/agent", self.intent_id)
        } else {
            self.auxiliary_thread_id.clone()
        }
    }

    /// Stable auxiliary Run identity, derived for legacy intents that predate
    /// the explicit serialized field.
    #[must_use]
    pub fn auxiliary_run_id(&self) -> String {
        if self.auxiliary_run_id.trim().is_empty() {
            format!("{}/agent/run", self.intent_id)
        } else {
            self.auxiliary_run_id.clone()
        }
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
        self.credential_realizations.clear();
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

    /// Record one exact credential effect under the live extraction claim.
    /// Replaying the identical receipt is idempotent; a different receipt for the
    /// same candidate/claim is rejected rather than becoming a second truth.
    pub fn record_credential_realization(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        receipt: awaken_runtime_contract::CredentialRealizationReceipt,
    ) -> Result<(), MemoryExtractionError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        if receipt.claim_epoch != generation {
            return Err(MemoryExtractionError::StaleClaim);
        }
        if let Some(existing) = self.credential_realizations.iter().find(|existing| {
            existing.candidate_fingerprint == receipt.candidate_fingerprint
                && existing.claim_epoch == receipt.claim_epoch
        }) {
            return if existing == &receipt {
                Ok(())
            } else {
                Err(MemoryExtractionError::Invalid(
                    "conflicting credential realization receipt".into(),
                ))
            };
        }
        self.credential_realizations.push(receipt);
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

    /// Greatest committed transcript boundary already owned by an intent for the
    /// Session. Pending work counts: once its intent is durable, later terminal
    /// Runs must not capture the same messages again.
    async fn extraction_cursor(&self, session_id: &str) -> Result<usize, MemoryExtractionError>;

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

/// Frozen inputs prepared by an embedding adapter for one terminal Run.
pub struct MemoryTerminalExtractionRequest {
    pub workspace_id: String,
    pub session_id: String,
    pub terminal_run_id: String,
    pub memory_store_id: String,
    pub memory_config_version: u64,
    pub committed_transcript: TranscriptSnapshot,
    pub extractor: MemoryExtractorSnapshot,
}

/// Embedding adapter used by the Memory-owned terminal observer after it has read
/// the committed Thread transcript. Implementations prepare the frozen extractor
/// and governed content-store binding; they do not decide lifecycle timing.
#[async_trait]
pub trait MemoryTerminalExtraction: Send + Sync {
    async fn extract_terminal(
        &self,
        terminal: &awaken_runtime_contract::terminal::CommittedTerminalRun,
        transcript: TranscriptSnapshot,
    ) -> Result<(), String>;
}

/// Memory Extraction's committed-terminal Runtime extension.
pub struct MemoryTerminalObserver {
    reader: std::sync::Arc<
        dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
    >,
    extraction: std::sync::Arc<dyn MemoryTerminalExtraction>,
}

impl MemoryTerminalObserver {
    #[must_use]
    pub fn new(
        reader: std::sync::Arc<
            dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
        >,
        extraction: std::sync::Arc<dyn MemoryTerminalExtraction>,
    ) -> Self {
        Self { reader, extraction }
    }
}

#[async_trait]
impl awaken_runtime_contract::terminal::RunTerminalObserver for MemoryTerminalObserver {
    fn observer_id(&self) -> &str {
        "memory-extraction"
    }

    async fn observe(
        &self,
        terminal: &awaken_runtime_contract::terminal::CommittedTerminalRun,
    ) -> Result<(), awaken_runtime_contract::terminal::RunTerminalObserverError> {
        let transcript = self.reader.transcript_snapshot(
            &terminal.thread_id,
            awaken_agent_contract::thread::read::transcript::TranscriptView::RawCommitted,
        );
        self.extraction
            .extract_terminal(terminal, transcript)
            .await
            .map_err(awaken_runtime_contract::terminal::RunTerminalObserverError)
    }
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

    /// Create the stable terminal-Run intent over only the transcript suffix not
    /// already owned by an earlier durable intent.
    pub async fn enqueue_terminal(
        &self,
        request: MemoryTerminalExtractionRequest,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        let idempotency_key = format!("{}:{}", request.session_id, request.terminal_run_id);
        let intent_id = format!("memory-extraction:{idempotency_key}");
        if let Some(existing) = self.repository.get_extraction(&intent_id).await? {
            let same_binding = existing.workspace_id == request.workspace_id
                && existing.session_id == request.session_id
                && existing.terminal_commit_id == request.terminal_run_id
                && existing.memory_store_id == request.memory_store_id
                && existing.memory_config_version == request.memory_config_version
                && existing.extractor == request.extractor;
            return if same_binding {
                Ok(PutMemoryExtractionOutcome::Existing)
            } else {
                Err(MemoryExtractionError::IdempotencyConflict(idempotency_key))
            };
        }
        if request.committed_transcript.reference().thread_id.0 != request.session_id {
            return Err(MemoryExtractionError::Invalid(
                "terminal transcript snapshot belongs to another Session".into(),
            ));
        }
        let start = self
            .repository
            .extraction_cursor(&request.session_id)
            .await?;
        if start > request.committed_transcript.messages().len() {
            return Err(MemoryExtractionError::Invalid(format!(
                "extraction cursor {start} exceeds committed transcript length {}",
                request.committed_transcript.messages().len()
            )));
        }
        let end = request.committed_transcript.messages().len();
        let start_seq = u64::try_from(start)
            .map_err(|_| MemoryExtractionError::Invalid("transcript cursor overflow".into()))?;
        let end_seq = u64::try_from(end)
            .map_err(|_| MemoryExtractionError::Invalid("transcript cursor overflow".into()))?;
        let mut ranges = Vec::new();
        let mut open = None;
        let mut transcript = Vec::new();
        for (offset, message) in request.committed_transcript.messages()[start..]
            .iter()
            .enumerate()
        {
            let sequence = start_seq
                + u64::try_from(offset).map_err(|_| {
                    MemoryExtractionError::Invalid("transcript sequence overflow".into())
                })?;
            if message.id.0.starts_with(crate::RECALL_MESSAGE_ID_PREFIX) {
                if let Some(range_start) = open.take() {
                    ranges.push(TranscriptRange::new(range_start, sequence));
                }
            } else {
                open.get_or_insert(sequence);
                transcript.push(message.clone());
            }
        }
        if let Some(range_start) = open {
            ranges.push(TranscriptRange::new(range_start, end_seq));
        }
        let intent = MemoryExtractionIntent::new_snapshot(
            intent_id,
            idempotency_key,
            request.workspace_id,
            request.session_id,
            request.terminal_run_id,
            request.memory_store_id,
            request.memory_config_version,
            request.committed_transcript.reference().clone(),
            ranges,
            transcript,
            request.extractor,
        )?;
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

    /// Renew from committed claim state, not from the controller's pre-I/O
    /// snapshot. Credential materialization may record its receipt while the
    /// extractor is running; overwriting that newer revision would either lose
    /// the receipt or turn every heartbeat into a false extraction retry.
    async fn renew_current_claim(
        &self,
        intent: &mut MemoryExtractionIntent,
        generation: u64,
    ) -> Result<(), MemoryExtractionError> {
        for _ in 0..8 {
            let mut current = self
                .repository
                .get_extraction(&intent.intent_id)
                .await?
                .ok_or_else(|| MemoryExtractionError::NotFound(intent.intent_id.clone()))?;
            let expected_revision = current.revision;
            current.renew_claim(&self.owner, generation, unix_ms(), self.policy.lease_ms)?;
            match self
                .repository
                .compare_and_swap_extraction(expected_revision, current.clone())
                .await
            {
                Ok(()) => {
                    *intent = current;
                    return Ok(());
                }
                Err(MemoryExtractionError::RevisionConflict(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(MemoryExtractionError::RevisionConflict(
            intent.intent_id.clone(),
        ))
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
                        self.renew_current_claim(intent, generation)
                            .await
                            .map_err(|error| (error.to_string(), false))?;
                    }
                }
            };
            // A driver may persist claim-fenced auxiliary facts (currently the
            // common credential-realization receipt) while extraction is in
            // flight. Reload the same claim before the lifecycle transition so
            // its CAS revision is authoritative instead of being overwritten by
            // the controller's pre-I/O snapshot.
            let current = self
                .repository
                .get_extraction(&intent.intent_id)
                .await
                .map_err(|error| (error.to_string(), false))?
                .ok_or_else(|| {
                    (
                        "Memory extraction disappeared during execution".into(),
                        true,
                    )
                })?;
            current
                .require_claim(&self.owner, generation, unix_ms())
                .map_err(|error| (error.to_string(), false))?;
            *intent = current;
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
        MemoryExtractionIntent::new_range(
            "extract-1",
            "run-7:terminal-3",
            "ws-a",
            "session-1",
            "terminal-3",
            "memory-1",
            2,
            0,
            1,
            vec![Message::text(Id("m1".into()), Role::User, "remember me")],
            MemoryExtractorSnapshot::host_executor(
                "memory-agent",
                "host",
                "model-config-2",
                "host",
            ),
        )
        .unwrap()
    }

    #[test]
    fn extractor_snapshot_decodes_current_and_legacy_storage_shapes() {
        // Cause graph / decision table:
        // C1=stored value has the current complete `agent`; C2=stored value has
        // legacy `agent_id` + `model`; E1=preserve the complete snapshot;
        // E2=migrate through the one canonical Memory Agent constructor.
        // Constraint: exactly one of C1/C2 matches the untagged stored shape.
        // | Rule | C1 | C2 | effect |
        // | R1   | T  | F  | E1     |
        // | R2   | F  | T  | E2     |
        let current = MemoryExtractorSnapshot::host_executor(
            "configured-memory-agent",
            "host",
            "model-config-2",
            "host",
        );
        let current_json = serde_json::to_value(&current).unwrap();
        let decoded_current: MemoryExtractorSnapshot =
            serde_json::from_value(current_json).unwrap();
        assert_eq!(decoded_current, current, "R1");

        let legacy_model = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "legacy-provider",
                "legacy-model",
                "host",
            ),
        );
        let legacy_json = serde_json::json!({
            "agent_id": "legacy-memory-agent",
            "model": legacy_model,
            "instructions": "legacy instructions",
            "extraction_prompt": "extract durable facts"
        });
        let decoded_legacy: MemoryExtractorSnapshot = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(
            decoded_legacy.agent,
            crate::memory_agent("legacy-memory-agent", legacy_model, "legacy instructions"),
            "R2"
        );
        assert_eq!(
            decoded_legacy.extraction_prompt.as_deref(),
            Some("extract durable facts"),
            "R2"
        );
    }

    fn credential_receipt(
        generation: u64,
    ) -> awaken_runtime_contract::CredentialRealizationReceipt {
        let binding = awaken_runtime_contract::AttemptCredentialBinding {
            candidate_fingerprint: awaken_runtime_contract::CandidateFingerprint(
                "candidate-a".into(),
            ),
            credential: awaken_runtime_contract::CredentialRef {
                id: "credential-a".into(),
                revision: 3,
            },
            selected_plaintext_holder: awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
            selected_realization_kind:
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            claim_epoch: generation,
        };
        awaken_runtime_contract::CredentialRealizationReceipt::new(
            &binding,
            binding.selected_realization_kind,
        )
        .unwrap()
    }

    #[test]
    fn credential_receipt_is_fenced_idempotent_and_claim_scoped() {
        // Cause graph / decision table:
        // C1=current owner+generation+lease; C2=identical receipt; C3=new claim.
        // | Rule | C1 | C2 | C3 | result                         |
        // | R1   | T  | -  | F  | persist and bump revision      |
        // | R2   | T  | T  | F  | idempotent, no revision change |
        // | R3   | F  | -  | F  | stale-claim rejection          |
        // | R4   | T  | -  | T  | prior-attempt receipt cleared  |
        let mut intent = intent();
        let generation = intent.claim("worker-a", 100, 50).unwrap();
        let receipt = credential_receipt(generation);
        intent
            .record_credential_realization("worker-a", generation, 110, receipt.clone())
            .unwrap();
        let recorded_revision = intent.revision;
        intent
            .record_credential_realization("worker-a", generation, 111, receipt)
            .unwrap();
        assert_eq!(intent.revision, recorded_revision, "R2");
        assert!(
            intent
                .record_credential_realization(
                    "worker-b",
                    generation,
                    112,
                    credential_receipt(generation),
                )
                .is_err(),
            "R3"
        );
        intent.retry("worker-a", generation, 113, "retry").unwrap();
        let next = intent.claim("worker-b", 200, 50).unwrap();
        assert!(next > generation);
        assert!(intent.credential_realizations.is_empty(), "R4");
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
        // Cause/effect rule: freezing a complete ordinary Agent snapshot may
        // include secret-free permission policy and credential *references*;
        // it must never persist an authorization principal or raw material.
        // `transcript[*].role` remains the Agent message role, not an IAM role.
        for forbidden in ["principal", "api_key", "bearer_token", "password", "secret"] {
            assert!(!text.contains(forbidden));
        }
        assert_eq!(value["workspace_id"], "ws-a");
        assert_eq!(value["memory_config_version"], 2);
    }

    #[derive(Default)]
    struct TestRepository(Mutex<Vec<MemoryExtractionIntent>>);

    #[async_trait]
    impl MemoryExtractionRepository for TestRepository {
        async fn put_extraction_if_absent(
            &self,
            intent: MemoryExtractionIntent,
        ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
            let mut stored = self.0.lock().unwrap();
            match stored.iter().find(|existing| {
                existing.intent_id == intent.intent_id
                    || existing.idempotency_key == intent.idempotency_key
            }) {
                Some(existing) if existing.same_request(&intent) => {
                    Ok(PutMemoryExtractionOutcome::Existing)
                }
                Some(_) => Err(MemoryExtractionError::IdempotencyConflict(
                    intent.idempotency_key,
                )),
                None => {
                    stored.push(intent);
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
                .iter()
                .find(|intent| intent.intent_id == intent_id)
                .cloned())
        }

        async fn extraction_cursor(
            &self,
            session_id: &str,
        ) -> Result<usize, MemoryExtractionError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|intent| intent.session_id == session_id)
                .map(MemoryExtractionIntent::transcript_cursor)
                .max()
                .unwrap_or(0))
        }

        async fn recoverable_extractions(
            &self,
            limit: usize,
        ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|intent| !intent.status.is_terminal())
                .take(limit)
                .cloned()
                .collect())
        }

        async fn compare_and_swap_extraction(
            &self,
            expected_revision: u64,
            intent: MemoryExtractionIntent,
        ) -> Result<(), MemoryExtractionError> {
            let mut stored = self.0.lock().unwrap();
            let Some(position) = stored
                .iter()
                .position(|current| current.intent_id == intent.intent_id)
            else {
                return Err(MemoryExtractionError::NotFound(intent.intent_id));
            };
            if stored[position].revision != expected_revision {
                return Err(MemoryExtractionError::RevisionConflict(intent.intent_id));
            }
            stored[position] = intent;
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
    async fn heartbeat_renews_from_the_latest_claim_revision() {
        // Concurrent claim-revision graph: C1 the controller owns generation G;
        // C2 credential materialization commits a newer revision while inference
        // is running; C3 heartbeat fires. E1 preserve the receipt, E2 renew the
        // same G lease, E3 do not consume a retry attempt.
        let repository = Arc::new(TestRepository::default());
        let controller = MemoryExtractionController::new(repository.clone(), "worker-a")
            .with_policy(MemoryExtractionPolicy {
                lease_ms: 10_000,
                heartbeat_ms: 100,
                max_attempts: 3,
                retry_base_ms: 0,
            });
        controller.enqueue(intent()).await.unwrap();
        let mut claimed = repository
            .get_extraction("extract-1")
            .await
            .unwrap()
            .unwrap();
        let expected_revision = claimed.revision;
        let generation = claimed.claim("worker-a", unix_ms(), 10_000).unwrap();
        repository
            .compare_and_swap_extraction(expected_revision, claimed.clone())
            .await
            .unwrap();

        let mut with_receipt = claimed.clone();
        let expected_revision = with_receipt.revision;
        with_receipt
            .record_credential_realization(
                "worker-a",
                generation,
                unix_ms(),
                credential_receipt(generation),
            )
            .unwrap();
        repository
            .compare_and_swap_extraction(expected_revision, with_receipt.clone())
            .await
            .unwrap();

        controller
            .renew_current_claim(&mut claimed, generation)
            .await
            .expect("C1+C2+C3 renews current state");
        assert_eq!(claimed.claim_generation, generation, "E2");
        assert_eq!(claimed.attempts, 1, "E3");
        assert_eq!(claimed.credential_realizations.len(), 1, "E1");
        assert_eq!(
            repository
                .get_extraction("extract-1")
                .await
                .unwrap()
                .unwrap()
                .credential_realizations
                .len(),
            1,
            "E1 durable"
        );
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

    #[tokio::test]
    async fn terminal_enqueue_uses_a_durable_snapshot_window_and_stable_agent_identity() {
        let repository = Arc::new(TestRepository::default());
        let controller = MemoryExtractionController::new(repository.clone(), "worker-a");
        let first = Message::text(Id("m1".into()), Role::User, "first new fact");
        let recall = Message::text(
            Id(format!("{}-1", crate::RECALL_MESSAGE_ID_PREFIX)),
            Role::System,
            "request-only recall must not be re-extracted",
        );
        let second = Message::text(Id("m2".into()), Role::User, "second new fact");
        let request = |run: &str, transcript: Vec<Message>| MemoryTerminalExtractionRequest {
            workspace_id: "ws-a".into(),
            session_id: "session-1".into(),
            terminal_run_id: run.into(),
            memory_store_id: "memory-1".into(),
            memory_config_version: 2,
            committed_transcript: TranscriptSnapshot::new(
                awaken_agent_contract::agent::thread::Id("session-1".into()),
                awaken_agent_contract::thread::read::transcript::TranscriptView::RawCommitted,
                transcript,
            ),
            extractor: intent().extractor,
        };

        controller
            .enqueue_terminal(request("run-1", vec![first.clone(), recall.clone()]))
            .await
            .unwrap();
        controller
            .enqueue_terminal(request(
                "run-2",
                vec![first.clone(), recall.clone(), second.clone()],
            ))
            .await
            .unwrap();
        assert_eq!(
            controller
                .enqueue_terminal(request("run-2", vec![first, recall, second.clone()]))
                .await
                .unwrap(),
            PutMemoryExtractionOutcome::Existing
        );

        let first_intent = repository
            .get_extraction("memory-extraction:session-1:run-1")
            .await
            .unwrap()
            .unwrap();
        let second_intent = repository
            .get_extraction("memory-extraction:session-1:run-2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (first_intent.transcript_start, first_intent.transcript_end),
            (0, 2)
        );
        assert_eq!(
            first_intent.transcript,
            vec![Message::text(Id("m1".into()), Role::User, "first new fact")]
        );
        assert_eq!(
            first_intent.transcript_ranges,
            vec![TranscriptRange::new(0, 1)]
        );
        assert_eq!(
            first_intent
                .transcript_snapshot
                .as_ref()
                .expect("snapshot")
                .end_seq,
            2
        );
        assert_eq!(
            first_intent.auxiliary_thread_id(),
            "memory-extraction:session-1:run-1/agent"
        );
        assert_eq!(
            first_intent.auxiliary_run_id(),
            "memory-extraction:session-1:run-1/agent/run"
        );
        assert_eq!(
            (second_intent.transcript_start, second_intent.transcript_end),
            (2, 3)
        );
        assert_eq!(
            second_intent.transcript_ranges,
            vec![TranscriptRange::new(2, 3)]
        );
        assert_eq!(second_intent.transcript, vec![second]);
        assert_eq!(repository.extraction_cursor("session-1").await.unwrap(), 3);
    }

    struct TerminalReader(Vec<Message>);

    impl awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView
        for TerminalReader
    {
        fn committed_messages(
            &self,
            _thread_id: &awaken_agent_contract::agent::thread::Id,
        ) -> Vec<Message> {
            self.0.clone()
        }

        fn resume_ticket(
            &self,
            _run_id: &awaken_agent_contract::agent::run::Id,
        ) -> Option<awaken_agent_contract::agent::awaiting::ResumeTicket> {
            None
        }

        fn run(
            &self,
            _run_id: &awaken_agent_contract::agent::run::Id,
        ) -> Option<awaken_agent_contract::agent::run::Record> {
            None
        }

        fn latest_run(
            &self,
            _thread_id: &awaken_agent_contract::agent::thread::Id,
        ) -> Option<awaken_agent_contract::agent::run::Record> {
            None
        }
    }

    #[derive(Default)]
    struct TerminalExtractionRecorder(Mutex<Vec<(String, TranscriptSnapshot)>>);

    #[async_trait]
    impl MemoryTerminalExtraction for TerminalExtractionRecorder {
        async fn extract_terminal(
            &self,
            terminal: &awaken_runtime_contract::terminal::CommittedTerminalRun,
            transcript: TranscriptSnapshot,
        ) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push((terminal.run_id.0.clone(), transcript));
            Ok(())
        }
    }

    #[tokio::test]
    async fn terminal_observer_reads_only_committed_thread_truth() {
        use awaken_runtime_contract::terminal::{CommittedTerminalRun, RunTerminalObserver};

        let committed = vec![Message::text(Id("m1".into()), Role::User, "remember me")];
        let extraction = Arc::new(TerminalExtractionRecorder::default());
        let observer = MemoryTerminalObserver::new(
            Arc::new(TerminalReader(committed.clone())),
            extraction.clone(),
        );

        observer
            .observe(&CommittedTerminalRun {
                run_id: awaken_agent_contract::agent::run::Id("run-7".into()),
                thread_id: awaken_agent_contract::agent::thread::Id("thread-1".into()),
                cause: awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            })
            .await
            .unwrap();

        let observations = extraction.0.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].0, "run-7");
        assert_eq!(observations[0].1.messages(), committed);
        assert_eq!(observations[0].1.reference().end_seq, 1);
    }
}
