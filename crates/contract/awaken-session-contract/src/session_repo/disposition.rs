//! Durable Session visibility and deletion admission.
//!
//! The parent aggregate owns persistence and terminal cleanup. This module owns
//! only the closed disposition state and the pure plan used by its one delete
//! reducer.

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
            Self::Archived { archived_at } => Some(archived_at),
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
pub(super) struct SessionDeleteRequestPlan {
    pub(super) transition_to_deleting: bool,
    pub(super) terminalize_execution: bool,
    pub(super) request_cleanup: bool,
}

/// Closed reducer plan for the durable Delete-intent transaction. Deleting and
/// Deleted are absorbing replays; every admitted request hides the aggregate,
/// fences nonterminal execution, and requests recoverable cleanup together.
#[must_use]
pub(super) const fn session_delete_request_plan(
    disposition: SessionDeleteDispositionClass,
    execution_terminal: bool,
) -> SessionDeleteRequestPlan {
    let transition_to_deleting = matches!(
        disposition,
        SessionDeleteDispositionClass::Active | SessionDeleteDispositionClass::Archived
    );
    SessionDeleteRequestPlan {
        transition_to_deleting,
        terminalize_execution: transition_to_deleting && !execution_terminal,
        request_cleanup: transition_to_deleting,
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionDispositionTransitionError {
    #[error("cannot archive a Session while deletion is in progress or complete")]
    ArchiveAfterDelete,
}
