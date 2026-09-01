//! Durable activity fencing for overlapping Session Runs.

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEventKind};
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, RunError, SessionExecutionState,
    SessionRuntimeInterval, SessionRuntimeIntervalObservation,
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

    pub(crate) fn run_error(self) -> RunError {
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
    fn activity_operation_record(session_id: &str, operation_id: &str) -> IdempotencyRecord {
        let identity = awaken_session_contract::stable_fingerprint(&(
            "managed-session-activity-v1",
            session_id,
            operation_id,
        ));
        IdempotencyRecord {
            key: format!("managed:session-activity:{session_id}:{identity}"),
            payload_hash: identity,
        }
    }

    async fn activity_operation_receipt(
        &self,
        session_id: &str,
        record: &IdempotencyRecord,
    ) -> Result<Option<u64>, SessionActivityError> {
        let receipt = self
            .session_repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_failure)
            .map_err(SessionActivityError::mutation)?;
        match receipt {
            Some(receipt) if receipt.payload_hash != record.payload_hash => {
                Err(SessionActivityError::Unavailable(
                    "Session activity operation id was reused with another payload".into(),
                ))
            }
            Some(receipt) => Ok(Some(receipt.committed_revision.0)),
            None => Ok(None),
        }
    }

    async fn replayed_activity_operation(
        &self,
        session_id: &str,
        record: &IdempotencyRecord,
    ) -> Result<Option<(PersistedSession, u64)>, SessionActivityError> {
        let Some(epoch) = self.activity_operation_receipt(session_id, record).await? else {
            return Ok(None);
        };
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionActivityError::mutation)?;
        Ok(Some((session, epoch)))
    }

    pub(crate) fn open_activity_on_snapshot(
        session: &mut PersistedSession,
        exact_epoch: Option<u64>,
        allow_external_realization: bool,
        continue_committed_await: bool,
    ) -> Result<u64, SessionActivityError> {
        if session.is_terminal() {
            return Err(SessionActivityError::Terminal);
        }
        if matches!(
            session.execution,
            SessionExecutionState::Preparing | SessionExecutionState::Activating
        ) && !allow_external_realization
        {
            return Err(SessionActivityError::NotReady);
        }
        if !continue_committed_await && !session.budget.can_admit_model_request() {
            return Err(SessionActivityError::BudgetReached);
        }
        let epoch = match exact_epoch {
            Some(epoch) if session.begin_activity_epoch_at(epoch) => epoch,
            Some(_) => return Err(SessionActivityError::Conflict),
            None => session
                .begin_activity_epoch()
                .ok_or(SessionActivityError::EpochExhausted)?,
        };
        session.environment.mark_active();
        if session.execution == SessionExecutionState::Idle {
            session
                .transition_execution(SessionExecutionState::Running)
                .map_err(|error| SessionActivityError::Unavailable(error.to_string()))?;
        }
        session.begin_runtime_interval(now_unix_ms());
        Ok(epoch)
    }

    /// Recover/admit the frozen Session binding, then open exactly one activity
    /// for a stable protocol operation. The existing operation receipt is the
    /// only retry coordinate; the protocol receives the committed epoch and
    /// must pass it through instead of performing another admission check.
    pub async fn begin_admitted_activity_for_operation(
        &self,
        agent_id: &str,
        session_id: &str,
        operation_id: &str,
    ) -> Result<(PersistedSession, u64), RunError> {
        if operation_id.trim().is_empty() {
            return Err(RunError::internal(
                "Session activity operation identity is required",
            ));
        }
        let record = Self::activity_operation_record(session_id, operation_id);
        // Exact committed receipts are recovery truth. Re-read them before
        // fresh policy so a response-loss repair can recover the original epoch
        // even if the Session subsequently reached a budget or terminal state.
        if let Some(committed) = self
            .replayed_activity_operation(session_id, &record)
            .await
            .map_err(SessionActivityError::run_error)?
        {
            return Ok(committed);
        }
        let owner_scope = self
            .owner(session_id)
            .await
            .map_err(SessionActivityError::mutation)
            .map_err(SessionActivityError::run_error)?;
        self.admit_run_session(&owner_scope, session_id, agent_id)
            .await?;
        self.begin_activity_for_operation(session_id, operation_id)
            .await
            .map_err(SessionActivityError::run_error)
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
        let admission = self.fresh_mutation_record(session_id, "begin-activity");
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
            // A driving event is also the trigger that makes a registered Worker
            // claim and realize a freshly prepared Session. Preserve that
            // stronger realization phase until it converges: replacing it with
            // `running` would let a failed Stage look like an ordinary Run and
            // a later settlement could erase `activation_failed` back to idle.
            let external = self.requires_external_realization(&session);
            Self::open_activity_on_snapshot(&mut session, None, external, false)?;
            match self
                .commit_session_snapshot_with_record(
                    &owner_scope,
                    session,
                    admission.clone(),
                    Vec::new(),
                )
                .await
            {
                Ok((session, _)) => return Ok(session),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Open the durable activity for one stable coordination operation. The
    /// existing Session mutation receipt is the sole operation→epoch mapping:
    /// its committed root revision is the epoch, so exact retries (including a
    /// retry after settlement) return the same value without reopening activity.
    pub async fn begin_activity_for_operation(
        &self,
        session_id: &str,
        operation_id: &str,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        self.begin_activity_for_operation_with_policy(session_id, operation_id, false, None)
            .await
    }

    /// Read only the exact committed activity receipt for crash recovery. This
    /// never evaluates fresh admission and never opens an epoch; cancellation
    /// uses it to distinguish a never-admitted reservation from one whose
    /// already-committed activity still requires canonical settlement.
    pub(crate) async fn recover_activity_for_operation(
        &self,
        session_id: &str,
        operation_id: &str,
    ) -> Result<Option<(PersistedSession, u64)>, SessionActivityError> {
        if operation_id.trim().is_empty() {
            return Err(SessionActivityError::Unavailable(
                "Session activity operation identity is required".into(),
            ));
        }
        let record = Self::activity_operation_record(session_id, operation_id);
        self.replayed_activity_operation(session_id, &record).await
    }

    /// Atomically replace an already-coordinated dispatch activity with the
    /// activity for its exact committed continuation. The old Worker may settle
    /// before or after this CAS: removing an already-settled epoch is a no-op,
    /// while a later stale settlement cannot close the newly opened interval.
    pub(crate) async fn transfer_committed_activity_for_operation(
        &self,
        session_id: &str,
        operation_id: &str,
        prior_activity_epoch: Option<u64>,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        if prior_activity_epoch == Some(0) {
            return Err(SessionActivityError::Unavailable(
                "prior Session activity epoch must be nonzero".into(),
            ));
        }
        self.begin_activity_for_operation_with_policy(
            session_id,
            operation_id,
            true,
            prior_activity_epoch,
        )
        .await
    }

    async fn begin_activity_for_operation_with_policy(
        &self,
        session_id: &str,
        operation_id: &str,
        continue_committed_await: bool,
        prior_activity_epoch: Option<u64>,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        if operation_id.trim().is_empty() {
            return Err(SessionActivityError::Unavailable(
                "Session activity operation identity is required".into(),
            ));
        }
        let record = Self::activity_operation_record(session_id, operation_id);
        if let Some(committed) = self
            .replayed_activity_operation(session_id, &record)
            .await?
        {
            return Ok(committed);
        }

        self.ensure_environment_resident(session_id, now_unix_ms())
            .await
            .map_err(|error| match error {
                super::SessionContinuationError::Terminal => SessionActivityError::Terminal,
                error => SessionActivityError::Unavailable(error.to_string()),
            })?;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if let Some(committed) = self
                .replayed_activity_operation(session_id, &record)
                .await?
            {
                return Ok(committed);
            }
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
            // Close the receipt/read race: if a concurrent exact caller
            // committed after our pre-loop check but before this snapshot, its
            // durable receipt wins before we compile another epoch.
            if let Some(committed) = self
                .replayed_activity_operation(session_id, &record)
                .await?
            {
                return Ok(committed);
            }
            let epoch = session
                .revision
                .0
                .checked_add(1)
                .ok_or(SessionActivityError::EpochExhausted)?;
            let external = self.requires_external_realization(&session);
            Self::open_activity_on_snapshot(
                &mut session,
                Some(epoch),
                external,
                continue_committed_await,
            )?;
            if let Some(prior_activity_epoch) = prior_activity_epoch {
                // The new epoch was inserted first, so removing the old one can
                // never make the aggregate falsely Idle inside this mutation.
                let _ = session.settle_activity_epoch(prior_activity_epoch);
            }
            match self
                .commit_session_snapshot_with_record(
                    &owner_scope,
                    session,
                    record.clone(),
                    Vec::new(),
                )
                .await
            {
                Ok((session, true)) => {
                    if session.revision.0 != epoch {
                        return Err(SessionActivityError::Unavailable(
                            "Session activity revision did not match its committed epoch".into(),
                        ));
                    }
                    return Ok((session, epoch));
                }
                Ok((session, false)) => {
                    let epoch = self
                        .activity_operation_receipt(session_id, &record)
                        .await?
                        .ok_or_else(|| {
                            SessionActivityError::Unavailable(
                                "replayed Session activity has no idempotency receipt".into(),
                            )
                        })?;
                    return Ok((session, epoch));
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Settle one admitted driving event. Completion order is irrelevant: every
    /// active epoch is removed exactly once, and only the last removal closes
    /// the shared Running interval. Terminal truth fences every completion.
    pub async fn settle_activity(
        &self,
        session_id: &str,
        expected_epoch: u64,
    ) -> Result<PersistedSession, SessionActivityError> {
        self.settle_activity_observed(session_id, expected_epoch, None)
            .await
    }

    /// Settle one admitted activity while retaining the exact Runtime commit
    /// boundary that caused it. Infrastructure-only and definitively rejected
    /// activities carry no observation; they still close through this same root
    /// CAS and never create a protocol-side lifecycle owner.
    pub async fn settle_activity_observed(
        &self,
        session_id: &str,
        expected_epoch: u64,
        observation: Option<SessionRuntimeIntervalObservation>,
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
            if session.is_terminal() {
                return Ok(session);
            }
            if let Some(observation) = observation.clone() {
                if session.closed_runtime_intervals.iter().any(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                }) {
                    return Ok(session);
                }
                // A reply may transfer the activity while its predecessor's
                // leased Worker is still closing. Fence that stale observation
                // before comparing it with the predecessor's already-closed
                // boundary: it belongs to neither the closed interval nor the
                // successor and is therefore an exact no-op.
                if !(session.active_activity_epochs.contains(&expected_epoch)
                    || (session.active_activity_epochs.is_empty()
                        && session.execution == SessionExecutionState::Running
                        && session.activity_epoch == expected_epoch))
                {
                    return Ok(session);
                }
                if session.closed_runtime_intervals.iter().any(|interval| {
                    interval.observations.iter().any(|existing| {
                        existing.activity_epoch == expected_epoch && existing != &observation
                    })
                }) {
                    return Err(SessionActivityError::Unavailable(
                        "Session activity epoch was reused with another Runtime boundary".into(),
                    ));
                }
                let exact_replay = session.running_interval.as_ref().is_some_and(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                });
                // An externally realized Session may admit and activate a Run
                // while its stronger Preparing/Activating phase is still
                // authoritative. If cancellation commits before realization,
                // no customer-visible Running interval ever opened; settle the
                // admitted epoch without fabricating one or attaching the Run
                // boundary to a nonexistent interval.
                let settles_before_runtime_interval = session.running_interval.is_none()
                    && matches!(
                        session.execution,
                        SessionExecutionState::Preparing | SessionExecutionState::Activating
                    );
                if !exact_replay
                    && !settles_before_runtime_interval
                    && !session.observe_runtime_interval(observation)
                {
                    return Err(SessionActivityError::Unavailable(
                        "Runtime boundary does not belong to the active Session interval".into(),
                    ));
                }
            }
            let Some(last_active) = session.settle_activity_epoch(expected_epoch) else {
                return Ok(session);
            };
            // Completion order does not grant inverse-transition ownership.
            // Every admitted epoch is removed exactly once; only the removal of
            // the final active epoch may close the one shared Running interval.
            // Initial Worker realization may still be preparing/activating, in
            // which case settlement records the activity completion without
            // overwriting that stronger execution phase.
            let lifecycle_facts =
                if last_active && session.execution == SessionExecutionState::Running {
                    session
                        .transition_execution(SessionExecutionState::Idle)
                        .map_err(|error| SessionActivityError::Unavailable(error.to_string()))?;
                    let ended_at_unix_ms = now_unix_ms();
                    session.environment.mark_idle(ended_at_unix_ms);
                    session
                        .close_runtime_interval(ended_at_unix_ms)
                        .map(|interval| runtime_interval_fact(&owner_scope, session_id, interval))
                        .into_iter()
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
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

    /// Retain a child or intermediate Runtime boundary without settling the
    /// shared activity epoch. A child-to-primary report continuation uses this
    /// path so the final interval carries both exact commit coordinates.
    pub(crate) async fn observe_activity_runtime_boundary(
        &self,
        session_id: &str,
        observation: SessionRuntimeIntervalObservation,
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
            if session.is_terminal()
                || session.closed_runtime_intervals.iter().any(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                })
                || session.running_interval.as_ref().is_some_and(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                })
            {
                return Ok(session);
            }
            if session.closed_runtime_intervals.iter().any(|interval| {
                interval.observations.iter().any(|existing| {
                    existing.activity_epoch == observation.activity_epoch
                        && existing != &observation
                })
            }) {
                return Err(SessionActivityError::Unavailable(
                    "Session activity epoch was reused with another Runtime boundary".into(),
                ));
            }
            if !session.observe_runtime_interval(observation.clone()) {
                return Err(SessionActivityError::Unavailable(
                    "Runtime boundary does not belong to the active Session interval".into(),
                ));
            }
            match self
                .commit_session_snapshot(
                    &owner_scope,
                    session,
                    "observe-runtime-boundary",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => return Ok(session),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Resolve one exact terminal Runtime lifecycle coordinate from the existing
    /// committed feed. `source_commit_fence` is the recovery snapshot boundary
    /// used for child settlement; direct steps select the latest matching commit
    /// observed after their synchronous Runtime result.
    pub(crate) async fn runtime_interval_observation(
        &self,
        session_id: &str,
        activity_epoch: u64,
        thread_id: &ThreadId,
        run_id: &RunId,
        state: &RunState,
        source_commit_fence: Option<u64>,
    ) -> Result<Option<SessionRuntimeIntervalObservation>, RunError> {
        const PAGE_SIZE: usize = 256;
        let mut cursor = RunLifecycleCursor::default();
        let mut selected = None;
        loop {
            let page = self
                .committed_run_lifecycle(session_id, cursor, PAGE_SIZE)
                .await?;
            let count = page.events.len();
            for event in page.events {
                if &event.thread_id == thread_id
                    && &event.run_id == run_id
                    && &event.state == state
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Awaiting
                            | RunLifecycleEventKind::Completed
                            | RunLifecycleEventKind::Failed
                            | RunLifecycleEventKind::Cancelled
                    )
                    && source_commit_fence.is_none_or(|fence| event.source_commit_cursor <= fence)
                {
                    selected = Some(SessionRuntimeIntervalObservation {
                        activity_epoch,
                        thread_id: event.thread_id,
                        run_id: event.run_id,
                        lifecycle_cursor: event.cursor,
                        source_commit_cursor: event.source_commit_cursor,
                    });
                }
            }
            if page.next_cursor == cursor || count < PAGE_SIZE {
                break;
            }
            cursor = page.next_cursor;
        }
        Ok(selected)
    }
}
