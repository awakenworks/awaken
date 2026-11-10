// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
//! Product post-commit execution of the state-owned BackgroundTask aggregate.
//!
//! This adapter observes committed Running claims and invokes their canonical
//! ordinary tool through Runtime. It writes no database. Completion stays
//! process-local while a deterministic ordinary Session Run wakes the same
//! Thread; the extension's StepStart hook folds the fenced candidate before
//! inference and the normal Thread commit remains the only persistence path.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::Store;
use awaken_agent_contract::stable_fingerprint;
use awaken_ext_background_task::{
    BackgroundTask, BackgroundTaskLifecycle, BackgroundTaskSupervisor, tasks_from_state,
};
use awaken_runtime::{PreparedToolExecutor, Runtime};
use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_runtime_contract::resolved::ToolKind;
use awaken_runtime_contract::runs::RunErrorKind;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{
    CommittedTerminalRun, RunTerminalObserver, RunTerminalObserverError,
};
use awaken_session_application_contract::{PrivateTaskInput, TaskAttention};
use awaken_time::WallClockExt;
use awaken_tool_contract::tool::background_task::{
    BackgroundTaskCompletion, BackgroundTaskEnd, BackgroundTaskId, BackgroundTaskWaitCandidate,
    BackgroundWait, TaskObservationCodecError, TaskObservationRecord, TaskTenure,
};
use awaken_tool_contract::tool::{ToolOutput, ToolTaskHandle, ToolTaskPoll, ToolTaskStart};

#[cfg(test)]
use awaken_session_run_delivery::SessionCommit;
use awaken_session_run_delivery::{BackgroundRuns, BackgroundWorkClass};

mod ownership;
mod placement;

pub(crate) use placement::{TaskAttentionRoute, TaskCommittedReads};

pub(crate) const OBSERVER_ID: &str = "awaken.background-task.executor.v1";
const ATTENTION_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
];
const REMOTE_OBSERVATION_RETRIES: usize = 3;
const DEFAULT_REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn attention_identity(
    thread_id: &awaken_agent_contract::agent::thread::Id,
    task_id: &BackgroundTaskId,
    fence: &TaskTenure,
    kind: &BackgroundAttention,
) -> String {
    stable_fingerprint(&(
        "background-task-attention-v1",
        thread_id.0.as_str(),
        task_id.as_str(),
        fence.holder().as_str(),
        fence.epoch().get(),
        kind.identity(),
    ))
}

#[derive(Clone)]
enum BackgroundAttention {
    Waiting,
    InputRequired { change: String },
    Watchdog { change: String },
    Terminal,
}

impl BackgroundAttention {
    fn identity(&self) -> &str {
        match self {
            Self::Waiting => "waiting",
            Self::InputRequired { change } | Self::Watchdog { change } => change,
            Self::Terminal => "terminal",
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Waiting => "entered durable remote execution",
            Self::InputRequired { .. } => "requires input or a decision",
            Self::Watchdog { .. } => "requires an observation-lease checkpoint",
            Self::Terminal => "produced a terminal completion candidate",
        }
    }
}

fn attention_message(
    thread_id: &awaken_agent_contract::agent::thread::Id,
    task_id: &BackgroundTaskId,
    fence: &TaskTenure,
    kind: &BackgroundAttention,
) -> (String, Message) {
    let identity = attention_identity(thread_id, task_id, fence, kind);
    let operation_id = format!("background-task-attention-{identity}");
    let message = Message::text(
        MessageId(format!("background-task-attention-message-{identity}")),
        Role::System,
        format!(
            "Background task {} has {}.\n\n\
             The runtime reconciles the latest fenced task state before this inference. \
             Accepted terminal results are supplied directly as source-labelled messages \
             according to the published notification policy. Continue from those results \
             without repeating work already completed by the task.",
            task_id.as_str(),
            kind.reason(),
        ),
    );
    (operation_id, message)
}

/// The observation one attention delivers: the slot's candidate for exactly
/// this attempt, encoded for the Run that folds it.
///
/// Decision table (attention kind × slot → carried input):
/// | Rule | kind | slot | input |
/// |---|---|---|---|
/// | P1 | Terminal | Completed with this tenure | its Ended record |
/// | P2 | Waiting, Watchdog | Waiting with this tenure | its Waiting record |
/// | P3 | InputRequired | any | none: the attention only wakes the Thread |
/// | P4 | any | vacant, another tenure or another phase | none |
fn attention_input(
    supervisor: &BackgroundTaskSupervisor,
    task_id: &BackgroundTaskId,
    fence: &TaskTenure,
    kind: &BackgroundAttention,
) -> Option<PrivateTaskInput> {
    let record = match kind {
        BackgroundAttention::Terminal => supervisor
            .completion(task_id)
            .filter(|completion| &completion.fence == fence)
            .map(|completion| TaskObservationRecord::ended(&completion)),
        BackgroundAttention::Waiting | BackgroundAttention::Watchdog { .. } => supervisor
            .wait_candidate(task_id)
            .filter(|candidate| &candidate.fence == fence)
            .map(|candidate| TaskObservationRecord::waiting(&candidate)),
        BackgroundAttention::InputRequired { .. } => None,
    }?;
    PrivateTaskInput::new(vec![record.encode()]).ok()
}

/// Offer the Task observations a claimed Run carries to this process's Task
/// owner before the Run executes, so its StepStart can fold them against
/// committed truth. Every record decodes before any is offered.
///
/// Decision table (causes → effect):
/// | Rule | cause | effect |
/// |---|---|---|
/// | C2 | the Run carries no private Task input | nothing offered |
/// | C4 | every record decodes | each offered (`accept_delivered`, D1-D7) |
/// | C5 | any record is refused by the Task codec | refused; nothing offered |
pub(crate) fn accept_claimed_task_input(
    supervisor: &BackgroundTaskSupervisor,
    dispatch: &awaken_dispatch_contract::RunDispatch,
) -> Result<(), TaskObservationCodecError> {
    let Some(input) = dispatch.private_task_input.as_ref() else {
        return Ok(());
    };
    // Old v1 without tenure remains inspectable, never an authenticated remote
    // permission. No current attention producer persists an unbound input.
    let read = input.tenure().ok_or_else(|| {
        TaskObservationCodecError::Decode("private Task input has no authenticated tenure".into())
    })?;
    read.validate().map_err(TaskObservationCodecError::Decode)?;
    if read.thread != dispatch.thread_id().0
        || read.session_thread() != dispatch.session_thread_id().0
    {
        return Err(TaskObservationCodecError::Decode(
            "private Task input names another dispatch partition".into(),
        ));
    }
    let task = BackgroundTaskId::new(read.task.clone())
        .map_err(|error| TaskObservationCodecError::Decode(error.to_string()))?;
    let expected = TaskTenure::granted(task, read.holder.clone(), read.epoch);
    let records = TaskObservationRecord::decode_bound_batch(input.observations(), &expected)?;
    for record in &records {
        supervisor.accept_delivered(record);
    }
    Ok(())
}

async fn publish_attention(
    route: Option<TaskAttentionRoute>,
    supervisor: &BackgroundTaskSupervisor,
    tenure: &awaken_session_application_contract::TaskTenureRead,
    task_id: &BackgroundTaskId,
    fence: &TaskTenure,
    kind: BackgroundAttention,
) {
    let Some(target) = route.as_ref().and_then(TaskAttentionRoute::target) else {
        return;
    };
    let thread_id = awaken_agent_contract::agent::thread::Id(tenure.thread.clone());
    let (operation_id, message) = attention_message(&thread_id, task_id, fence, &kind);
    let attention = TaskAttention {
        operation_id: operation_id.clone(),
        tenure: tenure.clone(),
        message,
        input: attention_input(supervisor, task_id, fence, &kind),
    };
    let attempts = ATTENTION_RETRY_DELAYS
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None));
    for (attempt, retry_delay) in attempts.enumerate() {
        match target.deliver(attention.clone()).await {
            Ok(_) => return,
            Err(error) if error.kind == RunErrorKind::BadRequest => {
                tracing::warn!(
                    task_id = task_id.as_str(),
                    %operation_id,
                    %error,
                    "background task attention Run was definitively rejected"
                );
                return;
            }
            Err(error) if retry_delay.is_some() => {
                tracing::warn!(
                    task_id = task_id.as_str(),
                    %operation_id,
                    %error,
                    attempt = attempt + 1,
                    "background task attention Run publication is retrying"
                );
                tokio::time::sleep(retry_delay.expect("guarded by is_some")).await;
            }
            Err(error) => {
                tracing::warn!(
                    task_id = task_id.as_str(),
                    %operation_id,
                    %error,
                    "background task attention Run publication exhausted process-local retries"
                );
                return;
            }
        }
    }
}

pub(crate) struct BackgroundTaskTerminalObserver {
    runtime: Arc<Runtime>,
    snapshot: ExecutableAgentSnapshot,
    context: RuntimeRunContext,
    commit: TaskCommittedReads,
    background: Arc<BackgroundRuns>,
    supervisor: Arc<BackgroundTaskSupervisor>,
    session_id: String,
    environment_generation: String,
    attention: Option<TaskAttentionRoute>,
}

impl BackgroundTaskTerminalObserver {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        snapshot: ExecutableAgentSnapshot,
        context: RuntimeRunContext,
        commit: TaskCommittedReads,
        background: Arc<BackgroundRuns>,
        supervisor: Arc<BackgroundTaskSupervisor>,
        session_id: String,
        environment_generation: String,
        attention: Option<TaskAttentionRoute>,
    ) -> Self {
        Self {
            runtime,
            snapshot,
            context,
            commit,
            background,
            supervisor,
            session_id,
            environment_generation,
            attention,
        }
    }

    fn completion(
        fence: TaskTenure,
        output: Result<ToolOutput, String>,
    ) -> BackgroundTaskCompletion {
        let end = match output {
            Ok(output) if output.state.is_empty() => BackgroundTaskEnd::Completed {
                content: output.content,
                is_error: output.is_error,
            },
            Ok(_) => BackgroundTaskEnd::Failed {
                message: "detached tool returned State commands; only its owning Runtime commit path may apply them".into(),
            },
            Err(message) => BackgroundTaskEnd::Failed { message },
        };
        BackgroundTaskCompletion { fence, end }
    }

    fn indeterminate(fence: TaskTenure, message: impl Into<String>) -> BackgroundTaskCompletion {
        BackgroundTaskCompletion {
            fence,
            end: BackgroundTaskEnd::Indeterminate {
                message: message.into(),
            },
        }
    }

    fn start_failure(
        fence: TaskTenure,
        kind: ToolKind,
        message: impl Into<String>,
    ) -> BackgroundTaskCompletion {
        let message = message.into();
        if kind == ToolKind::DetachedOnly {
            Self::indeterminate(
                fence,
                format!("detached task start outcome is unknown: {message}"),
            )
        } else {
            Self::completion(fence, Err(message))
        }
    }

    fn remote_wait(handle: &ToolTaskHandle) -> Result<BackgroundWait, String> {
        handle.validate().map_err(|error| error.to_string())?;
        Ok(BackgroundWait::Remote(handle.clone()))
    }

    async fn launch(&self, task: BackgroundTask, observed: &RunId) {
        let attempt = task.attempt().expect("committed Running attempt");
        let fence = attempt.tenure(task.id());
        let attention_tenure = placement::task_tenure_read(&task, &self.session_id);
        let expected = attempt.policy.clone();
        let Some(cancellation) = self.supervisor.register(task.id(), &fence) else {
            return;
        };
        let context = self.execution_context(&task, cancellation);
        let task_id = task.id().clone();
        let call = task.invocation().call.clone();
        let thread_id = task.origin().thread_id.clone();
        let prepared =
            match PreparedToolExecutor::new(self.runtime.clone(), &self.snapshot, context) {
                Ok(prepared) => Arc::new(prepared),
                Err(error) => {
                    let completion = Self::completion(fence.clone(), Err(error.to_string()));
                    self.supervisor.complete(task_id.clone(), completion);
                    publish_attention(
                        self.attention.clone(),
                        &self.supervisor,
                        &attention_tenure,
                        &task_id,
                        &fence,
                        BackgroundAttention::Terminal,
                    )
                    .await;
                    return;
                }
            };
        let resolved = match prepared.execution(&call) {
            Ok(resolved) => resolved,
            Err(error) => {
                let completion = Self::completion(fence.clone(), Err(error.to_string()));
                self.supervisor.complete(task_id.clone(), completion);
                publish_attention(
                    self.attention.clone(),
                    &self.supervisor,
                    &attention_tenure,
                    &task_id,
                    &fence,
                    BackgroundAttention::Terminal,
                )
                .await;
                return;
            }
        };
        if resolved.recovery != expected.recovery || resolved.concurrency != expected.concurrency {
            self.supervisor.complete(
                task_id.clone(),
                Self::completion(
                    fence.clone(),
                    Err("canonical tool execution facts changed after the committed claim".into()),
                ),
            );
            publish_attention(
                self.attention.clone(),
                &self.supervisor,
                &attention_tenure,
                &task_id,
                &fence,
                BackgroundAttention::Terminal,
            )
            .await;
            return;
        }
        let tool_kind = resolved.kind;
        let state = match self.commit.committed_state(&thread_id, observed).await {
            Ok(state) => Store::rebuild(&state),
            Err(error) => {
                self.supervisor.complete(
                    task_id.clone(),
                    Self::completion(
                        fence.clone(),
                        Err(format!("read committed background task state: {error}")),
                    ),
                );
                publish_attention(
                    self.attention.clone(),
                    &self.supervisor,
                    &attention_tenure,
                    &task_id,
                    &fence,
                    BackgroundAttention::Terminal,
                )
                .await;
                return;
            }
        };
        let supervisor = self.supervisor.clone();
        let attention = self.attention.clone();
        let class = BackgroundWorkClass::SharedEnvironment {
            session_id: self.session_id.clone(),
            generation_id: self.environment_generation.clone(),
        };
        self.background
            .spawn(class, async move {
                let run_id =
                    awaken_agent_contract::agent::run::Id(format!("background-{}", task_id.as_str()));
                let result = prepared
                    .start_task(
                        &run_id,
                        &thread_id,
                        format!("background:{}", task_id.as_str()),
                        &call,
                        &state,
                    )
                    .await;
                match result {
                    Ok(ToolTaskStart::Pending(handle)) => match Self::remote_wait(&handle) {
                        Ok(wait) => {
                            supervisor.wait(
                                task_id.clone(),
                                BackgroundTaskWaitCandidate {
                                    fence: fence.clone(),
                                    wait,
                                    renew_lease: false,
                                },
                            );
                            publish_attention(
                                attention,
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::Waiting,
                            )
                            .await;
                        }
                        Err(error) => {
                            supervisor.complete(
                                task_id.clone(),
                                Self::indeterminate(
                                    fence.clone(),
                                    format!("detached task returned an unusable continuation: {error}"),
                                ),
                            );
                            publish_attention(
                                attention,
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::Terminal,
                            )
                            .await;
                        }
                    },
                    Ok(ToolTaskStart::Completed(output)) => {
                        supervisor
                            .complete(task_id.clone(), Self::completion(fence.clone(), Ok(output)));
                        publish_attention(
                            attention,
                            &supervisor,
                            &attention_tenure,
                            &task_id,
                            &fence,
                            BackgroundAttention::Terminal,
                        )
                        .await;
                    }
                    Err(error @ (awaken_runtime::DetachedToolError::StateInvalidated
                        | awaken_runtime::DetachedToolError::Indeterminate)) => {
                        supervisor.complete(task_id.clone(), Self::indeterminate(fence.clone(), error.to_string()));
                        publish_attention(
                            attention,
                            &supervisor,
                            &attention_tenure,
                            &task_id,
                            &fence,
                            BackgroundAttention::Terminal,
                        )
                        .await;
                    }
                    Err(error) => {
                        supervisor.complete(
                            task_id.clone(),
                            Self::start_failure(fence.clone(), tool_kind, error.to_string()),
                        );
                        publish_attention(
                            attention,
                            &supervisor,
                            &attention_tenure,
                            &task_id,
                            &fence,
                            BackgroundAttention::Terminal,
                        )
                        .await;
                    }
                }
            })
            .await;
    }

    async fn launch_remote(&self, task: BackgroundTask, observed: &RunId) {
        let (attempt, continuation, cancelling) = match task.lifecycle() {
            BackgroundTaskLifecycle::Waiting {
                attempt,
                wait: BackgroundWait::Remote(handle),
            } => (attempt, handle.clone(), false),
            BackgroundTaskLifecycle::Cancelling {
                attempt,
                wait: Some(BackgroundWait::Remote(handle)),
            } => (attempt, handle.clone(), true),
            _ => unreachable!("committed remote continuation"),
        };
        let fence = attempt.tenure(task.id());
        let attention_tenure = placement::task_tenure_read(&task, &self.session_id);
        let expected = attempt.policy.clone();
        let lease_expires_at_ms = attempt.lease_expires_at_ms;
        let cancellation = self
            .supervisor
            .resume_wait(task.id(), &fence)
            .or_else(|| self.supervisor.register(task.id(), &fence));
        let Some(cancellation) = cancellation else {
            if cancelling {
                self.supervisor.cancel(task.id(), &fence);
            }
            return;
        };
        let context = self.execution_context(&task, cancellation.clone());
        let task_id = task.id().clone();
        let call = task.invocation().call.clone();
        let thread_id = task.origin().thread_id.clone();
        let prepared =
            match PreparedToolExecutor::new(self.runtime.clone(), &self.snapshot, context) {
                Ok(prepared) => Arc::new(prepared),
                Err(error) => {
                    self.supervisor.complete(
                        task_id.clone(),
                        Self::indeterminate(
                            fence.clone(),
                            format!("remote background task cannot be resumed: {error}"),
                        ),
                    );
                    publish_attention(
                        self.attention.clone(),
                        &self.supervisor,
                        &attention_tenure,
                        &task_id,
                        &fence,
                        BackgroundAttention::Terminal,
                    )
                    .await;
                    return;
                }
            };
        let resolved = match prepared.execution(&call) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.supervisor.complete(
                    task_id.clone(),
                    Self::indeterminate(
                        fence.clone(),
                        format!("remote background task binding cannot be resolved: {error}"),
                    ),
                );
                publish_attention(
                    self.attention.clone(),
                    &self.supervisor,
                    &attention_tenure,
                    &task_id,
                    &fence,
                    BackgroundAttention::Terminal,
                )
                .await;
                return;
            }
        };
        if resolved.recovery != expected.recovery || resolved.concurrency != expected.concurrency {
            self.supervisor.complete(
                task_id.clone(),
                Self::indeterminate(
                    fence.clone(),
                    "canonical tool execution facts changed after the remote continuation was committed",
                ),
            );
            publish_attention(
                self.attention.clone(),
                &self.supervisor,
                &attention_tenure,
                &task_id,
                &fence,
                BackgroundAttention::Terminal,
            )
            .await;
            return;
        }
        let mut handle = continuation;
        if let Err(error) = handle.validate() {
            self.supervisor.complete(
                task_id.clone(),
                Self::indeterminate(
                    fence.clone(),
                    format!("committed remote background task handle is invalid: {error}"),
                ),
            );
            publish_attention(
                self.attention.clone(),
                &self.supervisor,
                &attention_tenure,
                &task_id,
                &fence,
                BackgroundAttention::Terminal,
            )
            .await;
            return;
        }
        let state = match self.commit.committed_state(&thread_id, observed).await {
            Ok(state) => Store::rebuild(&state),
            Err(error) => {
                self.supervisor.complete(
                    task_id.clone(),
                    Self::completion(
                        fence.clone(),
                        Err(format!("read committed background task state: {error}")),
                    ),
                );
                publish_attention(
                    self.attention.clone(),
                    &self.supervisor,
                    &attention_tenure,
                    &task_id,
                    &fence,
                    BackgroundAttention::Terminal,
                )
                .await;
                return;
            }
        };
        let supervisor = self.supervisor.clone();
        let attention = self.attention.clone();
        let class = BackgroundWorkClass::SharedEnvironment {
            session_id: self.session_id.clone(),
            generation_id: self.environment_generation.clone(),
        };
        self.background
            .spawn(class, async move {
                let run_id =
                    awaken_agent_contract::agent::run::Id(format!("background-{}", task_id.as_str()));
                let renew_at = lease_expires_at_ms
                    .saturating_sub(BackgroundTaskSupervisor::lease_ms() / 2);
                let mut cancelling = cancelling;
                let mut cancel_sent = false;
                let mut failures = 0usize;
                loop {
                    let now = supervisor.clock().now_ms();
                    if now >= renew_at {
                        let wait = Self::remote_wait(&handle).expect("validated task handle");
                        supervisor.wait(
                            task_id.clone(),
                            BackgroundTaskWaitCandidate {
                                fence: fence.clone(),
                                wait,
                                renew_lease: true,
                            },
                        );
                        let change = stable_fingerprint(&(
                            "background-task-watchdog-v1",
                            task_id.as_str(),
                            fence.epoch().get(),
                            lease_expires_at_ms,
                        ));
                        publish_attention(
                            attention,
                            &supervisor,
                            &attention_tenure,
                            &task_id,
                            &fence,
                            BackgroundAttention::Watchdog { change },
                        )
                        .await;
                        return;
                    }
                    let poll_delay = handle
                        .poll_interval_ms
                        .map(Duration::from_millis)
                        .unwrap_or(DEFAULT_REMOTE_POLL_INTERVAL);
                    let renew_delay = Duration::from_millis(renew_at.saturating_sub(now));
                    tokio::select! {
                        _ = tokio::time::sleep(poll_delay.min(renew_delay)) => {}
                        _ = cancellation.cancelled(), if !cancelling => cancelling = true,
                    }
                    if supervisor.clock().now_ms() >= renew_at {
                        continue;
                    }
                    let operation = format!(
                        "background:{}:{}:{}",
                        task_id.as_str(),
                        fence.epoch().get(),
                        if cancelling && !cancel_sent { "cancel" } else { "poll" }
                    );
                    let observed = if cancelling && !cancel_sent {
                        cancel_sent = true;
                        prepared
                            .cancel_task(
                                &run_id,
                                &thread_id,
                                operation,
                                &call,
                                &handle,
                                &state,
                            )
                            .await
                    } else {
                        prepared
                            .poll_task(
                                &run_id,
                                &thread_id,
                                operation,
                                &call,
                                &handle,
                                &state,
                            )
                            .await
                    };
                    let observed = match observed {
                        Ok(observed) => {
                            failures = 0;
                            observed
                        }
                        Err(error @ (awaken_runtime::DetachedToolError::StateInvalidated
                            | awaken_runtime::DetachedToolError::Indeterminate)) => {
                            supervisor.complete(
                                task_id.clone(),
                                Self::indeterminate(fence.clone(), error.to_string()),
                            );
                            publish_attention(
                                attention,
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::Terminal,
                            )
                            .await;
                            return;
                        }
                        Err(error) => {
                            failures += 1;
                            if failures < REMOTE_OBSERVATION_RETRIES {
                                continue;
                            }
                            supervisor.complete(
                                task_id.clone(),
                                BackgroundTaskCompletion {
                                    fence: fence.clone(),
                                    end: BackgroundTaskEnd::Indeterminate {
                                        message: format!(
                                            "remote background task outcome is unknown after observation failure: {error}"
                                        ),
                                    },
                                },
                            );
                            publish_attention(
                                attention,
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::Terminal,
                            )
                            .await;
                            return;
                        }
                    };
                    match observed {
                        ToolTaskPoll::Pending { poll_interval_ms } => {
                            handle.poll_interval_ms = poll_interval_ms.or(handle.poll_interval_ms);
                        }
                        ToolTaskPoll::InputRequired {
                            poll_interval_ms,
                            message,
                        } => {
                            handle.poll_interval_ms = poll_interval_ms.or(handle.poll_interval_ms);
                            let change = stable_fingerprint(&(
                                "background-task-input-required-v1",
                                task_id.as_str(),
                                fence.epoch().get(),
                                message.as_deref().unwrap_or_default(),
                            ));
                            publish_attention(
                                attention.clone(),
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::InputRequired { change },
                            )
                            .await;
                        }
                        ToolTaskPoll::Completed(output) => {
                            supervisor.complete(
                                task_id.clone(),
                                Self::completion(fence.clone(), Ok(output)),
                            );
                            publish_attention(
                                attention,
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::Terminal,
                            )
                            .await;
                            return;
                        }
                        ToolTaskPoll::Failed { message } => {
                            supervisor.complete(
                                task_id.clone(),
                                BackgroundTaskCompletion {
                                    fence: fence.clone(),
                                    end: BackgroundTaskEnd::Failed { message },
                                },
                            );
                            publish_attention(
                                attention,
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::Terminal,
                            )
                            .await;
                            return;
                        }
                        ToolTaskPoll::Cancelled => {
                            supervisor.complete(
                                task_id.clone(),
                                BackgroundTaskCompletion {
                                    fence: fence.clone(),
                                    end: BackgroundTaskEnd::Cancelled {},
                                },
                            );
                            publish_attention(
                                attention,
                                &supervisor,
                                &attention_tenure,
                                &task_id,
                                &fence,
                                BackgroundAttention::Terminal,
                            )
                            .await;
                            return;
                        }
                    }
                }
            })
            .await;
    }

    async fn reconcile(&self, terminal: &CommittedTerminalRun) -> Result<(), String> {
        let state = Store::rebuild(
            &self
                .commit
                .committed_state(&terminal.thread_id, &terminal.run_id)
                .await?,
        );
        for task in tasks_from_state(&state).map_err(|error| error.to_string())? {
            match task.lifecycle() {
                BackgroundTaskLifecycle::Running { attempt }
                    if attempt.worker_id == self.supervisor.worker_id() =>
                {
                    self.launch(task, &terminal.run_id).await;
                }
                BackgroundTaskLifecycle::Waiting {
                    attempt,
                    wait: BackgroundWait::Remote(_),
                }
                | BackgroundTaskLifecycle::Cancelling {
                    attempt,
                    wait: Some(BackgroundWait::Remote(_)),
                } if attempt.worker_id == self.supervisor.worker_id() => {
                    self.launch_remote(task, &terminal.run_id).await;
                }
                BackgroundTaskLifecycle::Cancelling { attempt, .. }
                    if attempt.worker_id == self.supervisor.worker_id() =>
                {
                    self.supervisor
                        .cancel(task.id(), &attempt.tenure(task.id()));
                }
                BackgroundTaskLifecycle::Ended { .. } => self.supervisor.retire_terminal(task.id()),
                _ => {}
            }
        }
        Ok(())
    }
}

#[async_trait]
impl RunTerminalObserver for BackgroundTaskTerminalObserver {
    fn observer_id(&self) -> &str {
        OBSERVER_ID
    }

    async fn observe(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> Result<(), RunTerminalObserverError> {
        self.reconcile(terminal)
            .await
            .map_err(RunTerminalObserverError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_application_contract::SessionRunBackgroundApplication;
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_ext_background_task::{
        BackgroundInvocation, BackgroundTask, BackgroundTaskConfig, BackgroundTaskOrigin,
        BackgroundTaskPlugin, TaskExecutionPolicy, task_state_cell,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ToolDescriptor};
    use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshotId};
    use awaken_tool_contract::tool::{
        DurableTaskTool, RawTool, ToolCall, ToolConcurrency, ToolError, ToolOutputSpiller,
        ToolRecoveryPolicy, ToolResource, ToolResourceAccess,
    };

    #[derive(Clone, Debug, PartialEq)]
    struct AttentionCall {
        operation_id: String,
        thread: String,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
        input: Option<awaken_session_application_contract::PrivateTaskInput>,
    }

    struct RecordingAttentionApplication {
        transient_failures: AtomicUsize,
        definitive_rejection: bool,
        calls: Mutex<Vec<AttentionCall>>,
    }

    impl RecordingAttentionApplication {
        fn accepting(transient_failures: usize) -> Self {
            Self {
                transient_failures: AtomicUsize::new(transient_failures),
                definitive_rejection: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn rejecting() -> Self {
            Self {
                transient_failures: AtomicUsize::new(0),
                definitive_rejection: true,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl RecordingAttentionApplication {
        fn record(
            &self,
            call: AttentionCall,
        ) -> Result<(), awaken_runtime_contract::runs::RunError> {
            self.calls
                .lock()
                .expect("attention calls mutex poisoned")
                .push(call);
            if self.definitive_rejection {
                return Err(awaken_runtime_contract::runs::RunError::bad_request(
                    "definitive test rejection",
                ));
            }
            if self
                .transient_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(awaken_runtime_contract::runs::RunError::unavailable(
                    "transient test failure",
                ));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl SessionRunBackgroundApplication for RecordingAttentionApplication {
        async fn submit_session_run_background(
            &self,
            operation_id: &str,
            thread: &str,
            agent: Option<String>,
            messages: Vec<Message>,
            traceparent: Option<String>,
        ) -> Result<RunId, awaken_runtime_contract::runs::RunError> {
            self.record(AttentionCall {
                operation_id: operation_id.to_string(),
                thread: thread.to_string(),
                agent,
                messages,
                traceparent,
                input: None,
            })?;
            Ok(awaken_session_execution_contract::session_run_id(
                &awaken_session_types::SessionId::from(thread),
                operation_id,
            ))
        }

        async fn submit_task_attention(
            &self,
            attention: awaken_session_application_contract::TaskAttention,
        ) -> Result<
            awaken_session_application_contract::TaskAttentionAck,
            awaken_runtime_contract::runs::RunError,
        > {
            let attention = attention.bind_input().map_err(|error| {
                awaken_runtime_contract::runs::RunError::bad_request(error.to_string())
            })?;
            let run_id = awaken_session_execution_contract::session_run_id(
                &awaken_session_types::SessionId::from(attention.tenure.thread.as_str()),
                &attention.operation_id,
            );
            self.record(AttentionCall {
                operation_id: attention.operation_id,
                thread: attention.tenure.thread,
                agent: None,
                messages: vec![attention.message],
                traceparent: None,
                input: attention.input,
            })?;
            Ok(awaken_session_application_contract::TaskAttentionAck { run_id })
        }

        // This fixture records direct submission attempts, not durable acceptance.
        // A recorded call cannot fabricate an immutable Task receipt.
        async fn task_attention_receipt(
            &self,
            _: awaken_session_application_contract::TaskAttention,
        ) -> Result<
            Option<awaken_session_application_contract::TaskAttentionAck>,
            awaken_runtime_contract::runs::RunError,
        > {
            Err(awaken_runtime_contract::runs::RunError::unavailable(
                "Task attention receipt read is unavailable",
            ))
        }
    }

    #[test]
    fn attention_identity_and_system_input_are_fenced_and_minimal() {
        // Cause/effect decision table:
        // R1 same Thread+task+fence -> same operation/message identity;
        // R2 epoch changes -> both identities change, so a reclaimed attempt
        // cannot alias an old completion; R3 any terminal kind/arguments/result
        // -> payload contains only task id plus an instruction to read canonical
        // state, never completion content or invocation secrets.
        let thread = ThreadId("attention-thread".into());
        let task = BackgroundTaskId::new("attention-task").expect("task id");
        let first = awaken_lease_contract::fence::Tenure::granted(
            BackgroundTaskId::new("task").expect("task id"),
            "worker-secret".into(),
            awaken_lease_contract::fence::Epoch::from_persisted(1).unwrap(),
        );
        let second = awaken_lease_contract::fence::Tenure::granted(
            BackgroundTaskId::new("task").expect("task id"),
            "worker-secret".into(),
            awaken_lease_contract::fence::Epoch::from_persisted(2).unwrap(),
        );
        let (operation_a, message_a) =
            attention_message(&thread, &task, &first, &BackgroundAttention::Terminal);
        let (operation_retry, message_retry) =
            attention_message(&thread, &task, &first, &BackgroundAttention::Terminal);
        let (operation_b, message_b) =
            attention_message(&thread, &task, &second, &BackgroundAttention::Terminal);
        let (waiting_operation, _) =
            attention_message(&thread, &task, &first, &BackgroundAttention::Waiting);

        assert_eq!(operation_a, operation_retry, "R1 operation identity");
        assert_eq!(message_a, message_retry, "R1 message identity");
        assert_ne!(operation_a, operation_b, "R2 operation fence");
        assert_ne!(operation_a, waiting_operation, "R2 candidate kind");
        assert_ne!(message_a.id, message_b.id, "R2 message fence");
        assert_eq!(message_a.role, Role::System, "R3 system input");
        let text = message_a.text_content();
        assert!(text.contains(task.as_str()), "R3 task reference");
        assert!(
            !text.contains("get_background_task"),
            "R3 no redundant model lookup"
        );
        assert!(!text.contains("worker-secret"), "R3 no worker identity");
    }

    #[test]
    fn detached_task_start_failure_never_claims_a_remote_failure() {
        // Cause/effect decision table: R1 a Regular target returns a terminal
        // error from its default start adapter -> Failed; R2 a DetachedOnly
        // protocol target loses or rejects the start response -> Indeterminate,
        // because no durable remote task id crossed ThreadCommit and replay may
        // duplicate an external effect. Both rows preserve the original fence.
        let fence = awaken_lease_contract::fence::Tenure::granted(
            BackgroundTaskId::new("task").expect("task id"),
            "worker".into(),
            awaken_lease_contract::fence::Epoch::from_persisted(7).unwrap(),
        );
        let regular = BackgroundTaskTerminalObserver::start_failure(
            fence.clone(),
            ToolKind::Regular,
            "known local failure",
        );
        let detached = BackgroundTaskTerminalObserver::start_failure(
            fence.clone(),
            ToolKind::DetachedOnly,
            "response lost",
        );

        assert_eq!(regular.fence, fence, "R1 fence");
        assert!(
            matches!(regular.end, BackgroundTaskEnd::Failed { .. }),
            "R1"
        );
        assert_eq!(detached.fence, fence, "R2 fence");
        assert!(
            matches!(detached.end, BackgroundTaskEnd::Indeterminate { .. }),
            "R2"
        );
    }

    #[test]
    fn detached_state_invalidation_has_one_terminal_background_classification() {
        // Cause/effect decision table: C1 the detached start, poll, or cancel
        // crossed its dispatch boundary; C2 the executor then reports
        // StateInvalidated. SI1 C1+C2 => preserve the exact fence and publish
        // one Indeterminate candidate, never Failed/Completed. Both launch
        // branches call this single classifier; Runtime continuation tests own
        // the start/poll/cancel error mapping itself.
        let fence = awaken_lease_contract::fence::Tenure::granted(
            BackgroundTaskId::new("task").expect("task id"),
            "worker".into(),
            awaken_lease_contract::fence::Epoch::from_persisted(9).unwrap(),
        );

        let completion = BackgroundTaskTerminalObserver::indeterminate(
            fence.clone(),
            awaken_runtime::DetachedToolError::StateInvalidated.to_string(),
        );

        assert_eq!(completion.fence, fence, "SI1 exact claim fence");
        assert!(
            matches!(
                completion.end,
                BackgroundTaskEnd::Indeterminate { ref message }
                    if message.contains("invalidated after dispatch")
            ),
            "SI1 terminal classification"
        );
    }

    #[test]
    fn attention_carries_only_the_slot_candidate_of_its_exact_attempt() {
        // Decision table P1-P4 of `attention_input`.
        let task = BackgroundTaskId::new("task").expect("task id");
        let tenure = |epoch| {
            awaken_lease_contract::fence::Tenure::granted(
                task.clone(),
                "worker".into(),
                awaken_lease_contract::fence::Epoch::from_persisted(epoch).unwrap(),
            )
        };
        let wait = || {
            BackgroundWait::Remote(ToolTaskHandle {
                owner: "mcp".into(),
                binding: "generation".into(),
                task_id: "remote".into(),
                poll_interval_ms: None,
            })
        };
        let kind_of = |input: Option<PrivateTaskInput>| {
            input.map(|input| {
                let record = TaskObservationRecord::decode(&input.observations()[0]).unwrap();
                (
                    record.tenure().epoch().get(),
                    matches!(
                        record.observation(),
                        awaken_tool_contract::tool::background_task::TaskObservation::Ended { .. }
                    ),
                )
            })
        };
        let supervisor = BackgroundTaskSupervisor::new("worker", awaken_time::system_clock());
        // P4 vacant.
        assert_eq!(
            attention_input(
                &supervisor,
                &task,
                &tenure(1),
                &BackgroundAttention::Terminal
            ),
            None
        );
        supervisor.register(&task, &tenure(1)).unwrap();
        // P3 / P4 Active: nothing to deliver.
        for kind in [
            BackgroundAttention::InputRequired { change: "c".into() },
            BackgroundAttention::Waiting,
            BackgroundAttention::Terminal,
        ] {
            assert_eq!(attention_input(&supervisor, &task, &tenure(1), &kind), None);
        }
        supervisor.wait(
            task.clone(),
            BackgroundTaskWaitCandidate {
                fence: tenure(1),
                wait: wait(),
                renew_lease: true,
            },
        );
        // P2 Waiting and Watchdog; P4 another tenure; P3 still nothing.
        for kind in [
            BackgroundAttention::Waiting,
            BackgroundAttention::Watchdog { change: "w".into() },
        ] {
            assert_eq!(
                kind_of(attention_input(&supervisor, &task, &tenure(1), &kind)),
                Some((1, false))
            );
            assert_eq!(attention_input(&supervisor, &task, &tenure(2), &kind), None);
        }
        assert_eq!(
            attention_input(
                &supervisor,
                &task,
                &tenure(1),
                &BackgroundAttention::InputRequired { change: "c".into() }
            ),
            None
        );
        supervisor.complete(
            task.clone(),
            BackgroundTaskCompletion {
                fence: tenure(1),
                end: BackgroundTaskEnd::Cancelled {},
            },
        );
        // P1 Terminal; P4 another tenure.
        assert_eq!(
            kind_of(attention_input(
                &supervisor,
                &task,
                &tenure(1),
                &BackgroundAttention::Terminal
            )),
            Some((1, true))
        );
        assert_eq!(
            attention_input(
                &supervisor,
                &task,
                &tenure(2),
                &BackgroundAttention::Terminal
            ),
            None
        );
    }

    #[test]
    fn a_claimed_run_offers_its_task_input_only_when_every_record_decodes() {
        // Decision table C2/C4/C5 of `accept_claimed_task_input`.
        let task = BackgroundTaskId::new("task").expect("task id");
        let fence = awaken_lease_contract::fence::Tenure::granted(
            task.clone(),
            "remote-worker".into(),
            awaken_lease_contract::fence::Epoch::from_persisted(4).unwrap(),
        );
        let record = TaskObservationRecord::ended(&BackgroundTaskCompletion {
            fence: fence.clone(),
            end: BackgroundTaskEnd::Cancelled {},
        });
        let supervisor = BackgroundTaskSupervisor::new("local", awaken_time::system_clock());
        let tenure = awaken_session_application_contract::TaskTenureRead {
            thread: "thread".into(),
            partition: awaken_session_application_contract::TaskReadPartition::Thread {},
            run: "origin".into(),
            task: task.as_str().into(),
            holder: "remote-worker".into(),
            epoch: fence.epoch(),
        };
        let dispatch = |input| {
            let mut request = awaken_dispatch_contract::RunDispatch::new(
                awaken_runtime_contract::RunActivation::new(
                    RunId("attention-run".into()),
                    ThreadId("thread".into()),
                    snapshot(),
                    Vec::new(),
                ),
            );
            request.private_task_input = input;
            request
        };
        // C2
        accept_claimed_task_input(&supervisor, &dispatch(None)).expect("C2");
        assert!(supervisor.completion(&task).is_none(), "C2 nothing offered");
        // C5: one refused record refuses the whole input before any offer.
        let mut refused = record.encode();
        refused["version"] = serde_json::json!(2);
        let mixed =
            PrivateTaskInput::with_tenure(tenure.clone(), vec![record.encode(), refused]).unwrap();
        assert_eq!(
            accept_claimed_task_input(&supervisor, &dispatch(Some(mixed))),
            Err(TaskObservationCodecError::Version(2)),
            "C5"
        );
        assert!(supervisor.completion(&task).is_none(), "C5 nothing offered");
        // C3: retained legacy data is inspectable but carries no verified
        // remote/local subject. It cannot silently fill an authenticated tenure.
        let legacy = PrivateTaskInput::new(vec![record.encode()]).unwrap();
        assert!(
            accept_claimed_task_input(&supervisor, &dispatch(Some(legacy))).is_err(),
            "C3 unbound legacy refused"
        );
        assert!(supervisor.completion(&task).is_none(), "C3 no offer");
        // C6: actual dispatch partition and every decoded record must agree;
        // failures are atomic even when an earlier record is otherwise valid.
        let bound = PrivateTaskInput::with_tenure(tenure.clone(), vec![record.encode()]).unwrap();
        let mut foreign = dispatch(Some(bound.clone()));
        foreign.session_thread_id = Some(ThreadId("another-session".into()));
        assert!(
            accept_claimed_task_input(&supervisor, &foreign).is_err(),
            "C6 partition"
        );
        let mut other = record.encode();
        other["holder"] = serde_json::json!("other-holder");
        let mixed =
            PrivateTaskInput::with_tenure(tenure.clone(), vec![record.encode(), other]).unwrap();
        assert!(
            accept_claimed_task_input(&supervisor, &dispatch(Some(mixed))).is_err(),
            "C6 whole batch"
        );
        assert!(
            supervisor.completion(&task).is_none(),
            "C6 no partial offer"
        );
        // C7: explicitly bound meta-only input is a wake, never a completion.
        let meta = PrivateTaskInput::with_tenure(tenure.clone(), Vec::new()).unwrap();
        accept_claimed_task_input(&supervisor, &dispatch(Some(meta))).unwrap();
        assert!(supervisor.completion(&task).is_none(), "C7 zero offers");
        // C4
        let valid = PrivateTaskInput::with_tenure(tenure.clone(), vec![record.encode()]).unwrap();
        accept_claimed_task_input(&supervisor, &dispatch(Some(valid))).expect("C4");
        assert_eq!(
            supervisor
                .completion(&task)
                .map(|completion| completion.fence),
            Some(fence),
            "C4 the delivered candidate waits for the StepStart fold"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn attention_publication_retries_ambiguous_failures_with_one_identity_only() {
        // Cause/effect decision table:
        // R4 no application/expired role -> no publication and task truth stays
        // untouched; R5 unavailable/internal response -> retry the identical
        // Session command; R6 eventual acceptance -> stop after one successful
        // admission; R7 BadRequest -> stop immediately because rejection is
        // definitive. Exact operation identity makes response-loss retry safe.
        let thread = ThreadId("attention-thread".into());
        let task = BackgroundTaskId::new("attention-task").expect("task id");
        let fence = TaskTenure::granted(
            BackgroundTaskId::new("task").expect("task id"),
            "worker".into(),
            awaken_lease_contract::fence::Epoch::from_persisted(1).unwrap(),
        );

        let supervisor = BackgroundTaskSupervisor::new("worker", awaken_time::system_clock());
        publish_attention(
            None,
            &supervisor,
            &awaken_session_application_contract::TaskTenureRead {
                thread: thread.0.clone(),
                partition: awaken_session_application_contract::TaskReadPartition::Thread {},
                run: "origin".into(),
                task: task.as_str().into(),
                holder: fence.holder().clone(),
                epoch: fence.epoch(),
            },
            &task,
            &fence,
            BackgroundAttention::Terminal,
        )
        .await;

        let accepting = Arc::new(RecordingAttentionApplication::accepting(2));
        let accepting_port: Arc<dyn SessionRunBackgroundApplication> = accepting.clone();
        publish_attention(
            Some(crate::test_application_bindings::background_route(
                accepting_port.clone(),
            )),
            &supervisor,
            &awaken_session_application_contract::TaskTenureRead {
                thread: thread.0.clone(),
                partition: awaken_session_application_contract::TaskReadPartition::Thread {},
                run: "origin".into(),
                task: task.as_str().into(),
                holder: fence.holder().clone(),
                epoch: fence.epoch(),
            },
            &task,
            &fence,
            BackgroundAttention::Terminal,
        )
        .await;
        {
            let calls = accepting
                .calls
                .lock()
                .expect("attention calls mutex poisoned");
            assert_eq!(calls.len(), 3, "R5-R6 two retries then acceptance");
            assert!(calls.windows(2).all(|pair| pair[0] == pair[1]), "R5");
            assert!(calls.iter().all(|call| {
                call.thread == thread.0
                    && call.agent.is_none()
                    && call.traceparent.is_none()
                    && call.messages.len() == 1
                    && call.messages[0].role == Role::System
            }));
        }

        let rejecting = Arc::new(RecordingAttentionApplication::rejecting());
        let rejecting_port: Arc<dyn SessionRunBackgroundApplication> = rejecting.clone();
        publish_attention(
            Some(crate::test_application_bindings::background_route(
                rejecting_port.clone(),
            )),
            &supervisor,
            &awaken_session_application_contract::TaskTenureRead {
                thread: thread.0.clone(),
                partition: awaken_session_application_contract::TaskReadPartition::Thread {},
                run: "origin".into(),
                task: task.as_str().into(),
                holder: fence.holder().clone(),
                epoch: fence.epoch(),
            },
            &task,
            &fence,
            BackgroundAttention::Terminal,
        )
        .await;
        assert_eq!(
            rejecting
                .calls
                .lock()
                .expect("attention calls mutex poisoned")
                .len(),
            1,
            "R7 definitive rejection is not retried"
        );

        let exhausted = Arc::new(RecordingAttentionApplication::accepting(usize::MAX));
        let exhausted_port: Arc<dyn SessionRunBackgroundApplication> = exhausted.clone();
        publish_attention(
            Some(crate::test_application_bindings::background_route(
                exhausted_port.clone(),
            )),
            &supervisor,
            &awaken_session_application_contract::TaskTenureRead {
                thread: thread.0.clone(),
                partition: awaken_session_application_contract::TaskReadPartition::Thread {},
                run: "origin".into(),
                task: task.as_str().into(),
                holder: fence.holder().clone(),
                epoch: fence.epoch(),
            },
            &task,
            &fence,
            BackgroundAttention::Terminal,
        )
        .await;
        assert_eq!(
            exhausted
                .calls
                .lock()
                .expect("attention calls mutex poisoned")
                .len(),
            ATTENTION_RETRY_DELAYS.len() + 1,
            "R5 bounded retries cannot retain Session Environment forever"
        );
    }

    struct RecordingDelivery {
        transient_failures: AtomicUsize,
        definitive_rejection: bool,
        attentions: Mutex<Vec<TaskAttention>>,
    }

    impl RecordingDelivery {
        fn new(transient_failures: usize, definitive_rejection: bool) -> Self {
            Self {
                transient_failures: AtomicUsize::new(transient_failures),
                definitive_rejection,
                attentions: Mutex::new(Vec::new()),
            }
        }

        fn sent(&self) -> Vec<TaskAttention> {
            self.attentions
                .lock()
                .expect("delivered attentions mutex poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl awaken_session_application_contract::RegisteredTaskControl for RecordingDelivery {
        fn holder(&self) -> &str {
            "registered-holder"
        }

        async fn deliver(
            &self,
            attention: TaskAttention,
        ) -> Result<
            awaken_session_application_contract::TaskAttentionAck,
            awaken_runtime_contract::runs::RunError,
        > {
            self.attentions
                .lock()
                .expect("delivered attentions mutex poisoned")
                .push(attention.clone());
            if self.definitive_rejection {
                return Err(awaken_runtime_contract::runs::RunError::bad_request(
                    "the Task attention names a Task this Worker incarnation does not hold",
                ));
            }
            if self
                .transient_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(awaken_runtime_contract::runs::RunError::unavailable(
                    "Coordinator unavailable",
                ));
            }
            Ok(awaken_session_application_contract::TaskAttentionAck {
                run_id: RunId(format!("attention-run-{}", attention.operation_id)),
            })
        }

        async fn read_tenure(
            &self,
            _read: awaken_session_application_contract::TaskTenureRead,
        ) -> Result<
            Option<awaken_session_application_contract::TaskTenureRecord>,
            awaken_runtime_contract::runs::RunError,
        > {
            panic!("attention publication reads no Task tenure")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_registered_worker_publishes_attention_through_its_delivery_with_one_identity() {
        // W3 with K1/K2/K4/K5 at the publisher: a retryable failure resends the
        // identical attention (same operation id, holder and record); the
        // acknowledgement stops; a definitive refusal stops at once.
        let thread = ThreadId("registered-thread".into());
        let task = BackgroundTaskId::new("registered-task").expect("task id");
        let fence = TaskTenure::granted(
            BackgroundTaskId::new("registered-task").expect("task id"),
            "registered-holder".into(),
            awaken_lease_contract::fence::Epoch::from_persisted(1).unwrap(),
        );
        let supervisor =
            BackgroundTaskSupervisor::new("registered-holder", awaken_time::system_clock());

        let accepting = Arc::new(RecordingDelivery::new(2, false));
        publish_attention(
            Some(TaskAttentionRoute::Registered(accepting.clone())),
            &supervisor,
            &awaken_session_application_contract::TaskTenureRead {
                thread: thread.0.clone(),
                partition: awaken_session_application_contract::TaskReadPartition::Thread {},
                run: "origin".into(),
                task: task.as_str().into(),
                holder: fence.holder().clone(),
                epoch: fence.epoch(),
            },
            &task,
            &fence,
            BackgroundAttention::Terminal,
        )
        .await;
        let sent = accepting.sent();
        assert_eq!(sent.len(), 3, "K5 two retries, then K1");
        assert!(sent.windows(2).all(|pair| pair[0] == pair[1]), "K2");
        assert_eq!(sent[0].tenure.holder, "registered-holder", "W3 holder");
        assert_eq!(sent[0].tenure.thread, thread.0, "W3 Thread");

        let rejecting = Arc::new(RecordingDelivery::new(0, true));
        publish_attention(
            Some(TaskAttentionRoute::Registered(rejecting.clone())),
            &supervisor,
            &awaken_session_application_contract::TaskTenureRead {
                thread: thread.0.clone(),
                partition: awaken_session_application_contract::TaskReadPartition::Thread {},
                run: "origin".into(),
                task: task.as_str().into(),
                holder: fence.holder().clone(),
                epoch: fence.epoch(),
            },
            &task,
            &fence,
            BackgroundAttention::Terminal,
        )
        .await;
        assert_eq!(rejecting.sent().len(), 1, "K4 a refusal is not retried");
    }

    #[tokio::test]
    async fn session_background_application_composition_is_single_assignment_and_weak() {
        // Cause/effect decision table: R8 vacant installed Session + application -> one weak
        // executable edge; R9 occupied installed Session + another install -> reject without
        // replacement; R10 all external strong owners drop -> ExecutorComponents does
        // not retain the Session application. This prevents both a second Run
        // admission owner and an Arc ownership cycle.
        // Startup connection rules: R11 unbound => typed Unbound, expired role =>
        // typed RequiredApplicationUnavailable;
        // R12 live binding => readiness observed through every shared clone.
        // Naming the shared cell `value` retains those same single-assignment
        // and owner-release rules; no second cell is introduced.
        // Neither observation admits a Run or replaces its domain authority.
        let host = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
        )
        .await)
            .build()
            .expect("validated Host dependencies");
        let managed = host.clone();
        let shared = host.session_application_binding().clone();
        assert_eq!(
            shared.background_runs().err(),
            Some(awaken_session_execution_contract::SessionApplicationBindingError::Unbound),
            "R11 unbound",
        );
        let application = Arc::new(RecordingAttentionApplication::accepting(0));
        let port: Arc<dyn SessionRunBackgroundApplication> = application.clone();
        crate::test_application_bindings::select_background(&managed, port.clone())
            .expect("R8 first install");
        assert!(shared.background_runs().is_ok(), "R12 shared binding");
        assert_eq!(
            crate::test_application_bindings::select_background(&managed, port.clone()),
            Err(awaken_session_execution_contract::SessionApplicationBindingError::AlreadyBound),
            "R9 single assignment",
        );
        drop(application);
        drop(port);
        assert!(
            host.session_application.background_runs().is_err(),
            "R10 weak ownership edge"
        );
        assert_eq!(
            shared.background_runs().err(),
            Some(
                awaken_session_execution_contract::SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "background Run submission",
                )
            ),
            "R11 bound target expired",
        );
    }

    fn write_claim(resource: &str) -> ToolConcurrency {
        ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
            "sandbox", resource,
        ))])
    }

    #[tokio::test]
    async fn admission_check_and_claim_publication_are_one_atomic_transition() {
        // Cause/effect decision table:
        // R1 no active claim + write(A) -> admit G1;
        // R2 G1 active + write(A) -> conflicting G2 remains blocked;
        // R3 G1 active + write(B) -> compatible G3 is admitted concurrently;
        // R4 G1 released -> G2 is admitted exactly once.
        // Constraint: compatibility check and active-map insertion are one
        // linearization point; otherwise two R1 arrivals can both observe an
        // empty set and violate R2.
        let background = BackgroundRuns::new();
        let admission = background
            .tool_execution_admission("session", "generation")
            .expect("healthy registry");
        let first = admission.clone().acquire(write_claim("a")).await.unwrap();

        let mut conflicting = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.acquire(write_claim("a")).await.unwrap() })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut conflicting)
                .await
                .is_err(),
            "R2: a conflicting claim must not pass the active owner"
        );

        let compatible = admission.clone().acquire(write_claim("b")).await.unwrap();
        drop(compatible);
        assert!(!conflicting.is_finished(), "R3 preserves the R2 conflict");

        drop(first);
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), conflicting)
            .await
            .expect("R4: release wakes the conflicting waiter")
            .expect("R4: waiter task does not panic");
        drop(second);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_conflicting_arrivals_never_overlap() {
        // Race rule R5: N write(A) arrivals are released together while the
        // active set is empty. Effect: every arrival eventually enters, but the
        // observed maximum active critical sections is exactly one. This is the
        // regression case for the former check-unlock-insert TOCTOU; the
        // sequential-owner test above covers blocking and wake behavior.
        let background = BackgroundRuns::new();
        let admission = background
            .tool_execution_admission("session", "generation")
            .expect("healthy registry");
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut arrivals = Vec::new();
        for _ in 0..16 {
            let admission = admission.clone();
            let barrier = barrier.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            arrivals.push(tokio::spawn(async move {
                barrier.wait().await;
                let guard = admission.acquire(write_claim("same")).await.unwrap();
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }));
        }
        barrier.wait().await;
        for arrival in arrivals {
            arrival.await.expect("R5 arrival task");
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 1, "R5");
    }

    struct CountingTool(AtomicUsize);

    #[async_trait]
    impl RawTool for CountingTool {
        fn id(&self) -> &str {
            "count"
        }

        async fn invoke(
            &self,
            call: ToolCall,
        ) -> Result<ToolOutput, awaken_tool_contract::tool::ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::ok(call.call_id, "done"))
        }
    }

    struct SpillProbe(Arc<Mutex<Vec<(String, String, String)>>>);

    #[async_trait]
    impl ToolOutputSpiller for SpillProbe {
        async fn spill(
            &self,
            run_id: &RunId,
            call_id: &str,
            content: String,
        ) -> Result<String, ToolError> {
            self.0.lock().expect("spill probe mutex poisoned").push((
                run_id.0.clone(),
                call_id.to_string(),
                content,
            ));
            Ok(format!("artifact://{}/{call_id}", run_id.0))
        }
    }

    struct RecordingAfterTool(Arc<AtomicUsize>);

    #[async_trait]
    impl awaken_runtime_contract::plugin::PhaseHook for RecordingAfterTool {
        fn point(&self) -> awaken_runtime_contract::plugin::PhaseHookPoint {
            awaken_runtime_contract::plugin::PhaseHookPoint::AfterTool
        }

        async fn on_phase(
            &self,
            _ctx: &awaken_runtime_contract::plugin::PhaseContext,
            _conversation: &[awaken_agent_contract::agent::message::Message],
            _state: &Store,
        ) -> awaken_runtime_contract::execution::Result<awaken_runtime_contract::plugin::HookReaction>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(awaken_runtime_contract::plugin::HookReaction::default())
        }
    }

    #[derive(Clone)]
    pub(super) struct RecordingAfterToolPlugin(
        pub(super) Arc<AtomicUsize>,
        pub(super) ToolConcurrency,
    );

    impl awaken_runtime_contract::plugin::ToolConcurrencyConstraint for RecordingAfterToolPlugin {
        fn id(&self) -> &str {
            "test.after-tool"
        }
        fn constrain(&self, _call: &awaken_tool_contract::tool::ToolCall) -> ToolConcurrency {
            self.1.clone()
        }
    }

    impl awaken_runtime_contract::plugin::Plugin for RecordingAfterToolPlugin {
        fn manifest(&self) -> awaken_runtime_contract::plugin::PluginManifest {
            awaken_runtime_contract::plugin::PluginManifest {
                id: "test.after-tool".into(),
                bound: awaken_runtime_contract::plugin::CapabilityBound {
                    phase_hooks: vec![awaken_runtime_contract::plugin::PhaseHookPoint::AfterTool],
                    tool_constraints: awaken_runtime_contract::plugin::IdBound::Exact(vec![
                        "test.after-tool".into(),
                    ]),
                    ..Default::default()
                },
                ..Default::default()
            }
        }

        fn resolve(&self) -> awaken_runtime_contract::plugin::Contributions {
            let mut contributions =
                awaken_runtime_contract::plugin::Contributions::new("test.after-tool");
            contributions
                .phase_hooks
                .push(Arc::new(RecordingAfterTool(self.0.clone())));
            contributions.register_tool_constraint(Arc::new(self.clone()));
            contributions
        }
    }

    fn snapshot() -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("background-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("background-agent".into()),
            resolved_spec: awaken_runtime_contract::ResolvedSpec {
                catalog_fingerprint: CatalogFingerprint("background-catalog".into()),
                instructions: String::new(),
                max_steps: Some(1),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "native"),
                ),
                model_candidates: Vec::new(),
                tool_descriptors: vec![ToolDescriptor::pinned(
                    "test",
                    "count",
                    "Count once.",
                    serde_json::json!({"type":"object"}),
                )],
                plugin_ids: vec![awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.into()],
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("background-catalog".into()),
        }
    }

    #[tokio::test]
    async fn a_parent_created_child_launches_once_and_its_claimed_result_folds_in_the_same_partition()
     {
        // C1 the child factory and original DispatchWorker share the parent's
        // real commit object, supervisor, lifetime and exact child snapshot;
        // C2 the origin is already committed Ended; C3 duplicate delivery;
        // C4 the foreground claim settles before result folding on another
        // supervisor. E1 exactly one ordinary tool effect; E2 one private
        // attention names child+parent+origin+holder+epoch; E3 only that carrying
        // dispatch offers it; E4 ordinary StepStart folds once into the same
        // parent's committed store. No scripted Task read or extra Task store.
        use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView as _;
        use awaken_dispatch_contract::DispatchEnqueue as _;
        use awaken_worker_contract::PlacementRequirements;
        let clock = awaken_time::system_clock();
        let supervisor = Arc::new(BackgroundTaskSupervisor::new("child-holder", clock.clone()));
        let counter = Arc::new(CountingTool(AtomicUsize::new(0)));
        let config = snapshot();
        let runtime = Arc::new(
            Runtime::new()
                .with_tool(counter.clone())
                .with_plugin(Arc::new(BackgroundTaskPlugin::with_supervisor(
                    BackgroundTaskConfig {
                        tools: BTreeSet::from(["count".into()]),
                        ..Default::default()
                    },
                    supervisor.clone(),
                ))),
        );
        let memory = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
            clock.clone(),
        ));
        let commit = Arc::new(SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected_shared(memory.clone()),
        )));
        let queue = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
        let background = Arc::new(BackgroundRuns::new());
        let application = Arc::new(RecordingAttentionApplication::accepting(0));
        let application_port: Arc<dyn SessionRunBackgroundApplication> = application.clone();
        let mut task = BackgroundTask::requested(
            BackgroundTaskId::new("child-task").unwrap(),
            BackgroundTaskOrigin {
                thread_id: ThreadId("child".into()),
                run_id: RunId("child-origin".into()),
                operation_id: "operation".into(),
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id: "call".into(),
                    tool_id: "count".into(),
                    arguments: serde_json::json!({}),
                },
            },
        );
        task.start(
            supervisor.worker_id(),
            supervisor.clock().now_ms(),
            BackgroundTaskSupervisor::lease_ms(),
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .unwrap();
        super::ownership::tests::persist(&commit, &task, "child-origin").await;
        let adapters = crate::agent_runner::ChildExecutionAdapters {
            session_thread_id: Some(ThreadId("parent".into())),
            background_supervisor: Some(supervisor.clone()),
            background_runs: Some(background.clone()),
            background_attention: Some(crate::test_application_bindings::background_route(
                application_port,
            )),
            background_generation: Some("generation".into()),
            background_commit: Some(commit.clone()),
            ..Default::default()
        };
        let (context, reads) = crate::agent_runner::ChildWorkerFactory::background_context(
            runtime.clone(),
            &config,
            Some(&commit),
            RuntimeRunContext::new(),
            &adapters,
        )
        .unwrap();
        let scheduler = crate::agent_runner::RunScheduler {
            recovery_history: None,
            clock: clock.clone(),
            // Erase the SAME selected queue; enqueue/claim/asserts below keep
            // their original Memory handle and its original injected Clock.
            binding: awaken_dispatch_erasure::DispatchBinding::local(Arc::new(
                awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(
                    queue.clone(),
                ),
            )),
            commit: commit.clone(),
            reader: memory.clone(),
            owner: "child-holder".into(),
            session_resources: None,
            publication_source: None,
            claimed_model_request_gate: None,
        };
        let executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
            runtime.clone();
        let (worker, _) = crate::agent_runner::ChildWorkerFactory::parent_execution(
            runtime,
            &scheduler,
            memory.clone(),
            context,
            Default::default(),
            executor,
            reads,
        );
        let activation = awaken_runtime_contract::RunActivation::new(
            RunId("child-origin".into()),
            ThreadId("child".into()),
            config.clone(),
            Vec::new(),
        );
        let request = awaken_dispatch_contract::RunDispatch::new(activation)
            .for_session(ThreadId("parent".into()))
            .with_placement(PlacementRequirements::remote_preferred())
            .with_worker_codec_requirements();
        queue.enqueue(request).await.unwrap();
        worker
            .tick_run(&RunId("child-origin".into()), clock.clone())
            .await
            .unwrap();
        assert!(background.drain(Duration::from_secs(2)).await);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "E1");
        let carried = {
            let calls = application.calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "E2");
            let carried = calls[0].input.clone().unwrap();
            assert_eq!(
                carried.tenure().unwrap().partition,
                awaken_session_application_contract::TaskReadPartition::Session {
                    session_thread: "parent".into()
                }
            );
            carried
        };
        let receiver = Arc::new(BackgroundTaskSupervisor::new("receiver", clock));
        let mut delivery = awaken_dispatch_contract::RunDispatch::new(
            awaken_runtime_contract::RunActivation::new(
                RunId("attention".into()),
                ThreadId("child".into()),
                config,
                Vec::new(),
            ),
        )
        .for_session(ThreadId("parent".into()));
        delivery.private_task_input = Some(carried);
        accept_claimed_task_input(&receiver, &delivery).unwrap();
        // The original extension hook is the sole fold owner, on a different
        // supervisor from the tool holder; it does not inherit the old claim.
        let plugin = BackgroundTaskPlugin::with_supervisor(
            BackgroundTaskConfig {
                tools: BTreeSet::from(["count".into()]),
                ..Default::default()
            },
            receiver.clone(),
        );
        use awaken_runtime_contract::plugin::{
            PhaseContext, PhaseHookPoint, PhaseKind, Plugin as _, ResolvedExecutionEnv,
        };
        let env = ResolvedExecutionEnv::merge(vec![(plugin.manifest(), plugin.resolve())]).unwrap();
        let hook = env
            .hooks_for(PhaseHookPoint::StepStart)
            .into_iter()
            .next()
            .unwrap();
        let state = Store::rebuild(&memory.committed_state(&ThreadId("child".into())));
        let phase = PhaseContext {
            run_id: RunId("attention".into()),
            step: 0,
            kind: PhaseKind::StepStart,
        };
        let reaction = hook.on_phase(&phase, &[], &state).await.unwrap();
        commit
            .commit(ThreadCommit::assemble(
                ThreadId("child".into()),
                RunDisposition::ended(
                    RunId("attention".into()),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                reaction.messages,
                reaction.state,
                Vec::new(),
            ))
            .await
            .unwrap();
        let after = Store::rebuild(&memory.committed_state(&ThreadId("child".into())));
        assert!(
            task_state_cell(task.id())
                .load(&after)
                .unwrap()
                .unwrap()
                .lifecycle()
                .is_terminal(),
            "E4 original StepStart persisted"
        );
        let replay = hook.on_phase(&phase, &[], &after).await.unwrap();
        assert!(replay.state.is_empty(), "E4 no duplicate fold/notification");
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "E1 no repeated tool");
    }

    #[tokio::test]
    async fn committed_claim_launches_once_and_records_completion_without_a_second_store() {
        // Cause/effect table: C1 authoritative State and the process projection
        // are empty -> E1 observer launches nothing; C2 authoritative State has
        // the exact Running claim while the process projection remains empty ->
        // E2 observer launches that claim; C3 duplicate terminal deliveries ->
        // E3 supervisor registration admits one tool effect and one completion;
        // C4 another plugin owns an AfterTool workflow hook -> E4 detached target
        // execution never advances that foreground-only workflow event;
        // C5 the canonical output spiller is bound -> E5 completion retains only
        // its stable materialized result reference, not a parallel output store;
        // C6 a Session background Run port is installed -> E6 one same-Thread
        // System attention command is submitted after process completion.
        // Constraint: the scripted policy replaces only the existing
        // LocalCommitQueries read seam; the Memory store remains the observable
        // stale compatibility projection. Rules: R1=C1=>E1; R2=C2=>E2;
        // R3=C2+C3=>E3; R4=C2+C4=>E4; R5=C2+C5=>E5; R6=C2+C6=>E6.
        // C7 a plugin adds Write(shared) to the intrinsic Parallel target.
        // R7 the original frozen Task policy matches Prepared facts, so R2-R6
        // still execute exactly once; constraints never become State authority.
        let claim = ToolConcurrency::Resources(vec![
            awaken_tool_contract::tool::ToolResourceAccess::Write(
                awaken_tool_contract::tool::ToolResource::new("plugin-test", "shared"),
            ),
        ]);
        let supervisor = Arc::new(BackgroundTaskSupervisor::new(
            "worker-test",
            awaken_time::system_clock(),
        ));
        let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
            BackgroundTaskConfig {
                tools: BTreeSet::from(["count".into()]),
                ..Default::default()
            },
            supervisor.clone(),
        ));
        let counter = Arc::new(CountingTool(AtomicUsize::new(0)));
        let spills = Arc::new(Mutex::new(Vec::new()));
        let after_tool_calls = Arc::new(AtomicUsize::new(0));
        let runtime = Arc::new(
            Runtime::new()
                .with_plugin(plugin)
                .with_plugin(Arc::new(RecordingAfterToolPlugin(
                    after_tool_calls.clone(),
                    claim.clone(),
                )))
                .with_tool(counter.clone()),
        );
        let process_projection = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
            awaken_time::system_clock(),
        ));
        let authoritative_state = crate::authority::ScriptedStateQueries::new(Ok(Vec::new()));
        let commit = Arc::new(SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::with_queries(
                process_projection.clone(),
                authoritative_state.clone(),
            ),
        )));
        let background = Arc::new(BackgroundRuns::new());
        let attention = Arc::new(RecordingAttentionApplication::accepting(0));
        let attention_port: Arc<dyn SessionRunBackgroundApplication> = attention.clone();
        let mut snapshot = snapshot();
        snapshot
            .resolved_spec
            .plugin_ids
            .push("test.after-tool".into());
        let observer = BackgroundTaskTerminalObserver::new(
            runtime,
            snapshot,
            RuntimeRunContext::new().with_tool_output_spiller(Arc::new(SpillProbe(spills.clone()))),
            commit
                .committed_authority()
                .expect("local test commit is authoritative")
                .into(),
            background.clone(),
            supervisor.clone(),
            "session".into(),
            "generation".into(),
            Some(crate::test_application_bindings::background_route(
                attention_port.clone(),
            )),
        );
        let thread_id = ThreadId("thread".into());
        let origin_run = RunId("origin".into());
        let terminal = CommittedTerminalRun {
            run_id: origin_run.clone(),
            thread_id: thread_id.clone(),
            cause: awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        };
        observer.observe(&terminal).await.expect("R1 observer");
        assert_eq!(counter.0.load(Ordering::SeqCst), 0, "R1/E1");

        let mut task = BackgroundTask::requested(
            BackgroundTaskId::new("task-test").expect("task id"),
            BackgroundTaskOrigin {
                thread_id: thread_id.clone(),
                run_id: origin_run.clone(),
                operation_id: "operation".into(),
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id: "call".into(),
                    tool_id: "count".into(),
                    arguments: serde_json::json!({}),
                },
            },
        );
        let fence = task
            .start(
                supervisor.worker_id(),
                supervisor.clock().now_ms(),
                BackgroundTaskSupervisor::lease_ms(),
                TaskExecutionPolicy {
                    recovery: ToolRecoveryPolicy::default(),
                    concurrency: claim,
                },
            )
            .expect("claim");
        authoritative_state.replace(Ok(vec![
            task_state_cell(task.id())
                .write(&task)
                .expect("R2 authoritative task command"),
        ]));
        assert!(
            awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::committed_state(
                process_projection.as_ref(),
                &thread_id,
            )
            .is_empty(),
            "R2 process compatibility projection must remain stale"
        );
        observer.observe(&terminal).await.expect("first delivery");
        observer
            .observe(&terminal)
            .await
            .expect("duplicate delivery");
        assert!(background.drain(std::time::Duration::from_secs(2)).await);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "R2/R3 E2/E3");
        let attention_calls = attention
            .calls
            .lock()
            .expect("attention calls mutex poisoned");
        assert_eq!(attention_calls.len(), 1, "R3/R6 E3/E6 one attention Run");
        assert_eq!(attention_calls[0].thread, thread_id.0, "R6/E6 owner");
        assert_eq!(attention_calls[0].messages[0].role, Role::System, "R6/E6");
        let carried = attention_calls[0]
            .input
            .as_ref()
            .expect("P1 the terminal attention carries its observation");
        let record = TaskObservationRecord::decode(&carried.observations()[0])
            .expect("P1 the carried record decodes");
        assert_eq!(record.tenure(), fence, "P1 exact attempt tenure");
        assert!(
            matches!(
                record.observation(),
                awaken_tool_contract::tool::background_task::TaskObservation::Ended { .. }
            ),
            "P1 Ended record"
        );
        drop(attention_calls);
        assert_eq!(after_tool_calls.load(Ordering::SeqCst), 0, "R4/E4");
        let completion = supervisor.completion(task.id()).expect("R3/E3 completion");
        assert_eq!(completion.fence, fence);
        assert!(matches!(
            &completion.end,
            BackgroundTaskEnd::Completed { content, is_error: false }
                if content == &vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "artifact://background-task-test/call"
                )]
        ));
        assert_eq!(
            spills
                .lock()
                .expect("spill probe mutex poisoned")
                .as_slice(),
            &[(
                "background-task-test".to_string(),
                "call".to_string(),
                "done".to_string(),
            )],
            "R5/E5: detached execution reuses the canonical stable spill identity"
        );
        assert!(
            supervisor.register(task.id(), &fence).is_none(),
            "completion remains a deduplication guard until durable Ended truth"
        );
    }

    #[derive(Default)]
    pub(super) struct RemoteTaskTool {
        pub(super) starts: AtomicUsize,
        pub(super) invokes: AtomicUsize,
        pub(super) ordinary_invoke: bool,
        pub(super) polls: AtomicUsize,
        pub(super) cancels: AtomicUsize,
        pub(super) effect_gate: Option<Arc<tokio::sync::Semaphore>>,
        pub(super) effect_entered: tokio::sync::Notify,
        pub(super) uncertain_action: Option<usize>,
        pub(super) malformed_created_handle: bool,
    }

    impl RemoteTaskTool {
        fn observed(&self, action: usize) -> Result<(), ToolError> {
            if self.uncertain_action == Some(action) {
                Err(ToolError::Indeterminate(
                    "remote observation unknown".into(),
                ))
            } else {
                Ok(())
            }
        }
        async fn before_effect(&self) {
            if let Some(gate) = &self.effect_gate {
                self.effect_entered.notify_one();
                gate.acquire().await.unwrap().forget();
            }
        }
        pub(super) fn handle() -> ToolTaskHandle {
            ToolTaskHandle {
                owner: "fixture-durable-adapter".into(),
                binding: "remote".into(),
                task_id: "remote-secret-id".into(),
                poll_interval_ms: Some(1),
            }
        }
    }

    #[async_trait]
    impl RawTool for RemoteTaskTool {
        fn id(&self) -> &str {
            "remote"
        }

        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            assert!(
                self.ordinary_invoke,
                "durable detached target must use start_task"
            );
            self.before_effect().await;
            self.invokes.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::ok(call.call_id, "ordinary complete"))
        }

        fn durable_tasks(&self) -> Option<&dyn DurableTaskTool> {
            Some(self)
        }
    }

    #[async_trait]
    impl DurableTaskTool for RemoteTaskTool {
        async fn start_task(&self, _call: ToolCall) -> Result<ToolTaskStart, ToolError> {
            self.before_effect().await;
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.observed(0)?;
            let mut handle = Self::handle();
            if self.malformed_created_handle {
                handle.owner.clear();
            }
            Ok(ToolTaskStart::Pending(handle))
        }

        async fn poll_task(
            &self,
            call: &ToolCall,
            task: &ToolTaskHandle,
        ) -> Result<ToolTaskPoll, ToolError> {
            assert_eq!(task.task_id, "remote-secret-id");
            self.before_effect().await;
            self.polls.fetch_add(1, Ordering::SeqCst);
            self.observed(1)?;
            Ok(ToolTaskPoll::Completed(ToolOutput::ok(
                &call.call_id,
                "remote complete",
            )))
        }

        async fn cancel_task(
            &self,
            _call: &ToolCall,
            task: &ToolTaskHandle,
        ) -> Result<ToolTaskPoll, ToolError> {
            assert_eq!(task.task_id, Self::handle().task_id);
            self.before_effect().await;
            self.cancels.fetch_add(1, Ordering::SeqCst);
            self.observed(2)?;
            Ok(ToolTaskPoll::Cancelled)
        }
    }

    pub(super) fn remote_snapshot() -> ExecutableAgentSnapshot {
        let mut snapshot = snapshot();
        snapshot.resolved_spec.tool_descriptors = vec![
            ToolDescriptor::pinned(
                "test",
                "remote",
                "Remote work.",
                serde_json::json!({"type":"object"}),
            )
            .with_kind(awaken_runtime_contract::resolved::ToolKind::DetachedOnly),
        ];
        snapshot
    }

    #[tokio::test]
    async fn committed_remote_handle_is_polled_without_replaying_start() {
        // Cause/effect decision table: R1 committed Running -> exactly one
        // start_task and a process Wait candidate; R2 candidate is committed as
        // Waiting -> the same supervisor installed Session resumes one poll by the saved
        // handle; R3 an arbitrary non-MCP adapter owner is carried opaquely;
        // R4 explicit Completed -> terminal candidate and a distinct attention
        // identity. Constraints: no second start, no adapter/remote id in
        // System messages, and Thread State remains the only durable truth.
        let supervisor = Arc::new(BackgroundTaskSupervisor::new(
            "worker-remote",
            awaken_time::system_clock(),
        ));
        let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
            BackgroundTaskConfig {
                tools: BTreeSet::from(["remote".into()]),
                ..Default::default()
            },
            supervisor.clone(),
        ));
        let remote = Arc::new(RemoteTaskTool::default());
        let runtime = Arc::new(Runtime::new().with_plugin(plugin).with_tool(remote.clone()));
        let memory = awaken_store_inmem::MemoryCommitCoordinator::new(awaken_time::system_clock());
        let commit = Arc::new(SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(memory),
        )));
        let background = Arc::new(BackgroundRuns::new());
        let attention = Arc::new(RecordingAttentionApplication::accepting(0));
        let attention_port: Arc<dyn SessionRunBackgroundApplication> = attention.clone();
        let observer = BackgroundTaskTerminalObserver::new(
            runtime,
            remote_snapshot(),
            RuntimeRunContext::new(),
            commit
                .committed_authority()
                .expect("local test commit is authoritative")
                .into(),
            background.clone(),
            supervisor.clone(),
            "session".into(),
            "generation".into(),
            Some(crate::test_application_bindings::background_route(
                attention_port.clone(),
            )),
        );
        let thread_id = ThreadId("remote-thread".into());
        let mut task = BackgroundTask::requested(
            BackgroundTaskId::new("remote-task").expect("task id"),
            BackgroundTaskOrigin {
                thread_id: thread_id.clone(),
                run_id: RunId("origin".into()),
                operation_id: "operation".into(),
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id: "remote-call".into(),
                    tool_id: "remote".into(),
                    arguments: serde_json::json!({}),
                },
            },
        );
        let fence = task
            .start(
                supervisor.worker_id(),
                supervisor.clock().now_ms(),
                BackgroundTaskSupervisor::lease_ms(),
                TaskExecutionPolicy {
                    recovery: ToolRecoveryPolicy::default(),
                    concurrency: ToolConcurrency::Parallel,
                },
            )
            .expect("claim");
        let commit_task = |run: &str, task: &BackgroundTask| {
            ThreadCommit::assemble(
                thread_id.clone(),
                RunDisposition::ended(
                    RunId(run.into()),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                vec![task_state_cell(task.id()).write(task).expect("task state")],
                Vec::new(),
            )
        };
        commit
            .commit(commit_task("origin", &task))
            .await
            .expect("R1 commit");
        observer
            .observe(&CommittedTerminalRun {
                run_id: RunId("origin".into()),
                thread_id: thread_id.clone(),
                cause: awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            })
            .await
            .expect("R1 observe");
        assert!(background.drain(Duration::from_secs(2)).await);
        assert_eq!(remote.starts.load(Ordering::SeqCst), 1, "R1");
        let waiting = supervisor
            .wait_candidate(task.id())
            .expect("R1 wait candidate");
        task.wait(&waiting.fence, waiting.wait)
            .expect("R2 fold wait");
        commit
            .commit(commit_task("wait-attention", &task))
            .await
            .expect("R2 commit");
        observer
            .observe(&CommittedTerminalRun {
                run_id: RunId("wait-attention".into()),
                thread_id: thread_id.clone(),
                cause: awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            })
            .await
            .expect("R2 observe");
        assert!(background.drain(Duration::from_secs(2)).await);
        assert_eq!(remote.starts.load(Ordering::SeqCst), 1, "R2 no replay");
        assert_eq!(remote.polls.load(Ordering::SeqCst), 1, "R2 one poll");
        assert!(
            matches!(
                supervisor
                    .completion(task.id())
                    .map(|completion| completion.end),
                Some(BackgroundTaskEnd::Completed { .. })
            ),
            "R3"
        );
        let attention = attention.calls.lock().expect("attention calls");
        assert_eq!(attention.len(), 2, "R1 waiting + R3 terminal");
        assert_ne!(attention[0].operation_id, attention[1].operation_id, "R3");
        assert!(
            attention
                .iter()
                .all(|call| { !call.messages[0].text_content().contains("remote-secret-id") }),
            "remote handle never reaches Agent"
        );
        assert_eq!(fence.epoch().get(), 1);
    }

    #[tokio::test]
    async fn process_tool_backing_preserves_the_prepared_catalog_and_run_narrowing() {
        // The existing Host Runtime factory registers process-selected backing;
        // it does not grant execution. This PreparedToolExecutor table isolates
        // that seam; the following cold/warm case owns real claimed Task delivery.
        // | Rule | frozen count descriptor | explicit Agent allow | Run ceiling | effect |
        // | S1 | present | yes | Configured | original default/Parallel facts, one effect |
        // | S2 | absent | yes | Configured | NotExecutable, zero effects |
        // | S3 | present | yes | DenyAll | facts still resolve, NotAuthorized, zero effects |
        // No model, fake permission gate, duplicated registry or Task state owner
        // is supplied. The child factory's extra_allowed input remains empty.
        use awaken_runtime_contract::permission::ToolCapabilityNarrowing;
        for (rule, declared, narrowing, expected) in [
            ("S1", true, ToolCapabilityNarrowing::Configured, 1),
            ("S2", false, ToolCapabilityNarrowing::Configured, 0),
            ("S3", true, ToolCapabilityNarrowing::DenyAll, 0),
        ] {
            let counter = Arc::new(CountingTool(AtomicUsize::new(0)));
            let tools: Vec<Arc<dyn RawTool>> = vec![counter.clone()];
            let mut frozen = snapshot();
            // This is an ordinary prepared tool: no BackgroundTask plugin or
            // committed Task is claimed by the bounded registration test.
            frozen.resolved_spec.plugin_ids.clear();
            if !declared {
                frozen.resolved_spec.tool_descriptors.clear();
            }
            frozen.resolved_spec.plugin_config.agent.toolsets =
                vec![crate::config::test_agent_toolset_permission(
                    "count",
                    awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow,
                )];
            frozen.recompute_fingerprint().unwrap();
            let authorization = crate::config::effective_tool_authorization(
                &frozen.resolved_spec.plugin_config,
                &[],
                &frozen.resolved_spec.plugin_config.agent.toolsets,
            );
            let runtime = Arc::new(crate::config::build_runtime_with_authorization(
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                Arc::new(awaken_lease_testkit::ManualClock::new(1_234)),
                &crate::config::DeferredHandToolSource,
                &authorization,
                None,
                &tools,
            ));
            let prepared = PreparedToolExecutor::new(
                runtime,
                &frozen,
                RuntimeRunContext::new().with_tool_capability_narrowing(narrowing),
            )
            .unwrap();
            let call = ToolCall {
                call_id: format!("process-tool-{rule}"),
                tool_id: "count".into(),
                arguments: serde_json::json!({}),
            };
            let execution = prepared.execution(&call);
            if declared {
                let facts = execution.unwrap();
                assert_eq!(facts.recovery, ToolRecoveryPolicy::default(), "{rule}");
                assert_eq!(facts.concurrency, ToolConcurrency::Parallel, "{rule}");
                let result = prepared
                    .start_task(
                        &RunId(format!("process-tool-run-{rule}")),
                        &ThreadId("process-tool-thread".into()),
                        "process-tool-operation".into(),
                        &call,
                        &Store::new(),
                    )
                    .await;
                if expected == 1 {
                    assert!(matches!(result, Ok(ToolTaskStart::Completed(_))), "{rule}");
                } else {
                    assert!(
                        matches!(
                            result,
                            Err(awaken_runtime::DetachedToolError::NotAuthorized { .. })
                        ),
                        "{rule} the original Run ceiling precedes the executor"
                    );
                }
            } else {
                assert!(
                    matches!(
                        execution,
                        Err(awaken_runtime::DetachedToolError::NotExecutable(_))
                    ),
                    "{rule} backing registration cannot supply a missing frozen descriptor"
                );
            }
            assert_eq!(counter.0.load(Ordering::SeqCst), expected, "{rule}");
        }
    }

    #[tokio::test]
    async fn cold_and_warm_claimed_child_reconstruct_the_same_task_observer_without_child_storage()
    {
        use awaken_dispatch::{DispatchClaims as _, DispatchEnqueue as _, WorkerResolver as _};
        // C1 parent physical Root/Environment/commit are real and the child
        // origin is committed Ended with its original Task tenure; C2 parent
        // Runtime context is absent/present; C3 the existing resolver reconstructs
        // the child's frozen publication under its own Run claim. R1 cold and
        // R2 warm => one tool effect, one bound private attention and no child
        // physical Session. The warm parent has its own BackgroundTask observer:
        // it must not observe the child's terminal as parent authority. This
        // local Host/Worker/commit/tool test controls only the final application
        // ACK; Task HTTP and durable admission are independently covered.
        for warm in [false, true] {
            let parent = format!("recovered-task-parent-{warm}");
            let child = ThreadId(format!("recovered-task-child-{warm}"));
            let origin = RunId(format!("recovered-task-origin-{warm}"));
            let roots = crate::host::tests::ManagedSessionRootFixture::new().await;
            let counter = Arc::new(CountingTool(AtomicUsize::new(0)));
            let queue = Arc::new(awaken_dispatch::MemoryDispatchStore::new(
                awaken_time::system_clock(),
            ));
            let host = roots
                .configure(
                    crate::SessionExecutorBuilder::ephemeral(
                        Arc::new(crate::no_model::NoModelConfiguredExecutor),
                        "stub",
                    )
                    .await
                    .with_admin_tools(vec![counter.clone()])
                    // Only the existing owner projection changes; no second
                    // queue/feed or new Clock is constructed for cold/warm.
                    .with_ephemeral_dispatch(Arc::new(
                        awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(
                            queue.clone(),
                        ),
                    )),
                )
                .build()
                .unwrap();
            let application = Arc::new(RecordingAttentionApplication::accepting(0));
            // C4 the epoch-bearing child settles through the normal application
            // boundary before/after the Dispatch CAS. Reuse its existing recorder
            // and retain both complete local roles; an unused-role port is invalid
            // here. Only the final Task attention ACK remains controlled above.
            let coordination =
                Arc::new(crate::coordination::RecordingSessionAgentCoordination::default());
            let _applications = host
                .session_application_binding()
                .bind(
                    &awaken_session_execution_contract::SessionApplications::Local {
                        coordination: coordination.clone(),
                        background_runs: application.clone(),
                    },
                )
                .unwrap();
            let mut config = snapshot();
            config.resolved_spec.plugin_config.insert(
                awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.into(),
                serde_json::json!({"tools":["count"]}),
            );
            // The process registry supplies the executable, not a grant. Freeze
            // this fixture's explicit Agent permission before child compilation;
            // child authorization keeps its original empty extra_allowed input.
            config.resolved_spec.plugin_config.agent.toolsets =
                vec![crate::config::test_agent_toolset_permission(
                    "count",
                    awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow,
                )];
            config.recompute_fingerprint().unwrap();
            let mut projection = crate::host::empty_frozen_projection_for_snapshot(
                host.local_workspace(),
                crate::host::eager_environment(),
                &config,
            );
            projection.agent_publication = Some(config.clone());
            let projection = roots
                .install_projection(&host, &parent, projection)
                .await
                .unwrap();
            // An already-ended origin implies prior physical realization. Make
            // that real prerequisite through the original owner before seeding
            // its committed Runtime Task facts, without constructing a Runtime.
            drop(
                host.run_assembly
                    .session_child_execution_substrate(&parent, None)
                    .await
                    .unwrap(),
            );
            if warm {
                host.run_assembly.ctx_for(&parent, None).await.unwrap();
            }
            assert_eq!(
                host.run_assembly
                    .installed_sessions
                    .read_run(&parent, |run| run.context.is_some())
                    .unwrap(),
                Some(warm),
                "C2 actual Runtime residency"
            );
            let commit = host.thread_reads.commit_for_read(&parent).await.unwrap();
            let supervisor = host.capabilities.background_supervisor.clone();
            let mut task = BackgroundTask::requested(
                BackgroundTaskId::new(format!("recovered-task-{warm}")).unwrap(),
                BackgroundTaskOrigin {
                    thread_id: child.clone(),
                    run_id: origin.clone(),
                    operation_id: "background-tool-operation".into(),
                },
                BackgroundInvocation {
                    call: ToolCall {
                        call_id: "original-tool-call".into(),
                        tool_id: "count".into(),
                        arguments: serde_json::json!({}),
                    },
                },
            );
            task.start(
                supervisor.worker_id(),
                supervisor.clock().now_ms(),
                BackgroundTaskSupervisor::lease_ms(),
                TaskExecutionPolicy {
                    recovery: ToolRecoveryPolicy::default(),
                    concurrency: ToolConcurrency::Parallel,
                },
            )
            .unwrap();
            super::ownership::tests::persist(&commit, &task, &origin.0).await;
            let resources = projection
                .resource_transition(
                    awaken_session_execution_contract::FrozenResourceTransitionUse::ApplyEffects,
                )
                .unwrap()
                .desired()
                .clone();
            let request = awaken_session_run_delivery::child_dispatch_request(
                awaken_runtime_contract::RunActivation::new(
                    origin.clone(),
                    child.clone(),
                    config,
                    Vec::new(),
                ),
                ThreadId(parent.clone()),
                Some(resources),
                host.capabilities.agent.agent_publications.as_deref(),
                None,
            )
            .unwrap()
            .with_session_activity_epoch(7);
            queue.enqueue(request).await.unwrap();
            let claimed = queue
                .claim(
                    host.run_assembly.dispatch_owner(),
                    awaken_dispatch_execution::DEFAULT_LEASE_MS,
                    &Default::default(),
                )
                .await
                .unwrap()
                .unwrap();
            let resolver =
                crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch).unwrap();
            let worker = resolver.worker_for_claimed(&claimed).await.unwrap();
            worker
                .drive_claimed(claimed, host.clock())
                .await
                .unwrap()
                .expect("original terminal settles");
            // C4 => the original epoch-bearing child boundary reached the
            // selected recorder once with its actual physical/logical coordinates.
            {
                let boundaries = coordination.boundaries.lock().unwrap();
                assert_eq!(boundaries.len(), 1, "R1/R2 one child settlement");
                assert_eq!(boundaries[0].session_id.as_str(), parent, "C4 parent");
                assert_eq!(boundaries[0].source_thread_id, child, "C4 child");
                assert_eq!(boundaries[0].source_run_id, origin, "C4 origin Run");
                assert_eq!(
                    boundaries[0].source_agent_id, "background-agent",
                    "C4 Agent"
                );
                assert_eq!(boundaries[0].session_activity_epoch, 7, "C4 original epoch");
            }
            assert!(
                host.resources
                    .memory
                    .background()
                    .drain(std::time::Duration::from_secs(5))
                    .await,
                "R1/R2 Task effect and attention finish"
            );
            assert_eq!(
                counter.0.load(Ordering::SeqCst),
                1,
                "R1/R2 one actual tool effect; warm={warm}, completion={:?}, wait={:?}",
                supervisor.completion(task.id()),
                supervisor.wait_candidate(task.id()),
            );
            {
                let calls = application.calls.lock().unwrap();
                assert_eq!(calls.len(), 1, "R1/R2 no inherited parent observer");
                assert_eq!(calls[0].thread, child.0, "R1/R2 logical target");
                let input = calls[0].input.as_ref().unwrap();
                let tenure = input.tenure().unwrap();
                assert_eq!(tenure.thread, child.0, "R1/R2 bound logical Thread");
                assert_eq!(
                    tenure.session_thread(),
                    parent,
                    "R1/R2 original physical partition"
                );
                assert_eq!(tenure.run, origin.0, "R1/R2 original Task Run");
                assert_eq!(
                    tenure.holder,
                    supervisor.worker_id(),
                    "R1/R2 exact Task holder"
                );
            }
            assert!(
                !host
                    .run_assembly
                    .authority
                    .durable_thread_exists(&child.0)
                    .await
                    .unwrap(),
                "R1/R2 no child store"
            );
            assert!(
                host.run_assembly
                    .installed_sessions
                    .read(&child.0, |_| ())
                    .unwrap()
                    .is_none(),
                "R1/R2 no child installed Session"
            );
        }
    }
}
