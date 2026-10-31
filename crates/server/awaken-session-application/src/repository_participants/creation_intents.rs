// @formal surface obligation=session_resources.external_io_requires_durable_prepared_state
// @formal reason: A prepared Session participant is recorded durably before its external
// @formal reason: prepare and recovered without the original caller (ADR-0102 D1).
//! Pre-root creation intents for prepared Session participants (ADR-0102 D1).
//!
//! A Session-owned Repository is prepared before its Session root exists. The
//! prepare first records a durable intent in the Session store; the root
//! insert consumes it, and an abort claims it before retiring anything, so
//! adoption and retirement exclude each other. A caller that disappears
//! leaves the intent behind: once its lease expires (or at once, when an
//! abort already claimed it) the recovery cycle completes the abort.

use awaken_session_contract::SessionRepositoryError;
use awaken_session_contract::{PreparedParticipantIntentClaim, PreparedParticipantIntentRecord};
use awaken_session_types::{RunError, SessionId};
use awaken_time::WallClockExt as _;

use super::{
    SessionRepositoryOwner, SessionRepositoryParticipants, session_resources_reference_repository,
};

/// How long a preparing caller owns its intent before recovery may abort it.
/// It bounds prepare -> root insert, which is one request.
pub(crate) const CREATION_INTENT_LEASE_MS: u64 = 10 * 60 * 1000;

/// Most intents one recovery cycle drives.
const RECOVERY_BATCH: usize = 50;

/// What the last step of an unadopted participant's retirement may do.
pub(crate) enum AbortAdoption {
    /// The intent is claimed: no root can adopt the participant any more.
    Claimed,
    /// A root adopted the participant after the caller's adoption check.
    Adopted,
    /// No intent remains and no root adopts it: retire as before.
    Unclaimed,
}

/// Recovery progress over durable creation intents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CreationIntentReport {
    pub(crate) settled: usize,
    pub(crate) failures: usize,
}

impl SessionRepositoryParticipants {
    /// Record the participant under its Session's creation intent before the
    /// external prepare, so the prepare is never known only to this call.
    pub(crate) async fn record_creation_intent(
        &self,
        owner: &SessionRepositoryOwner,
        owner_scope: &str,
        repository_id: &str,
    ) -> Result<(), RunError> {
        let lease = self.clock.now_ms().saturating_add(CREATION_INTENT_LEASE_MS);
        match self
            .intents()
            .record_creation_intent(
                owner_scope,
                &SessionId::from(owner.session_id()),
                repository_id,
                lease,
            )
            .await
        {
            Ok(
                PreparedParticipantIntentRecord::Recorded
                | PreparedParticipantIntentRecord::RootExists,
            ) => Ok(()),
            Err(SessionRepositoryError::Conflict(_)) => Err(RunError::bad_request(
                "this Session identity is being retired; create the Session under a new id",
            )),
            Err(SessionRepositoryError::InvalidMutation(message)) => {
                Err(RunError::bad_request(message))
            }
            Err(error) => Err(RunError::unavailable(format!(
                "Session creation intent could not be recorded: {error}"
            ))),
        }
    }

    /// Claim the Session's intent right before retiring an unadopted
    /// participant. A root that adopted it meanwhile wins.
    pub(crate) async fn fence_creation_abort(
        &self,
        session_id: &str,
        repository_id: &str,
    ) -> Option<AbortAdoption> {
        let session_id = SessionId::from(session_id);
        match self.intents().claim_creation_abort(&session_id).await {
            Ok(PreparedParticipantIntentClaim::Claimed(_)) => Some(AbortAdoption::Claimed),
            Ok(PreparedParticipantIntentClaim::Absent) => {
                match self.mutations().repository().get(&session_id).await {
                    Ok(root) => Some(
                        if session_resources_reference_repository(&root.resources, repository_id) {
                            AbortAdoption::Adopted
                        } else {
                            AbortAdoption::Unclaimed
                        },
                    ),
                    Err(SessionRepositoryError::NotFound) => Some(AbortAdoption::Unclaimed),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    /// Retire a Session's claimed intent once every participant is retired.
    pub(crate) async fn complete_creation_abort(&self, session_id: &str) -> bool {
        self.intents()
            .complete_creation_abort(&SessionId::from(session_id))
            .await
            .is_ok()
    }

    /// Complete every abandoned or claimed creation intent: claim it, retire
    /// each participant it names, then retire the intent. A failure leaves the
    /// intent `Aborting` for the next cycle.
    pub(crate) async fn reconcile_creation_intents(&self) -> CreationIntentReport {
        let mut report = CreationIntentReport::default();
        let intents = match self
            .intents()
            .recoverable_creation_intents(self.clock.now_ms(), RECOVERY_BATCH)
            .await
        {
            Ok(intents) => intents,
            Err(error) => {
                tracing::warn!(%error, "Session creation intent scan failed");
                report.failures += 1;
                return report;
            }
        };
        for intent in intents {
            let claimed = match self
                .intents()
                .claim_creation_abort(intent.session_id())
                .await
            {
                Ok(PreparedParticipantIntentClaim::Claimed(claimed)) => claimed,
                // A root adopted it, or another recoverer finished first.
                Ok(PreparedParticipantIntentClaim::Absent) => {
                    report.settled += 1;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(session = %intent.session_id(), %error, "Session creation intent claim failed");
                    report.failures += 1;
                    continue;
                }
            };
            let mut retired = true;
            for repository_id in claimed.repository_ids() {
                let Some(owner) = SessionRepositoryOwner::from_repository_id(repository_id)
                    .filter(|owner| owner.session_id() == claimed.session_id().as_str())
                else {
                    continue;
                };
                retired &= self
                    .retire_owned(claimed.owner_scope(), &owner, repository_id)
                    .await;
            }
            if retired
                && self
                    .complete_creation_abort(claimed.session_id().as_str())
                    .await
            {
                report.settled += 1;
            } else {
                tracing::warn!(
                    session = %claimed.session_id(),
                    "Session creation abort remains pending for recovery"
                );
                report.failures += 1;
            }
        }
        report
    }
}
