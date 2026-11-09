// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This native preference contract preserves the exact owner tuple,
// @formal reason: generation and immutable original receipt in the existing configuration
// @formal reason: store. It neither authorizes Agent use nor changes definitions. Shared
// @formal reason: SQLite/PostgreSQL conformance covers its transaction boundary; this
// @formal reason: classification is not a formal-proof claim.
//! Account-owned Agent selection is a preference, never a grant or an Agent definition.
//! HTTP admission derives the owner from verified native Account identity. Reads of the
//! original receipt are independent of whether the selected Agent is still usable.

use serde::{Deserialize, Serialize};

/// The native identity coordinates, never an Actor id or a display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSelectionOwner {
    issuer: String,
    account: String,
}

fn exact(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

impl AgentSelectionOwner {
    /// Validate coordinates already verified by the native identity boundary.
    /// This constructor performs no authentication and confers no authority.
    pub fn new(issuer: String, account: String) -> Result<Self, AgentSelectionError> {
        if !exact(&issuer, 2048) || !exact(&account, 300) {
            return Err(AgentSelectionError::Invalid);
        }
        Ok(Self { issuer, account })
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }
}

/// A confirmed desired preference against one exact observed generation.
/// The edge never accepts owner, issuer, Workspace, authority or definition here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSelectionIntent {
    pub request_key: String,
    pub expected_generation: u64,
    pub selected: bool,
}

impl AgentSelectionIntent {
    pub fn validate(&self) -> Result<(), AgentSelectionError> {
        if !exact(&self.request_key, 255) {
            return Err(AgentSelectionError::Invalid);
        }
        Ok(())
    }
}

/// Generation zero denotes absence. A persisted state always has positive generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSelectionState {
    pub generation: u64,
    pub selected: bool,
}

/// Immutable original-outcome metadata: no manifest, definition, permission or conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSelectionReceipt {
    pub intent: AgentSelectionIntent,
    pub applied_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSelectionDecision {
    Apply {
        state: AgentSelectionState,
        receipt: AgentSelectionReceipt,
    },
    Replay(AgentSelectionReceipt),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentSelectionError {
    #[error("invalid Agent selection coordinates or original intent")]
    Invalid,
    #[error("Agent selection changed; current generation is {current_generation}")]
    Conflict { current_generation: u64 },
    #[error("original Agent selection key was reused with different content")]
    KeyReuse,
    #[error("Agent selection generation is exhausted")]
    GenerationExhausted,
}

/// Decide inside the repository's owner/Workspace/Agent transaction. The prior
/// receipt is looked up by that exact tuple and original key. It takes precedence
/// over the mutable preference: replaying an old selection must not re-select an
/// Agent after the person subsequently removed it.
pub fn decide_agent_selection(
    current: Option<AgentSelectionState>,
    original: Option<&AgentSelectionReceipt>,
    intent: &AgentSelectionIntent,
) -> Result<AgentSelectionDecision, AgentSelectionError> {
    intent.validate()?;
    if let Some(receipt) = original {
        if receipt.intent != *intent {
            return Err(AgentSelectionError::KeyReuse);
        }
        return Ok(AgentSelectionDecision::Replay(receipt.clone()));
    }
    let generation = current.map_or(0, |state| state.generation);
    if generation != intent.expected_generation {
        return Err(AgentSelectionError::Conflict {
            current_generation: generation,
        });
    }
    let next = generation
        .checked_add(1)
        .ok_or(AgentSelectionError::GenerationExhausted)?;
    Ok(AgentSelectionDecision::Apply {
        state: AgentSelectionState {
            generation: next,
            selected: intent.selected,
        },
        receipt: AgentSelectionReceipt {
            intent: intent.clone(),
            applied_generation: next,
        },
    })
}

/// Mandatory, narrow port for this Control aggregate. Implementations share the
/// existing Config database; this repository neither stores nor grants authority.
#[async_trait::async_trait]
pub trait PersonalAgentSelectionRepository: Send + Sync {
    async fn personal_agent_selection(
        &self,
        scope: &awaken_agent_contract::ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
    ) -> Result<Option<AgentSelectionState>, crate::ConfigStoreError>;

    /// Content-free original outcome, independent of current Agent visibility.
    async fn personal_agent_selection_outcome(
        &self,
        scope: &awaken_agent_contract::ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
        request_key: &str,
    ) -> Result<Option<AgentSelectionReceipt>, crate::ConfigStoreError>;

    /// Atomically CAS the preference and append its immutable original receipt.
    /// The HTTP owner still verifies current identity and operation authority.
    async fn set_personal_agent_selection(
        &self,
        scope: &awaken_agent_contract::ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
        intent: &AgentSelectionIntent,
    ) -> Result<Result<AgentSelectionReceipt, AgentSelectionError>, crate::ConfigStoreError>;
}

#[cfg(test)]
mod tests;
