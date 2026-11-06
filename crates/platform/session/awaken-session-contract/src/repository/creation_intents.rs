// @formal surface obligation=session_resources.external_io_requires_durable_prepared_state
// @formal reason: A prepared Session participant is recorded durably before its external
// @formal reason: prepare and recovered without the original caller (ADR-0102 D1).
//! Durable pre-root creation intents (ADR-0102 D1).
//!
//! A Session-owned participant (a Repository registered in the Resource
//! Registry) is prepared before the Session root exists. The intent records
//! that responsibility in the Session store so a rebuilt process can find it
//! without the original caller, and so adoption and abort exclude each other:
//! the root insert consumes a `Preparing` intent in its own transaction, and
//! an abort claims it (`Preparing -> Aborting`) before it retires anything.
//! A completed local abort retains its identity fence even without a root.

use async_trait::async_trait;

use super::SessionId;
use super::SessionRepositoryError;

/// Where a durable creation intent stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedParticipantIntentPhase {
    /// A create may still adopt the prepared participants.
    Preparing,
    /// An abort claimed the intent; no root can adopt it any more, and the
    /// participants are retired by whoever completes the abort.
    Aborting,
    /// The local abort completed. The retained identity cannot be prepared or
    /// adopted again. This is not an external Registry/Vault close receipt.
    Aborted,
}

impl PreparedParticipantIntentPhase {
    /// Only an open preparation may be adopted by a Root transaction.
    #[must_use]
    pub const fn permits_adoption(self) -> bool {
        matches!(self, Self::Preparing)
    }
}

/// One durable pre-root responsibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedParticipantIntent {
    session_id: SessionId,
    /// The owner scope (Workspace) the participants live in.
    owner_scope: String,
    /// Every participant id prepared under this intent, in first-prepared order.
    repository_ids: Vec<String>,
    phase: PreparedParticipantIntentPhase,
    /// Until this instant the preparing caller still owns the intent; after
    /// it, recovery may claim a `Preparing` intent. An `Aborting` intent is
    /// recoverable at once.
    lease_expires_at_unix_ms: u64,
}

impl PreparedParticipantIntent {
    /// Rebuild an intent as the durable store recorded it.
    pub fn new(
        session_id: SessionId,
        owner_scope: String,
        repository_ids: Vec<String>,
        phase: PreparedParticipantIntentPhase,
        lease_expires_at_unix_ms: u64,
    ) -> Self {
        Self {
            session_id,
            owner_scope,
            repository_ids,
            phase,
            lease_expires_at_unix_ms,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn owner_scope(&self) -> &str {
        &self.owner_scope
    }

    pub fn repository_ids(&self) -> &[String] {
        &self.repository_ids
    }

    pub fn into_repository_ids(self) -> Vec<String> {
        self.repository_ids
    }

    pub fn phase(&self) -> PreparedParticipantIntentPhase {
        self.phase
    }

    pub fn lease_expires_at_unix_ms(&self) -> u64 {
        self.lease_expires_at_unix_ms
    }

    /// The abort claim: no root can adopt the intent any more.
    #[must_use]
    pub fn begin_abort(mut self) -> Self {
        if self.phase == PreparedParticipantIntentPhase::Preparing {
            self.phase = PreparedParticipantIntentPhase::Aborting;
        }
        self
    }
}

/// Outcome of recording a prepare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedParticipantIntentRecord {
    /// The intent is durable (new, or extended with this participant).
    Recorded,
    /// The Session root already exists; its own references decide adoption,
    /// so no pre-root intent is kept.
    RootExists,
}

/// Outcome of claiming an intent for abort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparedParticipantIntentClaim {
    /// The intent is `Aborting` or retains its completed `Aborted` fence:
    /// no root can adopt it, and a replay cannot reopen it.
    Claimed(PreparedParticipantIntent),
    /// No intent exists: a root adopted it, none was recorded, or a legacy
    /// abort left no retained fence. Absence alone is not a closure proof.
    Absent,
}

/// Outbound port over durable pre-root creation intents.
///
/// The same durable store implements [`SessionRepository`](super::SessionRepository):
/// its `create` consumes a `Preparing` intent in the root-insert transaction
/// and refuses `Aborting` and retained `Aborted` identities as
/// `Conflict(Tombstoned)`.
#[async_trait]
pub trait PreparedParticipantIntents: Send + Sync {
    /// Record `repository_id` under the Session's intent before it is
    /// prepared, extending the lease. Refuses `Aborting`/`Aborted` intents as
    /// `Conflict(Tombstoned)`: that identity is being retired.
    ///
    /// Caller: the Session application's participant prepare.
    async fn record_creation_intent(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        repository_id: &str,
        lease_expires_at_unix_ms: u64,
    ) -> Result<PreparedParticipantIntentRecord, SessionRepositoryError>;

    /// Atomically move the intent to `Aborting` (idempotent).
    ///
    /// Caller: participant compensation and creation-intent recovery.
    async fn claim_creation_abort(
        &self,
        session_id: &SessionId,
    ) -> Result<PreparedParticipantIntentClaim, SessionRepositoryError>;

    /// Retain a completed `Aborting` intent as `Aborted`. Absence is success;
    /// replay never removes or reopens an existing terminal fence.
    ///
    /// Caller: the same compensation after every participant is retired.
    async fn complete_creation_abort(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionRepositoryError>;

    /// Intents recovery owns now: every `Aborting` intent and every
    /// `Preparing` intent whose lease expired at `now_unix_ms`, oldest lease
    /// first, at most `limit`.
    ///
    /// Caller: the Session recovery cycle.
    async fn recoverable_creation_intents(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<PreparedParticipantIntent>, SessionRepositoryError>;
}

#[cfg(test)]
mod phase_tests {
    use super::*;

    #[test]
    fn only_preparing_adopts_and_completed_abort_is_absorbing() {
        // DT-P: C1 Preparing/Aborting/Aborted. P1 only Preparing permits
        // adoption; P2 beginning abort maps Preparing to Aborting; P3 both
        // already-aborting and completed-abort replay preserve their phase.
        // Every row preserves the original identity, participants and lease.
        // Moving this test module after the port changes no behavior or rule;
        // this same three-row partition remains the complete phase coverage.
        for (phase, adopts, after) in [
            (
                PreparedParticipantIntentPhase::Preparing,
                true,
                PreparedParticipantIntentPhase::Aborting,
            ),
            (
                PreparedParticipantIntentPhase::Aborting,
                false,
                PreparedParticipantIntentPhase::Aborting,
            ),
            (
                PreparedParticipantIntentPhase::Aborted,
                false,
                PreparedParticipantIntentPhase::Aborted,
            ),
        ] {
            assert_eq!(phase.permits_adoption(), adopts);
            let intent = PreparedParticipantIntent::new(
                "session".into(),
                "workspace".into(),
                vec!["repository".into()],
                phase,
                100,
            );
            let replay = intent.clone().begin_abort();
            assert_eq!(replay.phase(), after);
            assert_eq!(replay.session_id(), intent.session_id());
            assert_eq!(replay.owner_scope(), intent.owner_scope());
            assert_eq!(replay.repository_ids(), intent.repository_ids());
            assert_eq!(replay.lease_expires_at_unix_ms(), 100);
        }
    }
}
