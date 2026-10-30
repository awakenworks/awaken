// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Existing Session Core scan and persistence-failure vocabulary.
// @formal reason: The closed error policy maps authority outages to Retry, corrupt
// @formal reason: durable truth to Quarantine, and command/CAS refusals to Reject;
// @formal reason: stores retain row decoding and quarantine ownership. This is the
// @formal reason: product requirement classification of unchanged policy, not a
// @formal reason: claim that the policy or an external store has a direct proof.
//! Shared Session scan vocabulary and persistence-failure policy.
//!
//! Durable stores remain the only row-decoding and quarantine owners. This
//! module supplies their closed page cursor and result vocabulary; it does not
//! persist scheduling state or introduce another recovery registry.

use super::{PersistedSession, ScopedPersistedSession, SessionRepositoryError};
use crate::state::SessionId;

/// Closed supervisor policy for persistence failures. Retryable outages remain
/// pending; corrupt durable truth is isolated for operator repair; command and
/// concurrency failures are returned without background replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRepositoryRecoveryAction {
    Retry,
    Quarantine,
    Reject,
}

use awaken_session_types::SessionRecoveryQuarantine;

/// Opaque keyset position for the shared Session reconciliation index.
///
/// The position carries no durable ownership or scheduling authority. It is
/// valid only as the exclusive lower bound of a subsequent read from the same
/// repository authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionScanCursor(SessionId);

impl SessionScanCursor {
    /// Project one already-read durable row identity into the next-page lower
    /// bound. Store implementations are the normal constructor; callers only
    /// carry the typed value returned by [`SessionScan`].
    #[must_use]
    pub fn after_session_id(session_id: impl Into<String>) -> Self {
        Self(crate::state::SessionId::from(session_id.into()))
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.0
    }
}

/// The closed set of SELECT-only durable Session selections a reader answers.
///
/// These complete, unpaged queries never maintain quarantine evidence. Each
/// fails closed with
/// [`SessionRepositoryError::Corrupt`] on any undecodable row.
///
/// S2: SELECT queries remain constructible; the former maintenance selection
/// cannot be supplied through this read capability. Reconciliation keeps the
/// existing cursor/result values on its required repository method instead.
///
/// ```
/// use awaken_session_contract::SessionScanQuery;
/// let query = SessionScanQuery::Workspace { workspace_id: "w" };
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_contract::SessionScanQuery;
/// let query = SessionScanQuery::Reconcilable { after: None };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionScanQuery<'a> {
    /// Every visible aggregate owned by one Workspace, read from durable truth
    /// so a cold protocol projection cache cannot hide existing Sessions.
    Workspace { workspace_id: &'a str },
    /// Complete, unpaged and fail-closed read of roots whose durable
    /// realization lease names one logical Worker. Expired leases still
    /// count: only the owning Session cleanup receipt can release affinity.
    /// The application, not a SQL predicate, decides whether a matching
    /// root has unfinished cleanup.
    WorkerRealizationOwner { worker_id: &'a str },
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

/// Result of a SELECT-only query or repository reconciliation maintenance.
/// Store adapters own decoding and quarantine; the same result vocabulary
/// carries maintained isolation evidence without another recovery registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionScan {
    pub sessions: Vec<ScopedPersistedSession>,
    /// Isolation evidence produced only by repository reconciliation maintenance.
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
            Self::AuthorityUnavailable(_) => SessionRepositoryRecoveryAction::Retry,
            Self::Corrupt(_) => SessionRepositoryRecoveryAction::Quarantine,
            Self::NotFound | Self::Conflict(_) | Self::InvalidMutation(_) => {
                SessionRepositoryRecoveryAction::Reject
            }
        }
    }
}
