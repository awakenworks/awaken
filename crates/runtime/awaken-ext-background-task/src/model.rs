use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::tool::{
    ToolCall, ToolConcurrency, ToolRecoveryMode, ToolRecoveryPolicy,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BackgroundTaskId(String);

impl BackgroundTaskId {
    pub fn new(value: impl Into<String>) -> Result<Self, BackgroundTaskError> {
        let value = value.into();
        if value.trim().is_empty() {
            Err(BackgroundTaskError::InvalidIdentity)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BackgroundTaskId {
    type Error = BackgroundTaskError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BackgroundTaskId> for String {
    fn from(value: BackgroundTaskId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundTaskOrigin {
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundInvocation {
    pub call: ToolCall,
}

/// Trusted execution facts resolved from the pinned ordinary tool before the
/// first worker acquires it. Requested tasks contain no guessed defaults; the
/// first attempt freezes the policy reused by every reclaim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskExecutionPolicy {
    pub recovery: ToolRecoveryPolicy,
    pub concurrency: ToolConcurrency,
}

/// Complete outcome of a claim decision. Ending an expired, non-replayable
/// task is a committed domain transition, not an error that callers may drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskClaim {
    Acquired(TaskFence),
    EndedIndeterminate,
    EndedAttemptsExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAttempt {
    pub worker_id: String,
    pub epoch: u64,
    pub lease_expires_at_ms: u64,
    pub policy: TaskExecutionPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFence {
    pub worker_id: String,
    pub epoch: u64,
}

impl TaskAttempt {
    #[must_use]
    pub fn fence(&self) -> TaskFence {
        TaskFence {
            worker_id: self.worker_id.clone(),
            epoch: self.epoch,
        }
    }
    #[must_use]
    pub fn owns(&self, fence: &TaskFence) -> bool {
        self.worker_id == fence.worker_id && self.epoch == fence.epoch
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProtocol {
    Mcp,
    A2a,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteContinuation {
    pub protocol: RemoteProtocol,
    pub server_binding: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
}

impl RemoteContinuation {
    pub fn validate(&self) -> Result<(), BackgroundTaskError> {
        if self.server_binding.trim().is_empty()
            || self.task_id.trim().is_empty()
            || self.poll_interval_ms == Some(0)
        {
            Err(BackgroundTaskError::InvalidContinuation)
        } else {
            Ok(())
        }
    }

    fn same_remote_task_as(&self, other: &Self) -> bool {
        self.protocol == other.protocol
            && self.server_binding == other.server_binding
            && self.task_id == other.task_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BackgroundWait {
    Remote(RemoteContinuation),
    ExternalInput,
    Resource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum BackgroundTaskEnd {
    Completed {
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    Failed {
        message: String,
    },
    Cancelled,
    Indeterminate {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BackgroundTaskLifecycle {
    Requested,
    Running {
        attempt: TaskAttempt,
    },
    Waiting {
        attempt: TaskAttempt,
        wait: BackgroundWait,
    },
    Cancelling {
        attempt: TaskAttempt,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait: Option<BackgroundWait>,
    },
    Ended {
        end: BackgroundTaskEnd,
    },
}

impl BackgroundTaskLifecycle {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Ended { .. })
    }
    fn attempt(&self) -> Option<&TaskAttempt> {
        match self {
            Self::Running { attempt }
            | Self::Waiting { attempt, .. }
            | Self::Cancelling { attempt, .. } => Some(attempt),
            Self::Requested | Self::Ended { .. } => None,
        }
    }

    const fn phase(&self) -> TaskPhase {
        match self {
            Self::Requested => TaskPhase::Requested,
            Self::Running { .. } => TaskPhase::Running,
            Self::Waiting { .. } => TaskPhase::Waiting,
            Self::Cancelling { .. } => TaskPhase::Cancelling,
            Self::Ended { .. } => TaskPhase::Ended,
        }
    }

    fn has_remote_continuation(&self) -> bool {
        matches!(
            self,
            Self::Waiting {
                wait: BackgroundWait::Remote(_),
                ..
            } | Self::Cancelling {
                wait: Some(BackgroundWait::Remote(_)),
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskPhase {
    Requested,
    Running,
    Waiting,
    Cancelling,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelDecision {
    EndCancelled,
    MarkCancelling,
    Stutter,
}

const fn cancel_decision(phase: TaskPhase) -> CancelDecision {
    match phase {
        TaskPhase::Requested => CancelDecision::EndCancelled,
        TaskPhase::Running | TaskPhase::Waiting => CancelDecision::MarkCancelling,
        TaskPhase::Cancelling | TaskPhase::Ended => CancelDecision::Stutter,
    }
}

const fn cancellation_wins_completion(phase: TaskPhase) -> bool {
    matches!(phase, TaskPhase::Cancelling)
}

const fn expired_attempt_is_reconnectable(
    mode: ToolRecoveryMode,
    has_remote_continuation: bool,
) -> bool {
    !matches!(mode, ToolRecoveryMode::NeverReplay) || has_remote_continuation
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "BackgroundTaskWire")]
pub struct BackgroundTask {
    pub id: BackgroundTaskId,
    pub origin: BackgroundTaskOrigin,
    pub invocation: BackgroundInvocation,
    pub lifecycle: BackgroundTaskLifecycle,
    pub revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundTaskWire {
    id: BackgroundTaskId,
    origin: BackgroundTaskOrigin,
    invocation: BackgroundInvocation,
    lifecycle: BackgroundTaskLifecycle,
    revision: u64,
}

impl TryFrom<BackgroundTaskWire> for BackgroundTask {
    type Error = BackgroundTaskError;

    fn try_from(wire: BackgroundTaskWire) -> Result<Self, Self::Error> {
        let task = Self {
            id: wire.id,
            origin: wire.origin,
            invocation: wire.invocation,
            lifecycle: wire.lifecycle,
            revision: wire.revision,
        };
        task.validate()?;
        Ok(task)
    }
}

impl BackgroundTask {
    #[must_use]
    pub fn requested(
        id: BackgroundTaskId,
        origin: BackgroundTaskOrigin,
        invocation: BackgroundInvocation,
    ) -> Self {
        Self {
            id,
            origin,
            invocation,
            lifecycle: BackgroundTaskLifecycle::Requested,
            revision: 0,
        }
    }

    /// Current fenced attempt, when the task has entered execution.
    #[must_use]
    pub fn attempt(&self) -> Option<&TaskAttempt> {
        self.lifecycle.attempt()
    }

    /// Stable request identity, excluding worker-owned lifecycle facts.
    #[must_use]
    pub fn same_request_as(&self, other: &Self) -> bool {
        self.id == other.id && self.origin == other.origin && self.invocation == other.invocation
    }

    pub fn start(
        &mut self,
        worker_id: impl Into<String>,
        now_ms: u64,
        lease_ms: u64,
        policy: TaskExecutionPolicy,
    ) -> Result<TaskFence, BackgroundTaskError> {
        let worker_id = worker_id.into();
        if worker_id.trim().is_empty() || lease_ms == 0 {
            return Err(BackgroundTaskError::InvalidClaim);
        }
        let expires = now_ms
            .checked_add(lease_ms)
            .ok_or(BackgroundTaskError::ClockOverflow)?;
        match self.lifecycle {
            BackgroundTaskLifecycle::Requested => {}
            BackgroundTaskLifecycle::Ended { .. } => return Err(BackgroundTaskError::Terminal),
            _ => return Err(BackgroundTaskError::Busy),
        }
        let attempt = TaskAttempt {
            worker_id,
            epoch: 1,
            lease_expires_at_ms: expires,
            policy,
        };
        let fence = attempt.fence();
        self.replace_lifecycle(BackgroundTaskLifecycle::Running { attempt })?;
        Ok(fence)
    }

    pub fn reclaim(
        &mut self,
        worker_id: impl Into<String>,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<TaskClaim, BackgroundTaskError> {
        let worker_id = worker_id.into();
        if worker_id.trim().is_empty() || lease_ms == 0 {
            return Err(BackgroundTaskError::InvalidClaim);
        }
        let expires = now_ms
            .checked_add(lease_ms)
            .ok_or(BackgroundTaskError::ClockOverflow)?;
        let remote_reconnectable = self.lifecycle.has_remote_continuation();
        let (epoch, policy) = match &self.lifecycle {
            BackgroundTaskLifecycle::Running { attempt }
            | BackgroundTaskLifecycle::Waiting { attempt, .. }
            | BackgroundTaskLifecycle::Cancelling { attempt, .. }
                if attempt.lease_expires_at_ms <= now_ms =>
            {
                if !expired_attempt_is_reconnectable(
                    attempt.policy.recovery.mode(),
                    remote_reconnectable,
                ) {
                    self.end(BackgroundTaskEnd::Indeterminate {
                        message: "worker lease expired after a non-replayable effect".into(),
                    })?;
                    return Ok(TaskClaim::EndedIndeterminate);
                }
                if attempt.epoch >= u64::from(attempt.policy.recovery.max_attempts().get()) {
                    if remote_reconnectable {
                        self.end(BackgroundTaskEnd::Indeterminate {
                            message: "remote background task observation budget exhausted; remote outcome is unknown".into(),
                        })?;
                        return Ok(TaskClaim::EndedIndeterminate);
                    }
                    self.end(BackgroundTaskEnd::Failed {
                        message: "background task recovery attempt budget exhausted".into(),
                    })?;
                    return Ok(TaskClaim::EndedAttemptsExhausted);
                }
                (
                    attempt
                        .epoch
                        .checked_add(1)
                        .ok_or(BackgroundTaskError::EpochOverflow)?,
                    attempt.policy.clone(),
                )
            }
            BackgroundTaskLifecycle::Ended { .. } => return Err(BackgroundTaskError::Terminal),
            BackgroundTaskLifecycle::Requested => {
                return Err(BackgroundTaskError::InvalidTransition);
            }
            _ => return Err(BackgroundTaskError::Busy),
        };
        let attempt = TaskAttempt {
            worker_id,
            epoch,
            lease_expires_at_ms: expires,
            policy,
        };
        let fence = attempt.fence();
        let lifecycle = match &self.lifecycle {
            BackgroundTaskLifecycle::Running { .. } => BackgroundTaskLifecycle::Running { attempt },
            BackgroundTaskLifecycle::Waiting { wait, .. } => BackgroundTaskLifecycle::Waiting {
                attempt,
                wait: wait.clone(),
            },
            BackgroundTaskLifecycle::Cancelling { wait, .. } => {
                BackgroundTaskLifecycle::Cancelling {
                    attempt,
                    wait: wait.clone(),
                }
            }
            BackgroundTaskLifecycle::Requested | BackgroundTaskLifecycle::Ended { .. } => {
                return Err(BackgroundTaskError::InvalidTransition);
            }
        };
        self.replace_lifecycle(lifecycle)?;
        Ok(TaskClaim::Acquired(fence))
    }

    pub fn heartbeat(
        &mut self,
        fence: &TaskFence,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<(), BackgroundTaskError> {
        if lease_ms == 0 {
            return Err(BackgroundTaskError::InvalidClaim);
        }
        let expires = now_ms
            .checked_add(lease_ms)
            .ok_or(BackgroundTaskError::ClockOverflow)?;
        let lifecycle = match &self.lifecycle {
            BackgroundTaskLifecycle::Running { attempt } if attempt.owns(fence) => {
                BackgroundTaskLifecycle::Running {
                    attempt: extended_attempt(attempt, expires)?,
                }
            }
            BackgroundTaskLifecycle::Waiting { attempt, wait } if attempt.owns(fence) => {
                BackgroundTaskLifecycle::Waiting {
                    attempt: extended_attempt(attempt, expires)?,
                    wait: wait.clone(),
                }
            }
            BackgroundTaskLifecycle::Cancelling { attempt, wait } if attempt.owns(fence) => {
                BackgroundTaskLifecycle::Cancelling {
                    attempt: extended_attempt(attempt, expires)?,
                    wait: wait.clone(),
                }
            }
            _ => return Err(BackgroundTaskError::StaleFence),
        };
        self.replace_lifecycle(lifecycle)
    }

    pub fn wait(
        &mut self,
        fence: &TaskFence,
        wait: BackgroundWait,
    ) -> Result<(), BackgroundTaskError> {
        if let BackgroundWait::Remote(continuation) = &wait {
            continuation.validate()?;
        }
        let lifecycle = match &self.lifecycle {
            BackgroundTaskLifecycle::Running { attempt } if attempt.owns(fence) => {
                BackgroundTaskLifecycle::Waiting {
                    attempt: attempt.clone(),
                    wait,
                }
            }
            BackgroundTaskLifecycle::Waiting {
                attempt,
                wait: current,
            } if attempt.owns(fence) && same_wait_target(current, &wait) => {
                BackgroundTaskLifecycle::Waiting {
                    attempt: attempt.clone(),
                    wait,
                }
            }
            BackgroundTaskLifecycle::Cancelling {
                attempt,
                wait: current,
            } if attempt.owns(fence) && cancelling_accepts_wait(current.as_ref(), &wait) => {
                BackgroundTaskLifecycle::Cancelling {
                    attempt: attempt.clone(),
                    wait: Some(wait),
                }
            }
            lifecycle if lifecycle.attempt().is_some() => {
                if lifecycle
                    .attempt()
                    .is_some_and(|attempt| !attempt.owns(fence))
                {
                    return Err(BackgroundTaskError::StaleFence);
                }
                return Err(BackgroundTaskError::InvalidTransition);
            }
            _ => return Err(BackgroundTaskError::StaleFence),
        };
        self.replace_lifecycle(lifecycle)
    }

    pub fn request_cancel(&mut self) -> Result<(), BackgroundTaskError> {
        let lifecycle = match cancel_decision(self.lifecycle.phase()) {
            CancelDecision::EndCancelled => Some(BackgroundTaskLifecycle::Ended {
                end: BackgroundTaskEnd::Cancelled,
            }),
            CancelDecision::MarkCancelling => {
                let (attempt, wait) = match &self.lifecycle {
                    BackgroundTaskLifecycle::Running { attempt } => (attempt, None),
                    BackgroundTaskLifecycle::Waiting { attempt, wait } => {
                        (attempt, Some(wait.clone()))
                    }
                    _ => return Err(BackgroundTaskError::InvalidPersistedState),
                };
                Some(BackgroundTaskLifecycle::Cancelling {
                    attempt: attempt.clone(),
                    wait,
                })
            }
            CancelDecision::Stutter => None,
        };
        match lifecycle {
            Some(lifecycle) => self.replace_lifecycle(lifecycle),
            None => Ok(()),
        }
    }

    pub fn finish(
        &mut self,
        fence: &TaskFence,
        end: BackgroundTaskEnd,
    ) -> Result<(), BackgroundTaskError> {
        self.lifecycle
            .attempt()
            .filter(|attempt| attempt.owns(fence))
            .ok_or(BackgroundTaskError::StaleFence)?;
        let end = if cancellation_wins_completion(self.lifecycle.phase()) {
            BackgroundTaskEnd::Cancelled
        } else {
            end
        };
        self.end(end)
    }

    fn end(&mut self, end: BackgroundTaskEnd) -> Result<(), BackgroundTaskError> {
        self.replace_lifecycle(BackgroundTaskLifecycle::Ended { end })
    }

    fn replace_lifecycle(
        &mut self,
        lifecycle: BackgroundTaskLifecycle,
    ) -> Result<(), BackgroundTaskError> {
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(BackgroundTaskError::RevisionOverflow)?;
        self.lifecycle = lifecycle;
        self.revision = revision;
        Ok(())
    }

    fn validate(&self) -> Result<(), BackgroundTaskError> {
        if self.origin.thread_id.0.trim().is_empty()
            || self.origin.run_id.0.trim().is_empty()
            || self.origin.operation_id.trim().is_empty()
            || self.invocation.call.call_id.trim().is_empty()
            || self.invocation.call.tool_id.trim().is_empty()
        {
            return Err(BackgroundTaskError::InvalidPersistedState);
        }
        match &self.lifecycle {
            BackgroundTaskLifecycle::Requested if self.revision == 0 => Ok(()),
            BackgroundTaskLifecycle::Ended { .. } if self.revision > 0 => Ok(()),
            BackgroundTaskLifecycle::Running { attempt } if self.revision > 0 => {
                validate_attempt(attempt)
            }
            BackgroundTaskLifecycle::Waiting { attempt, wait } if self.revision > 0 => {
                validate_attempt(attempt)?;
                validate_wait(wait)?;
                Ok(())
            }
            BackgroundTaskLifecycle::Cancelling { attempt, wait } if self.revision > 0 => {
                validate_attempt(attempt)?;
                if let Some(wait) = wait {
                    validate_wait(wait)?;
                }
                Ok(())
            }
            _ => Err(BackgroundTaskError::InvalidPersistedState),
        }
    }
}

fn validate_wait(wait: &BackgroundWait) -> Result<(), BackgroundTaskError> {
    if let BackgroundWait::Remote(continuation) = wait {
        continuation.validate()?;
    }
    Ok(())
}

fn same_wait_target(current: &BackgroundWait, next: &BackgroundWait) -> bool {
    match (current, next) {
        (BackgroundWait::Remote(current), BackgroundWait::Remote(next)) => {
            current.same_remote_task_as(next)
        }
        (BackgroundWait::ExternalInput, BackgroundWait::ExternalInput)
        | (BackgroundWait::Resource, BackgroundWait::Resource) => true,
        _ => false,
    }
}

fn cancelling_accepts_wait(current: Option<&BackgroundWait>, next: &BackgroundWait) -> bool {
    matches!(next, BackgroundWait::Remote(_))
        && current.is_none_or(|current| same_wait_target(current, next))
}

fn extended_attempt(
    attempt: &TaskAttempt,
    expires: u64,
) -> Result<TaskAttempt, BackgroundTaskError> {
    if expires <= attempt.lease_expires_at_ms {
        return Err(BackgroundTaskError::NonMonotonicLease);
    }
    let mut next = attempt.clone();
    next.lease_expires_at_ms = expires;
    Ok(next)
}

fn validate_attempt(attempt: &TaskAttempt) -> Result<(), BackgroundTaskError> {
    if attempt.worker_id.trim().is_empty() || attempt.epoch == 0 || attempt.lease_expires_at_ms == 0
    {
        Err(BackgroundTaskError::InvalidPersistedState)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BackgroundTaskError {
    #[error("background task id must not be empty")]
    InvalidIdentity,
    #[error("background task claim requires a worker and a positive lease")]
    InvalidClaim,
    #[error("background task is already owned")]
    Busy,
    #[error("background task is terminal")]
    Terminal,
    #[error("background task transition is invalid")]
    InvalidTransition,
    #[error("background task fence is stale")]
    StaleFence,
    #[error("background task lease must advance monotonically")]
    NonMonotonicLease,
    #[error(
        "remote continuation requires a server binding, task id, and a positive poll interval when present"
    )]
    InvalidContinuation,
    #[error("background task clock overflow")]
    ClockOverflow,
    #[error("background task epoch overflow")]
    EpochOverflow,
    #[error("background task revision overflow")]
    RevisionOverflow,
    #[error("background task persisted state violates its aggregate invariants")]
    InvalidPersistedState,
}

#[cfg(kani)]
#[kani::proof]
fn cancellation_is_monotone_and_terminal_states_are_absorbing() {
    let raw: u8 = kani::any();
    kani::assume(raw < 5);
    let phase = match raw {
        0 => TaskPhase::Requested,
        1 => TaskPhase::Running,
        2 => TaskPhase::Waiting,
        3 => TaskPhase::Cancelling,
        _ => TaskPhase::Ended,
    };
    let decision = cancel_decision(phase);
    assert_eq!(
        decision == CancelDecision::EndCancelled,
        phase == TaskPhase::Requested
    );
    assert_eq!(
        decision == CancelDecision::MarkCancelling,
        matches!(phase, TaskPhase::Running | TaskPhase::Waiting)
    );
    assert_eq!(
        decision == CancelDecision::Stutter,
        matches!(phase, TaskPhase::Cancelling | TaskPhase::Ended)
    );
    assert_eq!(
        cancellation_wins_completion(phase),
        phase == TaskPhase::Cancelling
    );
}

#[cfg(kani)]
#[kani::proof]
fn never_replay_recovery_requires_a_durable_remote_continuation() {
    let raw: u8 = kani::any();
    kani::assume(raw < 4);
    let mode = match raw {
        0 => ToolRecoveryMode::NeverReplay,
        1 => ToolRecoveryMode::ReplaySafe,
        2 => ToolRecoveryMode::Idempotent,
        _ => ToolRecoveryMode::DurableRequest,
    };
    let has_remote_continuation: bool = kani::any();
    assert_eq!(
        expired_attempt_is_reconnectable(mode, has_remote_continuation),
        mode != ToolRecoveryMode::NeverReplay || has_remote_continuation
    );
    if mode == ToolRecoveryMode::NeverReplay {
        assert_eq!(
            expired_attempt_is_reconnectable(mode, has_remote_continuation),
            has_remote_continuation
        );
    }
}
