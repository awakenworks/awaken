//! Durable activity fencing for overlapping Session turns.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::stream::sink::Sink;
use awaken_session_contract::{
    ManagedLifecycleFact, PersistedSession, RunError, SessionExecutionState,
    SessionRuntimeInterval, StepOutcome,
};

use super::{SessionApplication, SessionMutationError, mutation::repository_failure};

pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

pub(crate) fn runtime_interval_fact(
    owner_scope: &str,
    session_id: &str,
    interval: SessionRuntimeInterval,
) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: interval.interval_id.clone(),
        object_id: session_id.to_string(),
        workspace_id: Some(owner_scope.to_string()),
        event_type: "session.runtime_interval_closed".to_string(),
        timestamp: i64::try_from(interval.ended_at_unix_ms / 1_000).unwrap_or(i64::MAX),
        runtime_interval: Some(interval),
    }
}

/// Failure from a Session activity transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionActivityError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is terminal and cannot begin another activity")]
    Terminal,
    #[error("Session is not ready for activity")]
    NotReady,
    #[error("Session list-cost budget has been reached")]
    BudgetReached,
    #[error("Session activity epoch is exhausted")]
    EpochExhausted,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session activity persistence is unavailable: {0}")]
    Unavailable(String),
}

/// One committed Runtime step together with the durable Session state after its
/// activity fence has been settled.
pub struct SessionMessageOutcome {
    pub step: StepOutcome,
    pub session: PersistedSession,
}

impl SessionActivityError {
    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::Unavailable(
                "activity mutation idempotency key unexpectedly changed payload".into(),
            ),
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }

    fn run_error(self) -> RunError {
        match self {
            Self::NotFound => RunError::bad_request("Session was not found"),
            Self::Terminal => RunError::bad_request("Session no longer accepts new messages"),
            Self::NotReady => RunError::unavailable_classified(
                "session_not_ready",
                "Session realization has not completed",
            ),
            Self::BudgetReached => RunError::classified(
                "budget_reached",
                "Session list-cost budget has been reached",
            ),
            Self::EpochExhausted => RunError::internal("Session activity epoch is exhausted"),
            Self::Conflict => RunError::unavailable("Session activity changed concurrently"),
            Self::Unavailable(message) => RunError::unavailable(message),
        }
    }
}

impl SessionApplication {
    /// Execute one user message under the Session's durable activity fence.
    /// Settlement is attempted after both successful and failed Runtime work so
    /// protocol adapters and internal jobs cannot leave independent lifecycle
    /// behavior behind.
    pub async fn run_session_message(
        &self,
        agent_id: &str,
        session_id: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        sink: Arc<dyn Sink>,
    ) -> Result<SessionMessageOutcome, RunError> {
        let activity = self
            .begin_activity(session_id)
            .await
            .map_err(SessionActivityError::run_error)?;
        let step = self
            .run_streaming_attributed(agent_id, session_id, content, data_subject_id, sink)
            .await;
        let session = self
            .settle_activity(session_id, activity.activity_epoch)
            .await
            .map_err(SessionActivityError::run_error)?;
        step.map(|step| SessionMessageOutcome { step, session })
    }

    /// Admit one driving event and return the committed aggregate carrying its
    /// monotonically increasing completion fence.
    pub async fn begin_activity(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, SessionActivityError> {
        match self.session_repository().get(session_id).await {
            Ok(session) if session.is_terminal() => return Err(SessionActivityError::Terminal),
            Ok(_) => {}
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                return Err(SessionActivityError::NotFound);
            }
            Err(error) => return Err(SessionActivityError::Unavailable(error.to_string())),
        }
        self.ensure_environment_resident(session_id, now_unix_ms())
            .await
            .map_err(|error| match error {
                super::SessionContinuationError::Terminal => SessionActivityError::Terminal,
                error => SessionActivityError::Unavailable(error.to_string()),
            })?;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionActivityError::mutation)?;
            if session.is_terminal() {
                return Err(SessionActivityError::Terminal);
            }
            if matches!(
                session.execution,
                SessionExecutionState::Preparing | SessionExecutionState::Activating
            ) {
                return Err(SessionActivityError::NotReady);
            }
            if !session.budget.can_admit_model_request() {
                return Err(SessionActivityError::BudgetReached);
            }
            session.activity_epoch = session
                .activity_epoch
                .checked_add(1)
                .ok_or(SessionActivityError::EpochExhausted)?;
            session.environment.mark_active();
            // A driving event is also the trigger that makes a registered Worker
            // claim and realize a freshly prepared Session. Preserve that
            // stronger realization phase until it converges: replacing it with
            // `running` would let a failed Stage look like an ordinary turn and
            // a later settlement could erase `activation_failed` back to idle.
            if session.execution == SessionExecutionState::Idle {
                session
                    .transition_execution(SessionExecutionState::Running)
                    .map_err(|error| SessionActivityError::Unavailable(error.to_string()))?;
            }
            session.begin_runtime_interval(now_unix_ms());
            match self
                .commit_session_snapshot(&owner_scope, session, "begin-activity", Vec::new())
                .await
            {
                Ok(session) => return Ok(session),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Settle a driving event only while its epoch still owns the Session.
    /// A newer activity or any terminal transition fences the completion.
    pub async fn settle_activity(
        &self,
        session_id: &str,
        expected_epoch: u64,
    ) -> Result<PersistedSession, SessionActivityError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionActivityError::mutation)?;
            if session.is_terminal() || session.activity_epoch != expected_epoch {
                return Ok(session);
            }
            // Only the activity transition that wrote `running` owns the inverse
            // transition. Initial realization may still be preparing/activating,
            // or may have failed terminally on another process while this event
            // was queued; settlement must not overwrite either authority.
            if session.execution != SessionExecutionState::Running {
                return Ok(session);
            }
            session
                .transition_execution(SessionExecutionState::Idle)
                .map_err(|error| SessionActivityError::Unavailable(error.to_string()))?;
            let ended_at_unix_ms = now_unix_ms();
            session.environment.mark_idle(ended_at_unix_ms);
            let lifecycle_facts = session
                .close_runtime_interval(ended_at_unix_ms)
                .map(|interval| runtime_interval_fact(&owner_scope, session_id, interval))
                .into_iter()
                .collect::<Vec<_>>();
            let emitted_runtime_interval = !lifecycle_facts.is_empty();
            match self
                .commit_session_snapshot(&owner_scope, session, "settle-activity", lifecycle_facts)
                .await
            {
                Ok(session) => {
                    if emitted_runtime_interval {
                        self.notify_lifecycle_fact();
                    }
                    return Ok(session);
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }
}
