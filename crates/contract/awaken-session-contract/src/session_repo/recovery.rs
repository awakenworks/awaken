//! Shared Session scan vocabulary and persistence-failure policy.
//!
//! Durable stores remain the only row-decoding and quarantine owners. This
//! module supplies their closed page cursor and result vocabulary; it does not
//! persist scheduling state or introduce another recovery registry.

use super::{PersistedSession, ScopedPersistedSession, SessionRepositoryError};

/// Closed supervisor policy for persistence failures. Retryable outages remain
/// pending; corrupt durable truth is isolated for operator repair; command and
/// concurrency failures are returned without background replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRepositoryRecoveryAction {
    Retry,
    Quarantine,
    Reject,
}

/// One corrupt durable Session row isolated by the authoritative store scan.
/// The raw aggregate is deliberately absent so recovery reporting cannot leak
/// persisted configuration or credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecoveryQuarantine {
    pub session_id: crate::SessionId,
    pub reason: String,
}

/// Opaque keyset position for the shared Session reconciliation index.
///
/// The position carries no durable ownership or scheduling authority. It is
/// valid only as the exclusive lower bound of a subsequent read from the same
/// repository authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionScanCursor(String);

impl SessionScanCursor {
    /// Project one already-read durable row identity into the next-page lower
    /// bound. Store implementations are the normal constructor; callers only
    /// carry the typed value returned by [`SessionScan`].
    #[must_use]
    pub fn after_session_id(session_id: impl Into<String>) -> Self {
        Self(session_id.into())
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.0
    }
}

/// The closed set of durable Session selections a repository answers.
///
/// Each query fixes its own failure policy so a caller cannot choose a weaker
/// one: only the reconciliation index quarantines undecodable rows and pages;
/// the remaining queries are complete, unpaged reads that fail closed with
/// [`SessionRepositoryError::Corrupt`] on any undecodable row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionScanQuery<'a> {
    /// Sessions carrying any durable Resource, MCP, environment, or WorkQueue
    /// projection reconciliation work, in one fixed-size keyset page. The row
    /// scan preserves the intrinsic Workspace partition and revalidates prior
    /// quarantine records; application coordinators filter by their owned state
    /// machine. One index avoids parallel per-feature recovery registries.
    Reconcilable {
        after: Option<&'a SessionScanCursor>,
    },
    /// Every visible aggregate owned by one Workspace, read from durable truth
    /// so a cold protocol projection cache cannot hide existing Sessions.
    Workspace { workspace_id: &'a str },
    /// Healthy (non-terminal) Sessions whose immutable authoring baseline
    /// references one Vault in one Workspace.
    VaultReferences {
        workspace_id: &'a str,
        vault_id: &'a str,
    },
    /// Healthy Sessions whose current desired MCP attachments reference one
    /// exact credential source. Callers supply a source id only from the
    /// credential authority's committed rollout provenance; this index is
    /// additive to Vault authoring references and neither substitutes for the
    /// other.
    CredentialSourceReferences {
        workspace_id: &'a str,
        source_id: &'a awaken_credential_contract::CredentialSourceId,
    },
}

/// Complete result of one [`SessionScanQuery`]. Store adapters own row decoding
/// and durable quarantine, so callers receive healthy Sessions and isolation
/// evidence from one authority instead of maintaining a parallel registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionScan {
    pub sessions: Vec<ScopedPersistedSession>,
    /// Isolation evidence; only [`SessionScanQuery::Reconcilable`] produces it.
    pub quarantined: Vec<SessionRecoveryQuarantine>,
    /// Exclusive keyset lower bound for the next fixed-size page. `None`
    /// proves this scan reached the end of the current index snapshot; unpaged
    /// queries always return `None`.
    pub next_cursor: Option<SessionScanCursor>,
}

impl SessionScan {
    /// Project a fail-closed query result onto its aggregates.
    #[must_use]
    pub fn into_sessions(self) -> Vec<PersistedSession> {
        self.sessions
            .into_iter()
            .map(|scoped| scoped.session)
            .collect()
    }
}

impl SessionRepositoryError {
    #[must_use]
    pub const fn recovery_action(&self) -> SessionRepositoryRecoveryAction {
        match self {
            Self::Unavailable(_) => SessionRepositoryRecoveryAction::Retry,
            Self::Corrupt(_) => SessionRepositoryRecoveryAction::Quarantine,
            Self::NotFound | Self::Conflict(_) | Self::InvalidMutation(_) => {
                SessionRepositoryRecoveryAction::Reject
            }
        }
    }
}
