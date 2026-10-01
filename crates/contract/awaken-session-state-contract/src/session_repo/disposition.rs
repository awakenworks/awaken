// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
//! Durable Session visibility and deletion admission.
//!
//! The parent aggregate owns persistence and terminal cleanup. This module owns
//! only the closed disposition state and the pure plan used by its one delete
//! reducer.

use crate::SessionId;
/// Durable retention and public-visibility state of one Session.
///
/// `Deleting` is committed before physical cleanup starts. `Deleted` is retained
/// for imported historical rows and compact tombstone projections; ordinary new
/// deletions remove the aggregate only after cleanup receipts are committed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionDisposition {
    #[default]
    Active,
    Archived {
        archived_at: String,
        /// The Application clock sampled while constructing the first archive
        /// transition that commits, before its durable CAS. Legacy archived
        /// rows have no trustworthy observation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        archived_at_unix_ms: Option<u64>,
    },
    Deleting,
    Deleted,
}

impl SessionDisposition {
    #[must_use]
    pub const fn denies_activity(&self) -> bool {
        !matches!(self, Self::Active)
    }

    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        matches!(self, Self::Deleting | Self::Deleted)
    }

    #[must_use]
    pub fn archived_at(&self) -> Option<&str> {
        match self {
            Self::Archived { archived_at, .. } => Some(archived_at),
            Self::Active | Self::Deleting | Self::Deleted => None,
        }
    }

    #[must_use]
    pub const fn archived_at_unix_ms(&self) -> Option<u64> {
        match self {
            Self::Archived {
                archived_at_unix_ms,
                ..
            } => *archived_at_unix_ms,
            Self::Active | Self::Deleting | Self::Deleted => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum SessionDeleteDispositionClass {
    Active,
    Archived,
    Deleting,
    Deleted,
}

impl From<&SessionDisposition> for SessionDeleteDispositionClass {
    fn from(value: &SessionDisposition) -> Self {
        match value {
            SessionDisposition::Active => Self::Active,
            SessionDisposition::Archived { .. } => Self::Archived,
            SessionDisposition::Deleting => Self::Deleting,
            SessionDisposition::Deleted => Self::Deleted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SessionDeleteLifecycleFactPlan<'a> {
    pub(super) session_id: &'a SessionId,
    pub(super) owner_scope: &'a str,
    pub(super) requested_at_unix_s: i64,
}

impl SessionDeleteLifecycleFactPlan<'_> {
    fn into_fact(self) -> crate::ManagedLifecycleFact {
        crate::ManagedLifecycleFact {
            id: format!("session:{}:deleted", self.session_id),
            object_id: self.session_id.to_string(),
            workspace_id: Some(self.owner_scope.to_string()),
            event_type: "session.deleted".into(),
            timestamp: self.requested_at_unix_s,
            runtime_interval: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SessionDeleteCommitPlan<'a> {
    terminalize_execution: bool,
    cleanup_identity: crate::terminal_cleanup::SessionCleanupRequestIdentity<'a>,
    lifecycle_fact: SessionDeleteLifecycleFactPlan<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionDeleteRequestPlan<'a> {
    Commit(SessionDeleteCommitPlan<'a>),
    Replay,
}

impl SessionDeleteRequestPlan<'_> {
    #[cfg(kani)]
    pub(super) const fn transitioned(&self) -> bool {
        matches!(self, Self::Commit(_))
    }

    pub(super) const fn commit_parts(
        &self,
    ) -> Option<(
        bool,
        crate::terminal_cleanup::SessionCleanupRequestIdentity<'_>,
    )> {
        match self {
            Self::Commit(plan) => Some((plan.terminalize_execution, plan.cleanup_identity)),
            Self::Replay => None,
        }
    }

    #[cfg(kani)]
    pub(super) const fn lifecycle_fact_plan(&self) -> Option<SessionDeleteLifecycleFactPlan<'_>> {
        match self {
            Self::Commit(plan) => Some(plan.lifecycle_fact),
            Self::Replay => None,
        }
    }

    pub(super) fn into_lifecycle_fact(self) -> Option<crate::ManagedLifecycleFact> {
        match self {
            Self::Commit(plan) => Some(plan.lifecycle_fact.into_fact()),
            Self::Replay => None,
        }
    }
}

/// Closed reducer plan for the durable Delete-intent transaction. Deleting and
/// Deleted are absorbing replays; every admitted request hides the aggregate,
/// fences nonterminal execution, and requests recoverable cleanup together.
#[must_use]
pub(super) fn session_delete_request_plan<'a>(
    disposition: SessionDeleteDispositionClass,
    execution_terminal: bool,
    session_id: &'a SessionId,
    owner_scope: &'a str,
    requested_at_unix_s: i64,
) -> SessionDeleteRequestPlan<'a> {
    match disposition {
        SessionDeleteDispositionClass::Active | SessionDeleteDispositionClass::Archived => {
            SessionDeleteRequestPlan::Commit(SessionDeleteCommitPlan {
                terminalize_execution: !execution_terminal,
                cleanup_identity: crate::terminal_cleanup::SessionCleanupRequestIdentity::new(
                    session_id,
                ),
                lifecycle_fact: SessionDeleteLifecycleFactPlan {
                    session_id,
                    owner_scope,
                    requested_at_unix_s,
                },
            })
        }
        SessionDeleteDispositionClass::Deleting | SessionDeleteDispositionClass::Deleted => {
            SessionDeleteRequestPlan::Replay
        }
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionDispositionTransitionError {
    #[error("cannot archive a Session while deletion is in progress or complete")]
    ArchiveAfterDelete,
    #[error("new Session archive requires an Application time observation")]
    ArchiveTimeRequired,
}

#[cfg(test)]
mod time_tests {
    use super::SessionDisposition;

    #[test]
    fn archived_time_provenance_round_trips_without_backfilling_legacy_rows() {
        // Causes: D1 historical archived JSON has only its v1 display string;
        // D2 a new archive persists typed time 0, a valid boundary. Effects:
        // D1 deserializes unknown and reserializes byte-equivalent fields;
        // D2 retains Some(0), never conflating it with historical missing.
        let historical = serde_json::json!({
            "state": "archived", "archived_at": "2026-01-01T00:00:00Z"
        });
        let old: SessionDisposition = serde_json::from_value(historical.clone()).unwrap();
        assert_eq!(old.archived_at_unix_ms(), None, "D1");
        assert_eq!(serde_json::to_value(old).unwrap(), historical, "D1");
        let new = SessionDisposition::Archived {
            archived_at: "2026-01-01T00:00:00Z".into(),
            archived_at_unix_ms: Some(0),
        };
        let new_wire = serde_json::to_value(&new).unwrap();
        assert_eq!(new_wire["archived_at_unix_ms"], 0, "D2");
        assert_eq!(
            serde_json::from_value::<SessionDisposition>(new_wire).unwrap(),
            new,
            "D2"
        );
    }
}
