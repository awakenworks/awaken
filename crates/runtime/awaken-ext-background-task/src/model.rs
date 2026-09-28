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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FencedMutationRequest {
    Heartbeat { now_ms: u64, lease_ms: u64 },
    Finish { indeterminate: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FencedMutationPlan {
    Heartbeat {
        lease_expires_at_ms: u64,
        revision: u64,
    },
    Finish {
        cancellation_wins: bool,
        revision: u64,
    },
}

/// Sole admission and scalar successor plan for fenced heartbeat and finish.
///
/// Exact owner+epoch rejection intentionally precedes command-specific input,
/// clock, lease and revision checks. The caller applies a returned plan once;
/// every error therefore leaves the complete aggregate unchanged.
fn fenced_mutation_plan(
    phase: TaskPhase,
    attempt: Option<&TaskAttempt>,
    fence: &TaskFence,
    revision: u64,
    request: FencedMutationRequest,
) -> Result<FencedMutationPlan, BackgroundTaskError> {
    let attempt = attempt
        .filter(|attempt| attempt.owns(fence))
        .ok_or(BackgroundTaskError::StaleFence)?;
    match request {
        FencedMutationRequest::Heartbeat { now_ms, lease_ms } => {
            if lease_ms == 0 {
                return Err(BackgroundTaskError::InvalidClaim);
            }
            let lease_expires_at_ms = now_ms
                .checked_add(lease_ms)
                .ok_or(BackgroundTaskError::ClockOverflow)?;
            if lease_expires_at_ms <= attempt.lease_expires_at_ms {
                return Err(BackgroundTaskError::NonMonotonicLease);
            }
            let revision = revision
                .checked_add(1)
                .ok_or(BackgroundTaskError::RevisionOverflow)?;
            Ok(FencedMutationPlan::Heartbeat {
                lease_expires_at_ms,
                revision,
            })
        }
        FencedMutationRequest::Finish { indeterminate } => {
            let revision = revision
                .checked_add(1)
                .ok_or(BackgroundTaskError::RevisionOverflow)?;
            Ok(FencedMutationPlan::Finish {
                cancellation_wins: cancellation_wins_completion(phase, indeterminate),
                revision,
            })
        }
    }
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

#[derive(Debug, Clone, Copy)]
struct ReclaimMutationFacts {
    lease_expires_at_ms: u64,
    mode: ToolRecoveryMode,
    remote: bool,
    cancelling: bool,
    epoch: u64,
    maximum: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReclaimMutationPlan {
    Acquire {
        epoch: u64,
        lease_expires_at_ms: u64,
        revision: u64,
    },
    EndIndeterminate {
        remote: bool,
        revision: u64,
    },
    EndAttemptsExhausted {
        revision: u64,
    },
}

/// Sole scalar successor plan for reclaiming an expired attempt.
///
/// The aggregate supplies facts from its one lifecycle and applies one returned
/// plan. Rejected phase, expiry, or revision admission produces no plan, while
/// every admitted plan either advances the epoch exactly once or ends the task.
const fn reclaim_mutation_plan(
    phase: TaskPhase,
    facts: Option<ReclaimMutationFacts>,
    now_ms: u64,
    lease_expires_at_ms: u64,
    revision: u64,
) -> Result<ReclaimMutationPlan, BackgroundTaskError> {
    let facts = match phase {
        TaskPhase::Requested => return Err(BackgroundTaskError::InvalidTransition),
        TaskPhase::Ended => return Err(BackgroundTaskError::Terminal),
        TaskPhase::Running | TaskPhase::Waiting | TaskPhase::Cancelling => match facts {
            Some(facts) if facts.lease_expires_at_ms <= now_ms => facts,
            _ => return Err(BackgroundTaskError::Busy),
        },
    };
    let decision = expired_recovery_decision(
        facts.mode,
        facts.remote,
        facts.cancelling,
        facts.epoch,
        facts.maximum,
    );
    let revision = match revision.checked_add(1) {
        Some(revision) => revision,
        None => return Err(BackgroundTaskError::RevisionOverflow),
    };
    match decision {
        ExpiredRecoveryDecision::Acquire(epoch) => Ok(ReclaimMutationPlan::Acquire {
            epoch,
            lease_expires_at_ms,
            revision,
        }),
        ExpiredRecoveryDecision::Indeterminate => Ok(ReclaimMutationPlan::EndIndeterminate {
            remote: facts.remote,
            revision,
        }),
        ExpiredRecoveryDecision::AttemptsExhausted => {
            Ok(ReclaimMutationPlan::EndAttemptsExhausted { revision })
        }
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
        let facts = self
            .lifecycle
            .attempt()
            .map(|attempt| ReclaimMutationFacts {
                lease_expires_at_ms: attempt.lease_expires_at_ms,
                mode: attempt.policy.recovery.mode(),
                remote: remote_reconnectable,
                cancelling,
                epoch: attempt.epoch,
                maximum: attempt.policy.recovery.max_attempts().get(),
            });
        let plan = reclaim_mutation_plan(
            self.lifecycle.phase(),
            facts,
            now_ms,
            expires,
            self.revision,
        )?;
        let (epoch, lease_expires_at_ms, revision) = match plan {
            ReclaimMutationPlan::EndIndeterminate { remote, revision } => {
                let message = if remote {
                    "remote background task observation budget exhausted; remote outcome is unknown"
                } else {
                    "worker lease expired without a safe recovery or cancellation observation"
                };
                self.apply_planned_lifecycle(
                    BackgroundTaskLifecycle::Ended {
                        end: BackgroundTaskEnd::Indeterminate {
                            message: message.into(),
                        },
                    },
                    revision,
                );
                return Ok(TaskClaim::EndedIndeterminate);
            }
            ReclaimMutationPlan::EndAttemptsExhausted { revision } => {
                self.apply_planned_lifecycle(
                    BackgroundTaskLifecycle::Ended {
                        end: BackgroundTaskEnd::Failed {
                            message: "background task recovery attempt budget exhausted".into(),
                        },
                    },
                    revision,
                );
                return Ok(TaskClaim::EndedAttemptsExhausted);
            }
            ReclaimMutationPlan::Acquire {
                epoch,
                lease_expires_at_ms,
                revision,
            } => (epoch, lease_expires_at_ms, revision),
        };
        let policy = self
            .lifecycle
            .attempt()
            .expect("reclaim plan admits only a live attempt")
            .policy
            .clone();
        let attempt = TaskAttempt {
            worker_id,
            epoch,
            lease_expires_at_ms,
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
        self.apply_planned_lifecycle(lifecycle, revision);
        Ok(TaskClaim::Acquired(fence))
    }

    pub fn heartbeat(
        &mut self,
        fence: &TaskFence,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<(), BackgroundTaskError> {
        let FencedMutationPlan::Heartbeat {
            lease_expires_at_ms,
            revision,
        } = fenced_mutation_plan(
            self.lifecycle.phase(),
            self.lifecycle.attempt(),
            fence,
            self.revision,
            FencedMutationRequest::Heartbeat { now_ms, lease_ms },
        )?
        else {
            unreachable!("heartbeat request returns a heartbeat plan")
        };
        let lifecycle = match &self.lifecycle {
            BackgroundTaskLifecycle::Running { attempt } => {
                let mut attempt = attempt.clone();
                attempt.lease_expires_at_ms = lease_expires_at_ms;
                BackgroundTaskLifecycle::Running { attempt }
            }
            BackgroundTaskLifecycle::Waiting { attempt, wait } => {
                let mut attempt = attempt.clone();
                attempt.lease_expires_at_ms = lease_expires_at_ms;
                BackgroundTaskLifecycle::Waiting {
                    attempt,
                    wait: wait.clone(),
                }
            }
            BackgroundTaskLifecycle::Cancelling { attempt, wait } => {
                let mut attempt = attempt.clone();
                attempt.lease_expires_at_ms = lease_expires_at_ms;
                BackgroundTaskLifecycle::Cancelling {
                    attempt,
                    wait: wait.clone(),
                }
            }
            BackgroundTaskLifecycle::Requested | BackgroundTaskLifecycle::Ended { .. } => {
                unreachable!("fenced mutation plan rejects phases without an attempt")
            }
        };
        self.apply_planned_lifecycle(lifecycle, revision);
        Ok(())
    }

    pub fn wait(
        &mut self,
        fence: &TaskFence,
        wait: BackgroundWait,
    ) -> Result<(), BackgroundTaskError> {
        if let BackgroundWait::Remote(handle) = &wait {
            validate_task_handle(handle)?;
        }
        self.owned_attempt(fence)?;
        let lifecycle = match &self.lifecycle {
            BackgroundTaskLifecycle::Running { attempt } => BackgroundTaskLifecycle::Waiting {
                attempt: attempt.clone(),
                wait,
            },
            BackgroundTaskLifecycle::Waiting {
                attempt,
                wait: current,
            } if same_wait_target(current, &wait) => BackgroundTaskLifecycle::Waiting {
                attempt: attempt.clone(),
                wait,
            },
            BackgroundTaskLifecycle::Cancelling {
                attempt,
                wait: current,
            } if cancelling_accepts_wait(current.as_ref(), &wait) => {
                BackgroundTaskLifecycle::Cancelling {
                    attempt: attempt.clone(),
                    wait: Some(wait),
                }
            }
            _ => return Err(BackgroundTaskError::InvalidTransition),
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
        let FencedMutationPlan::Finish {
            cancellation_wins,
            revision,
        } = fenced_mutation_plan(
            self.lifecycle.phase(),
            self.lifecycle.attempt(),
            fence,
            self.revision,
            FencedMutationRequest::Finish {
                indeterminate: matches!(end, BackgroundTaskEnd::Indeterminate { .. }),
            },
        )?
        else {
            unreachable!("finish request returns a finish plan")
        };
        let end = if cancellation_wins {
            BackgroundTaskEnd::Cancelled
        } else {
            end
        };
        self.apply_planned_lifecycle(BackgroundTaskLifecycle::Ended { end }, revision);
        Ok(())
    }

    /// Resolve the one current worker/epoch authority before a fenced command
    /// performs command-specific validation or constructs a successor state.
    fn owned_attempt(&self, fence: &TaskFence) -> Result<&TaskAttempt, BackgroundTaskError> {
        self.lifecycle
            .attempt()
            .filter(|attempt| attempt.owns(fence))
            .ok_or(BackgroundTaskError::StaleFence)
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

    fn apply_planned_lifecycle(&mut self, lifecycle: BackgroundTaskLifecycle, revision: u64) {
        debug_assert_eq!(self.revision.checked_add(1), Some(revision));
        self.lifecycle = lifecycle;
        self.revision = revision;
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
/// @obligation background_task.single_fenced_attempt
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
/// @obligation background_task.stale_fence_never_finishes_or_heartbeats
#[kani::proof]
fn stale_fenced_heartbeat_and_finish_have_no_mutation_plan() {
    // Cause/effect decision table over the sole production mutation plan:
    // C1 all five lifecycle phases; C2 owner mismatch or epoch mismatch; C3
    // heartbeat or finish; C4 all u64 epochs, revisions, clock values and lease
    // values plus both finish outcomes. R1 either mismatch returns StaleFence;
    // R2 rejection precedes every command-specific validation, so no successor
    // plan exists for either production caller to apply. Fixed distinct worker
    // strings exercise Rust's actual String equality; heap allocation and the
    // later durable Thread commit are outside this scalar safety proof.
    let phase_raw: u8 = kani::any();
    kani::assume(phase_raw < 5);
    let phase = match phase_raw {
        0 => TaskPhase::Requested,
        1 => TaskPhase::Running,
        2 => TaskPhase::Waiting,
        3 => TaskPhase::Cancelling,
        _ => TaskPhase::Ended,
    };
    let epoch: u64 = kani::any();
    let attempt = TaskAttempt {
        worker_id: "current-worker".into(),
        epoch,
        lease_expires_at_ms: kani::any(),
        policy: TaskExecutionPolicy {
            recovery: ToolRecoveryPolicy::replay_safe(),
            concurrency: ToolConcurrency::Parallel,
        },
    };
    let request = if kani::any() {
        FencedMutationRequest::Heartbeat {
            now_ms: kani::any(),
            lease_ms: kani::any(),
        }
    } else {
        FencedMutationRequest::Finish {
            indeterminate: kani::any(),
        }
    };
    let revision: u64 = kani::any();
    let owner_mismatch = TaskFence {
        worker_id: "stale-worker".into(),
        epoch,
    };
    let epoch_mismatch = TaskFence {
        worker_id: "current-worker".into(),
        epoch: epoch ^ 1,
    };

    for stale in [&owner_mismatch, &epoch_mismatch] {
        assert_eq!(
            fenced_mutation_plan(phase, Some(&attempt), stale, revision, request),
            Err(BackgroundTaskError::StaleFence)
        );
    }
}

#[cfg(kani)]
/// @obligation background_task.cancellation_absorption
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
/// @obligation background_task.never_replay_and_attempt_exhaustion_fail_closed
/// @obligation background_task.remote_continuation_survives_wait_cancel_and_reclaim
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
/// @obligation background_task.expiry_reclaim_advances_epoch_or_terminates
/// @obligation background_task.never_replay_and_attempt_exhaustion_fail_closed
#[kani::proof]
fn expired_reclaim_plan_advances_epoch_or_terminates() {
    // Cause/effect decision table over the production scalar plan: C1 all five
    // phases; C2 attempt present/absent; C3 all u64 lease deadlines/times; C4
    // all four recovery modes, remote/cancelling facts, u64 epochs, u16 budgets;
    // C5 all u64 revisions and successor lease values. R1 Requested/Ended use
    // their exact errors; R2 a live missing/non-expired attempt is Busy; R3 an
    // admitted revision overflow yields no plan; R4 every other admitted expiry
    // either advances epoch exactly once or returns a conservative terminal
    // disposition. String/payload construction and durable commit are outside
    // this scalar obligation.
    let phase_raw: u8 = kani::any();
    kani::assume(phase_raw < 5);
    let phase = match phase_raw {
        0 => TaskPhase::Requested,
        1 => TaskPhase::Running,
        2 => TaskPhase::Waiting,
        3 => TaskPhase::Cancelling,
        _ => TaskPhase::Ended,
    };
    let mode_raw: u8 = kani::any();
    kani::assume(mode_raw < 4);
    let mode = match mode_raw {
        0 => ToolRecoveryMode::NeverReplay,
        1 => ToolRecoveryMode::ReplaySafe,
        2 => ToolRecoveryMode::Idempotent,
        _ => ToolRecoveryMode::DurableRequest,
    };
    let has_facts: bool = kani::any();
    let remote: bool = kani::any();
    let cancelling: bool = kani::any();
    let epoch: u64 = kani::any();
    let maximum: u16 = kani::any();
    let current_lease_expires_at_ms: u64 = kani::any();
    let now_ms: u64 = kani::any();
    let next_lease_expires_at_ms: u64 = kani::any();
    let revision: u64 = kani::any();
    let facts = has_facts.then_some(ReclaimMutationFacts {
        lease_expires_at_ms: current_lease_expires_at_ms,
        mode,
        remote,
        cancelling,
        epoch,
        maximum,
    });
    let plan = reclaim_mutation_plan(phase, facts, now_ms, next_lease_expires_at_ms, revision);
    if phase == TaskPhase::Requested {
        assert_eq!(plan, Err(BackgroundTaskError::InvalidTransition));
        return;
    }
    if phase == TaskPhase::Ended {
        assert_eq!(plan, Err(BackgroundTaskError::Terminal));
        return;
    }
    if !has_facts || current_lease_expires_at_ms > now_ms {
        assert_eq!(plan, Err(BackgroundTaskError::Busy));
        return;
    }
    if revision == u64::MAX {
        assert_eq!(plan, Err(BackgroundTaskError::RevisionOverflow));
        return;
    }
    let unknown = (!remote && (mode_raw == 0 || cancelling))
        || (remote && u128::from(epoch) >= u128::from(maximum));
    let exhausted =
        !remote && mode_raw != 0 && !cancelling && u128::from(epoch) >= u128::from(maximum);
    match plan.expect("admitted expiry and revision capacity return a plan") {
        ReclaimMutationPlan::Acquire {
            epoch: next,
            lease_expires_at_ms,
            revision: next_revision,
        } => {
            assert!(!unknown && !exhausted);
            assert_eq!(u128::from(next), u128::from(epoch) + 1);
            assert!(next <= u64::from(maximum));
            assert_eq!(lease_expires_at_ms, next_lease_expires_at_ms);
            assert_eq!(next_revision, revision + 1);
        }
        ReclaimMutationPlan::EndIndeterminate {
            remote: planned_remote,
            revision: next_revision,
        } => {
            assert!(unknown);
            assert_eq!(planned_remote, remote);
            assert_eq!(next_revision, revision + 1);
        }
        ReclaimMutationPlan::EndAttemptsExhausted {
            revision: next_revision,
        } => {
            assert!(exhausted);
            assert_eq!(next_revision, revision + 1);
        }
    }
}
