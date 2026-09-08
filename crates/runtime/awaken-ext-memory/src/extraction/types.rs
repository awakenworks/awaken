//! Value objects and terminal outcomes for durable Memory extraction.

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
    ExecutionIndeterminate,
    TerminalFailed,
}

impl MemoryExtractionStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::ExecutionIndeterminate | Self::TerminalFailed
        )
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
