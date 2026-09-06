use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::tool::{
    ToolCall, ToolConcurrency, ToolRecoveryMode, ToolRecoveryPolicy, ToolTaskHandle,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BackgroundWait {
    Remote(ToolTaskHandle),
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

    fn remote_wait(&self) -> Option<&BackgroundWait> {
        match self {
            Self::Waiting {
                wait: wait @ BackgroundWait::Remote(_),
                ..
            }
            | Self::Cancelling {
                wait: Some(wait @ BackgroundWait::Remote(_)),
                ..
            } => Some(wait),
            _ => None,
        }
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

const fn cancellation_wins_completion(phase: TaskPhase, indeterminate: bool) -> bool {
    // Intent dominates a known racing result, but cannot certify that an
    // unobserved remote effect stopped. Preserve the uncertainty for recovery.
    matches!(phase, TaskPhase::Cancelling) && !indeterminate
}

const fn execution_is_admitted(
    current: TaskPhase,
    expected: TaskPhase,
    identity_matches: bool,
    lease_expires_at_ms: u64,
    now_ms: u64,
    exact_remote_target: bool,
) -> bool {
    if !identity_matches || now_ms >= lease_expires_at_ms {
        return false;
    }
    match (expected, current) {
        (TaskPhase::Running, TaskPhase::Running) => true,
        (TaskPhase::Waiting, TaskPhase::Waiting | TaskPhase::Cancelling)
        | (TaskPhase::Cancelling, TaskPhase::Cancelling) => exact_remote_target,
        _ => false,
    }
}

const fn expired_attempt_is_reconnectable(
    mode: ToolRecoveryMode,
    has_remote_continuation: bool,
    cancelling: bool,
) -> bool {
    has_remote_continuation || (!cancelling && !matches!(mode, ToolRecoveryMode::NeverReplay))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiredRecoveryDecision {
    Acquire(u64),
    Indeterminate,
    AttemptsExhausted,
}

/// Sole decision for an already-expired attempt. Timing admission and atomic
/// revision replacement remain at the aggregate boundary; this kernel cannot
/// manufacture a remote termination receipt or wrap an attempt epoch.
const fn expired_recovery_decision(
    mode: ToolRecoveryMode,
    remote: bool,
    cancelling: bool,
    epoch: u64,
    maximum: u16,
) -> ExpiredRecoveryDecision {
    if !expired_attempt_is_reconnectable(mode, remote, cancelling) {
        ExpiredRecoveryDecision::Indeterminate
    } else if epoch >= maximum as u64 {
        if remote {
            ExpiredRecoveryDecision::Indeterminate
        } else {
            ExpiredRecoveryDecision::AttemptsExhausted
        }
    } else {
        // epoch < a u16 budget makes the successor representable in u64.
        ExpiredRecoveryDecision::Acquire(epoch + 1)
    }
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

    /// Admit an external execution against freshly read committed truth. The
    /// expected snapshot freezes request, policy, fence and (for continuation)
    /// remote identity. A heartbeat may renew the current lease and cancellation
    /// may narrow Waiting to Cancelling, but neither authorizes another start.
    /// This is a read decision, not a permit that fences a third-party effect.
    #[must_use]
    pub fn admits_execution_of(&self, expected: &Self, now_ms: u64) -> bool {
        let (Some(current), Some(claim)) = (self.attempt(), expected.attempt()) else {
            return false;
        };
        let exact_remote_target = self
            .lifecycle
            .remote_wait()
            .zip(expected.lifecycle.remote_wait())
            .is_some_and(|(current, expected)| same_wait_target(current, expected));
        execution_is_admitted(
            self.lifecycle.phase(),
            expected.lifecycle.phase(),
            self.same_request_as(expected)
                && current.owns(&claim.fence())
                && current.policy == claim.policy,
            current.lease_expires_at_ms,
            now_ms,
            exact_remote_target,
        )
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
        let remote_reconnectable = self.lifecycle.remote_wait().is_some();
        let cancelling = matches!(self.lifecycle, BackgroundTaskLifecycle::Cancelling { .. });
        let (epoch, policy) = match &self.lifecycle {
            BackgroundTaskLifecycle::Running { attempt }
            | BackgroundTaskLifecycle::Waiting { attempt, .. }
            | BackgroundTaskLifecycle::Cancelling { attempt, .. }
                if attempt.lease_expires_at_ms <= now_ms =>
            {
                match expired_recovery_decision(
                    attempt.policy.recovery.mode(),
                    remote_reconnectable,
                    cancelling,
                    attempt.epoch,
                    attempt.policy.recovery.max_attempts().get(),
                ) {
                    ExpiredRecoveryDecision::Indeterminate => {
                        let message = if remote_reconnectable {
                            "remote background task observation budget exhausted; remote outcome is unknown"
                        } else {
                            "worker lease expired without a safe recovery or cancellation observation"
                        };
                        self.end(BackgroundTaskEnd::Indeterminate {
                            message: message.into(),
                        })?;
                        return Ok(TaskClaim::EndedIndeterminate);
                    }
                    ExpiredRecoveryDecision::AttemptsExhausted => {
                        self.end(BackgroundTaskEnd::Failed {
                            message: "background task recovery attempt budget exhausted".into(),
                        })?;
                        return Ok(TaskClaim::EndedAttemptsExhausted);
                    }
                    ExpiredRecoveryDecision::Acquire(epoch) => (epoch, attempt.policy.clone()),
                }
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
        if let BackgroundWait::Remote(handle) = &wait {
            validate_task_handle(handle)?;
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
        let end = if cancellation_wins_completion(
            self.lifecycle.phase(),
            matches!(end, BackgroundTaskEnd::Indeterminate { .. }),
        ) {
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
    if let BackgroundWait::Remote(handle) = wait {
        validate_task_handle(handle)?;
    }
    Ok(())
}

fn validate_task_handle(handle: &ToolTaskHandle) -> Result<(), BackgroundTaskError> {
    handle
        .validate()
        .map_err(|_| BackgroundTaskError::InvalidContinuation)
}

fn same_wait_target(current: &BackgroundWait, next: &BackgroundWait) -> bool {
    match (current, next) {
        (BackgroundWait::Remote(current), BackgroundWait::Remote(next)) => {
            current.owner == next.owner
                && current.binding == next.binding
                && current.task_id == next.task_id
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
        "remote continuation requires an owner, binding, task id, and a positive poll interval when present"
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
fn background_execution_admission_is_exact_fenced_live_and_phase_monotone() {
    // All pairs of five phases, both identity/remote predicates and all u64
    // times/deadlines. R1 mismatch or now>=deadline -> reject; R2 Running may
    // start only while still Running; R3 Waiting may observe Waiting/Cancelling
    // with the exact remote target; R4 Cancelling may only continue Cancelling.
    // Requested/Ended and local waits cannot execute. This proves the production
    // decision, not heap identity projection, clock truth or an atomic DB/effect.
    let phases = [
        TaskPhase::Requested,
        TaskPhase::Running,
        TaskPhase::Waiting,
        TaskPhase::Cancelling,
        TaskPhase::Ended,
    ];
    let current: usize = kani::any();
    let expected: usize = kani::any();
    kani::assume(current < 5 && expected < 5);
    let identity: bool = kani::any();
    let remote: bool = kani::any();
    let expires: u64 = kani::any();
    let now: u64 = kani::any();
    let admitted = execution_is_admitted(
        phases[current],
        phases[expected],
        identity,
        expires,
        now,
        remote,
    );
    let phase_allowed = (expected == 1 && current == 1)
        || (remote && expected == 2 && (current == 2 || current == 3))
        || (remote && expected == 3 && current == 3);
    assert_eq!(
        admitted,
        identity && u128::from(now) < u128::from(expires) && phase_allowed
    );
}

#[cfg(kani)]
#[kani::proof]
fn cancellation_is_monotone_and_terminal_states_are_absorbing() {
    // C1 every phase; C2 known/unknown completion. Cancel is a stutter for
    // Cancelling/Ended, marks live tasks, ends Requested. Only known matching
    // completion can be projected to Cancelled; unknown must stay unknown.
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
    let indeterminate: bool = kani::any();
    assert_eq!(
        cancellation_wins_completion(phase, indeterminate),
        phase == TaskPhase::Cancelling && !indeterminate
    );
}

#[cfg(kani)]
#[kani::proof]
fn never_replay_recovery_requires_a_durable_remote_continuation() {
    // C1 recovery mode; C2 remote handle; C3 cancellation. Remote reconnects,
    // local NeverReplay or local cancellation cannot replay an uncertain effect.
    let raw: u8 = kani::any();
    kani::assume(raw < 4);
    let mode = match raw {
        0 => ToolRecoveryMode::NeverReplay,
        1 => ToolRecoveryMode::ReplaySafe,
        2 => ToolRecoveryMode::Idempotent,
        _ => ToolRecoveryMode::DurableRequest,
    };
    let has_remote_continuation: bool = kani::any();
    let cancelling: bool = kani::any();
    assert_eq!(
        expired_attempt_is_reconnectable(mode, has_remote_continuation, cancelling),
        has_remote_continuation || (mode != ToolRecoveryMode::NeverReplay && !cancelling)
    );
    if mode == ToolRecoveryMode::NeverReplay {
        assert_eq!(
            expired_attempt_is_reconnectable(mode, has_remote_continuation, cancelling),
            has_remote_continuation
        );
    }
}

#[cfg(kani)]
#[kani::proof]
fn expired_background_recovery_is_exact_nonwrapping_and_never_invents_termination() {
    // All u64 epochs, all u16 budgets (including defensively handled zero),
    // all recovery modes and both remote/cancel facts. R1 non-remote cancellation
    // or NeverReplay => unknown; R2 exhausted remote => unknown; R3 exhausted
    // replayable local => Failed; R4 otherwise exact successor. Preconditions:
    // caller has admitted expiry. Heap payload/frame, clock admission, aggregate
    // revision atomicity and async commit refinement are not claimed here.
    let raw: u8 = kani::any();
    kani::assume(raw < 4);
    let mode = match raw {
        0 => ToolRecoveryMode::NeverReplay,
        1 => ToolRecoveryMode::ReplaySafe,
        2 => ToolRecoveryMode::Idempotent,
        _ => ToolRecoveryMode::DurableRequest,
    };
    let remote: bool = kani::any();
    let cancelling: bool = kani::any();
    let epoch: u64 = kani::any();
    let maximum: u16 = kani::any();
    let decision = expired_recovery_decision(mode, remote, cancelling, epoch, maximum);
    let unknown = (!remote && (raw == 0 || cancelling))
        || (remote && u128::from(epoch) >= u128::from(maximum));
    let exhausted = !remote && raw != 0 && !cancelling && u128::from(epoch) >= u128::from(maximum);
    assert_eq!(decision == ExpiredRecoveryDecision::Indeterminate, unknown);
    assert_eq!(
        decision == ExpiredRecoveryDecision::AttemptsExhausted,
        exhausted
    );
    match decision {
        ExpiredRecoveryDecision::Acquire(next) => {
            assert!(!unknown && !exhausted);
            assert_eq!(u128::from(next), u128::from(epoch) + 1);
            assert!(next <= u64::from(maximum));
        }
        _ => assert!(unknown || exhausted),
    }
}
