// @formal entrypoint memory_extraction.unknown_execution_never_reenters_retry
//! Value objects and terminal outcomes for durable Memory extraction.

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
    pub observed_generation: Option<awaken_resource_contract::MemoryGeneration>,
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
