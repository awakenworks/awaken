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
//! An intent therefore exists only while no root exists.

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
        self.phase = PreparedParticipantIntentPhase::Aborting;
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
    /// The intent is now `Aborting` (or already was): no root can adopt it.
    Claimed(PreparedParticipantIntent),
    /// No intent exists: a root adopted it, an abort already completed, or
    /// none was recorded. The caller decides from the durable root.
    Absent,
}

/// Outbound port over durable pre-root creation intents.
///
/// The same durable store implements [`SessionRepository`](super::SessionRepository):
/// its `create` consumes a `Preparing` intent in the root-insert transaction
/// and refuses an `Aborting` one as `Conflict(Tombstoned)`.
#[async_trait]
pub trait PreparedParticipantIntents: Send + Sync {
    /// Record `repository_id` under the Session's intent before it is
    /// prepared, extending the lease. Refuses an `Aborting` intent as
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

    /// Retire a completed `Aborting` intent. Absence is success.
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
