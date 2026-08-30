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
use awaken_agent_contract::agent::state::Store;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_ext_background_task::{
    BackgroundTaskCompletion, BackgroundTaskEnd, BackgroundTaskId, BackgroundTaskLifecycle,
    BackgroundTaskSupervisor, BackgroundTaskWaitCandidate, BackgroundWait, TaskFence,
    tasks_from_state,
};
use awaken_runtime::{PreparedToolExecutor, Runtime};
use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_runtime_contract::resolved::ToolKind;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{
    CommittedTerminalRun, RunTerminalObserver, RunTerminalObserverError,
};
use awaken_runtime_contract::tool::{ToolOutput, ToolTaskHandle, ToolTaskPoll, ToolTaskStart};
use awaken_session_contract::{RunErrorKind, SessionRunBackgroundApplication, stable_fingerprint};

use crate::background::{BackgroundRuns, BackgroundWorkClass};
use crate::store::HostCommit;

const OBSERVER_ID: &str = "awaken.background-task.executor.v1";
const ATTENTION_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
];
const REMOTE_OBSERVATION_RETRIES: usize = 3;
const DEFAULT_REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn attention_identity(
    thread_id: &awaken_runtime_contract::ThreadId,
    task_id: &BackgroundTaskId,
    fence: &TaskFence,
    kind: &BackgroundAttention,
) -> String {
    stable_fingerprint(&(
        "background-task-attention-v1",
        thread_id.0.as_str(),
        task_id.as_str(),
        fence.worker_id.as_str(),
        fence.epoch,
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
    thread_id: &awaken_runtime_contract::ThreadId,
    task_id: &BackgroundTaskId,
    fence: &TaskFence,
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
             Use get_background_task with this task_id to inspect the authoritative status \
             and result. Decide whether to incorporate it, cancel related work, or continue \
             without it. Do not repeat work already completed by the task.",
            task_id.as_str(),
            kind.reason(),
        ),
    );
    (operation_id, message)
}

async fn publish_attention(
    application: Option<std::sync::Weak<dyn SessionRunBackgroundApplication>>,
    thread_id: &awaken_runtime_contract::ThreadId,
    task_id: &BackgroundTaskId,
    fence: &TaskFence,
    kind: BackgroundAttention,
) {
    let Some(application) = application.and_then(|application| application.upgrade()) else {
        return;
    };
    let (operation_id, message) = attention_message(thread_id, task_id, fence, &kind);
    let attempts = ATTENTION_RETRY_DELAYS
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None));
    for (attempt, retry_delay) in attempts.enumerate() {
        match application
            .submit_session_run_background(
                &operation_id,
                &thread_id.0,
                None,
                vec![message.clone()],
                None,
            )
            .await
        {
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
    commit: Arc<HostCommit>,
    background: Arc<BackgroundRuns>,
    supervisor: Arc<BackgroundTaskSupervisor>,
    session_id: String,
    environment_generation: String,
    attention: Option<std::sync::Weak<dyn SessionRunBackgroundApplication>>,
}

impl BackgroundTaskTerminalObserver {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        snapshot: ExecutableAgentSnapshot,
        context: RuntimeRunContext,
        commit: Arc<HostCommit>,
        background: Arc<BackgroundRuns>,
        supervisor: Arc<BackgroundTaskSupervisor>,
        session_id: String,
        environment_generation: String,
        attention: Option<std::sync::Weak<dyn SessionRunBackgroundApplication>>,
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
        fence: TaskFence,
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

    fn indeterminate(fence: TaskFence, message: impl Into<String>) -> BackgroundTaskCompletion {
        BackgroundTaskCompletion {
            fence,
            end: BackgroundTaskEnd::Indeterminate {
                message: message.into(),
            },
        }
    }

    fn start_failure(
        fence: TaskFence,
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

    async fn launch(
        &self,
        task_id: BackgroundTaskId,
        fence: TaskFence,
        call: awaken_runtime_contract::tool::ToolCall,
        thread_id: awaken_runtime_contract::ThreadId,
        expected: awaken_ext_background_task::TaskExecutionPolicy,
    ) {
        let Some(cancellation) = self.supervisor.register(&task_id) else {
            return;
        };
        let mut context = self.context.clone();
        context.cancellation = Some(cancellation.clone());
        context.terminal_observers.clear();
        let prepared =
            match PreparedToolExecutor::new(self.runtime.clone(), &self.snapshot, context) {
                Ok(prepared) => Arc::new(prepared),
                Err(error) => {
                    let completion = Self::completion(fence.clone(), Err(error.to_string()));
                    self.supervisor.complete(task_id.clone(), completion);
                    publish_attention(
                        self.attention.clone(),
                        &thread_id,
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
                    &thread_id,
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
                &thread_id,
                &task_id,
                &fence,
                BackgroundAttention::Terminal,
            )
            .await;
            return;
        }
        let tool_kind = resolved.kind;
        let state = Store::rebuild(&self.commit.committed_state(&thread_id));
        let supervisor = self.supervisor.clone();
        let attention = self.attention.clone();
        let class = BackgroundWorkClass::SharedEnvironment {
            session_id: self.session_id.clone(),
            generation_id: self.environment_generation.clone(),
        };
        self.background
            .spawn(class, async move {
                let run_id =
                    awaken_runtime_contract::RunId(format!("background-{}", task_id.as_str()));
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
                                &thread_id,
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
                                &thread_id,
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
                            &thread_id,
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
                            &thread_id,
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

    #[allow(clippy::too_many_arguments)]
    async fn launch_remote(
        &self,
        task_id: BackgroundTaskId,
        fence: TaskFence,
        call: awaken_runtime_contract::tool::ToolCall,
        thread_id: awaken_runtime_contract::ThreadId,
        expected: awaken_ext_background_task::TaskExecutionPolicy,
        continuation: ToolTaskHandle,
        lease_expires_at_ms: u64,
        cancelling: bool,
    ) {
        let cancellation = self
            .supervisor
            .resume_wait(&task_id, &fence)
            .or_else(|| self.supervisor.register(&task_id));
        let Some(cancellation) = cancellation else {
            if cancelling {
                self.supervisor.cancel(&task_id);
            }
            return;
        };
        let mut context = self.context.clone();
        context.cancellation = Some(cancellation.clone());
        context.terminal_observers.clear();
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
                        &thread_id,
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
                    &thread_id,
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
                &thread_id,
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
                &thread_id,
                &task_id,
                &fence,
                BackgroundAttention::Terminal,
            )
            .await;
            return;
        }
        let state = Store::rebuild(&self.commit.committed_state(&thread_id));
        let supervisor = self.supervisor.clone();
        let attention = self.attention.clone();
        let class = BackgroundWorkClass::SharedEnvironment {
            session_id: self.session_id.clone(),
            generation_id: self.environment_generation.clone(),
        };
        self.background
            .spawn(class, async move {
                let run_id =
                    awaken_runtime_contract::RunId(format!("background-{}", task_id.as_str()));
                let renew_at = lease_expires_at_ms
                    .saturating_sub(BackgroundTaskSupervisor::lease_ms() / 2);
                let mut cancelling = cancelling;
                let mut cancel_sent = false;
                let mut failures = 0usize;
                loop {
                    let now = BackgroundTaskSupervisor::now_ms();
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
                            fence.epoch,
                            lease_expires_at_ms,
                        ));
                        publish_attention(
                            attention,
                            &thread_id,
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
                    if BackgroundTaskSupervisor::now_ms() >= renew_at {
                        continue;
                    }
                    let operation = format!(
                        "background:{}:{}:{}",
                        task_id.as_str(),
                        fence.epoch,
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
                                &thread_id,
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
                                fence.epoch,
                                message.as_deref().unwrap_or_default(),
                            ));
                            publish_attention(
                                attention.clone(),
                                &thread_id,
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
                                &thread_id,
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
                                &thread_id,
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
                                    end: BackgroundTaskEnd::Cancelled,
                                },
                            );
                            publish_attention(
                                attention,
                                &thread_id,
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
        let state = Store::rebuild(&self.commit.committed_state(&terminal.thread_id));
        for task in tasks_from_state(&state).map_err(|error| error.to_string())? {
            match &task.lifecycle {
                BackgroundTaskLifecycle::Running { attempt }
                    if attempt.worker_id == self.supervisor.worker_id() =>
                {
                    self.launch(
                        task.id,
                        attempt.fence(),
                        task.invocation.call,
                        task.origin.thread_id,
                        attempt.policy.clone(),
                    )
                    .await;
                }
                BackgroundTaskLifecycle::Waiting {
                    attempt,
                    wait: BackgroundWait::Remote(continuation),
                } if attempt.worker_id == self.supervisor.worker_id() => {
                    self.launch_remote(
                        task.id,
                        attempt.fence(),
                        task.invocation.call,
                        task.origin.thread_id,
                        attempt.policy.clone(),
                        continuation.clone(),
                        attempt.lease_expires_at_ms,
                        false,
                    )
                    .await;
                }
                BackgroundTaskLifecycle::Cancelling {
                    attempt,
                    wait: Some(BackgroundWait::Remote(continuation)),
                } if attempt.worker_id == self.supervisor.worker_id() => {
                    self.launch_remote(
                        task.id,
                        attempt.fence(),
                        task.invocation.call,
                        task.origin.thread_id,
                        attempt.policy.clone(),
                        continuation.clone(),
                        attempt.lease_expires_at_ms,
                        true,
                    )
                    .await;
                }
                BackgroundTaskLifecycle::Cancelling { attempt, .. }
                    if attempt.worker_id == self.supervisor.worker_id() =>
                {
                    self.supervisor.cancel(&task.id);
                }
                BackgroundTaskLifecycle::Ended { .. } => self.supervisor.retire(&task.id),
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
    use awaken_runtime_contract::tool::{
        RawTool, ToolCall, ToolConcurrency, ToolError, ToolOutputSpiller, ToolRecoveryPolicy,
        ToolResource, ToolResourceAccess,
    };

    #[derive(Clone, Debug, PartialEq)]
    struct AttentionCall {
        operation_id: String,
        thread: String,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
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

    #[async_trait]
    impl SessionRunBackgroundApplication for RecordingAttentionApplication {
        async fn submit_session_run_background(
            &self,
            operation_id: &str,
            thread: &str,
            agent: Option<String>,
            messages: Vec<Message>,
            traceparent: Option<String>,
        ) -> Result<RunId, awaken_session_contract::RunApplicationError> {
            self.calls
                .lock()
                .expect("attention calls mutex poisoned")
                .push(AttentionCall {
                    operation_id: operation_id.to_string(),
                    thread: thread.to_string(),
                    agent,
                    messages,
                    traceparent,
                });
            if self.definitive_rejection {
                return Err(awaken_session_contract::RunError::bad_request(
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
                return Err(awaken_session_contract::RunError::unavailable(
                    "transient test failure",
                ));
            }
            Ok(awaken_session_contract::session_run_id(
                thread,
                operation_id,
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
        let first = TaskFence {
            worker_id: "worker-secret".into(),
            epoch: 1,
        };
        let second = TaskFence {
            worker_id: "worker-secret".into(),
            epoch: 2,
        };
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
        assert!(text.contains("get_background_task"), "R3 state lookup");
        assert!(!text.contains("worker-secret"), "R3 no worker identity");
    }

    #[test]
    fn detached_task_start_failure_never_claims_a_remote_failure() {
        // Cause/effect decision table: R1 a Regular target returns a terminal
        // error from its default start adapter -> Failed; R2 a DetachedOnly
        // protocol target loses or rejects the start response -> Indeterminate,
        // because no durable remote task id crossed ThreadCommit and replay may
        // duplicate an external effect. Both rows preserve the original fence.
        let fence = TaskFence {
            worker_id: "worker".into(),
            epoch: 7,
        };
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

    #[tokio::test(start_paused = true)]
    async fn attention_publication_retries_ambiguous_failures_with_one_identity_only() {
        // Cause/effect decision table:
        // R4 no application/expired Weak -> no publication and task truth stays
        // untouched; R5 unavailable/internal response -> retry the identical
        // Session command; R6 eventual acceptance -> stop after one successful
        // admission; R7 BadRequest -> stop immediately because rejection is
        // definitive. Exact operation identity makes response-loss retry safe.
        let thread = ThreadId("attention-thread".into());
        let task = BackgroundTaskId::new("attention-task").expect("task id");
        let fence = TaskFence {
            worker_id: "worker".into(),
            epoch: 1,
        };

        publish_attention(None, &thread, &task, &fence, BackgroundAttention::Terminal).await;

        let accepting = Arc::new(RecordingAttentionApplication::accepting(2));
        let accepting_port: Arc<dyn SessionRunBackgroundApplication> = accepting.clone();
        publish_attention(
            Some(Arc::downgrade(&accepting_port)),
            &thread,
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
            Some(Arc::downgrade(&rejecting_port)),
            &thread,
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
            Some(Arc::downgrade(&exhausted_port)),
            &thread,
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

    #[test]
    fn session_background_application_composition_is_single_assignment_and_weak() {
        // Cause/effect decision table: R8 vacant slot + application -> one weak
        // executable edge; R9 occupied slot + another install -> reject without
        // replacement; R10 all external strong owners drop -> SharedHost does
        // not retain the Session application. This prevents both a second Run
        // admission owner and an Arc ownership cycle.
        let host = Arc::new(crate::SharedHost::new(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
        ));
        let managed = crate::ManagedHost::new(host.clone());
        let application = Arc::new(RecordingAttentionApplication::accepting(0));
        let port: Arc<dyn SessionRunBackgroundApplication> = application.clone();
        managed
            .install_session_background_run_application(Arc::downgrade(&port))
            .expect("R8 first install");
        assert_eq!(
            managed.install_session_background_run_application(Arc::downgrade(&port)),
            Err(crate::SessionRunBackgroundInstallError::AlreadyInstalled),
            "R9 single assignment",
        );
        drop(application);
        drop(port);
        assert!(
            host.session_background_runs
                .read()
                .expect("Session background Run application lock poisoned")
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .is_none(),
            "R10 weak ownership edge"
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
        let admission = background.tool_execution_admission("session", "generation");
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
        let admission = background.tool_execution_admission("session", "generation");
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
        ) -> Result<ToolOutput, awaken_runtime_contract::tool::ToolError> {
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
            _conversation: &[awaken_runtime_contract::Message],
            _state: &Store,
        ) -> awaken_runtime_contract::plugin::HookReaction {
            self.0.fetch_add(1, Ordering::SeqCst);
            awaken_runtime_contract::plugin::HookReaction::default()
        }
    }

    struct RecordingAfterToolPlugin(Arc<AtomicUsize>);

    impl awaken_runtime_contract::plugin::Plugin for RecordingAfterToolPlugin {
        fn manifest(&self) -> awaken_runtime_contract::plugin::PluginManifest {
            awaken_runtime_contract::plugin::PluginManifest {
                id: "test.after-tool".into(),
                bound: awaken_runtime_contract::plugin::CapabilityBound {
                    phase_hooks: vec![awaken_runtime_contract::plugin::PhaseHookPoint::AfterTool],
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
                max_steps: 1,
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
    async fn committed_claim_launches_once_and_records_completion_without_a_second_store() {
        // Cause graph: C1 Running task is absent from committed State -> observer
        // sees nothing; C2 the exact claim is committed -> duplicate observer
        // deliveries race; C3 supervisor registration wins once -> E1 one tool
        // effect, E2 one process-local completion, E3 no observer-side commit;
        // C4 another plugin owns an AfterTool workflow hook -> E4 detached target
        // execution never advances that foreground-only workflow event;
        // C5 the canonical output spiller is bound -> E5 completion retains only
        // its stable materialized result reference, not a parallel output store;
        // C6 a Session background Run port is installed -> E6 one same-Thread
        // System attention command is submitted after process completion.
        let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker-test"));
        let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
            BackgroundTaskConfig {
                tools: BTreeSet::from(["count".into()]),
            },
            supervisor.clone(),
        ));
        let counter = Arc::new(CountingTool(AtomicUsize::new(0)));
        let spills = Arc::new(Mutex::new(Vec::new()));
        let after_tool_calls = Arc::new(AtomicUsize::new(0));
        let runtime = Arc::new(
            Runtime::new()
                .with_plugin(plugin)
                .with_plugin(Arc::new(RecordingAfterToolPlugin(after_tool_calls.clone())))
                .with_tool(counter.clone()),
        );
        let memory = awaken_store_inmem::MemoryCommitCoordinator::new();
        let commit = Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(memory),
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
            commit.clone(),
            background.clone(),
            supervisor.clone(),
            "session".into(),
            "generation".into(),
            Some(Arc::downgrade(&attention_port)),
        );
        let thread_id = ThreadId("thread".into());
        let origin_run = RunId("origin".into());
        let terminal = CommittedTerminalRun {
            run_id: origin_run.clone(),
            thread_id: thread_id.clone(),
            cause: awaken_runtime_contract::EndCause::NaturalEnd,
        };
        observer.observe(&terminal).await.expect("C1 observer");
        assert_eq!(counter.0.load(Ordering::SeqCst), 0, "C1");

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
                BackgroundTaskSupervisor::now_ms(),
                BackgroundTaskSupervisor::lease_ms(),
                TaskExecutionPolicy {
                    recovery: ToolRecoveryPolicy::default(),
                    concurrency: ToolConcurrency::Parallel,
                },
            )
            .expect("claim");
        commit
            .commit(ThreadCommit::assemble(
                thread_id.clone(),
                RunDisposition::ended(origin_run, awaken_runtime_contract::EndCause::NaturalEnd),
                true,
                Vec::new(),
                vec![
                    task_state_cell(&task.id)
                        .write(&task)
                        .expect("task command"),
                ],
                Vec::new(),
            ))
            .await
            .expect("C2 commit");
        observer.observe(&terminal).await.expect("first delivery");
        observer
            .observe(&terminal)
            .await
            .expect("duplicate delivery");
        assert!(background.drain(std::time::Duration::from_secs(2)).await);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "C3/E1");
        let attention_calls = attention
            .calls
            .lock()
            .expect("attention calls mutex poisoned");
        assert_eq!(attention_calls.len(), 1, "C3/E6 one attention Run");
        assert_eq!(attention_calls[0].thread, thread_id.0, "C3/E6 owner");
        assert_eq!(attention_calls[0].messages[0].role, Role::System, "C3/E6");
        drop(attention_calls);
        assert_eq!(after_tool_calls.load(Ordering::SeqCst), 0, "C4/E4");
        let completion = supervisor.completion(&task.id).expect("C3/E2 completion");
        assert_eq!(completion.fence, fence);
        assert!(matches!(
            &completion.end,
            BackgroundTaskEnd::Completed { content, is_error: false }
                if content == &vec![awaken_runtime_contract::ContentBlock::text(
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
            "C5/E5: detached execution reuses the canonical stable spill identity"
        );
        assert!(
            supervisor.register(&task.id).is_none(),
            "completion remains a deduplication guard until durable Ended truth"
        );
    }

    struct RemoteTaskTool {
        starts: AtomicUsize,
        polls: AtomicUsize,
    }

    #[async_trait]
    impl RawTool for RemoteTaskTool {
        fn id(&self) -> &str {
            "remote"
        }

        async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
            panic!("durable detached target must use start_task")
        }

        async fn start_task(&self, _call: ToolCall) -> Result<ToolTaskStart, ToolError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(ToolTaskStart::Pending(ToolTaskHandle {
                owner: "fixture-durable-adapter".into(),
                binding: "remote".into(),
                task_id: "remote-secret-id".into(),
                poll_interval_ms: Some(1),
            }))
        }

        async fn poll_task(
            &self,
            call: &ToolCall,
            task: &ToolTaskHandle,
        ) -> Result<ToolTaskPoll, ToolError> {
            assert_eq!(task.task_id, "remote-secret-id");
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolTaskPoll::Completed(ToolOutput::ok(
                &call.call_id,
                "remote complete",
            )))
        }
    }

    #[tokio::test]
    async fn committed_remote_handle_is_polled_without_replaying_start() {
        // Cause/effect decision table: R1 committed Running -> exactly one
        // start_task and a process Wait candidate; R2 candidate is committed as
        // Waiting -> the same supervisor slot resumes one poll by the saved
        // handle; R3 an arbitrary non-MCP adapter owner is carried opaquely;
        // R4 explicit Completed -> terminal candidate and a distinct attention
        // identity. Constraints: no second start, no adapter/remote id in
        // System messages, and Thread State remains the only durable truth.
        let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker-remote"));
        let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
            BackgroundTaskConfig {
                tools: BTreeSet::from(["remote".into()]),
            },
            supervisor.clone(),
        ));
        let remote = Arc::new(RemoteTaskTool {
            starts: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
        });
        let runtime = Arc::new(Runtime::new().with_plugin(plugin).with_tool(remote.clone()));
        let memory = awaken_store_inmem::MemoryCommitCoordinator::new();
        let commit = Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(memory),
        )));
        let background = Arc::new(BackgroundRuns::new());
        let attention = Arc::new(RecordingAttentionApplication::accepting(0));
        let attention_port: Arc<dyn SessionRunBackgroundApplication> = attention.clone();
        let mut remote_snapshot = snapshot();
        remote_snapshot.resolved_spec.tool_descriptors = vec![
            ToolDescriptor::pinned(
                "test",
                "remote",
                "Remote work.",
                serde_json::json!({"type":"object"}),
            )
            .with_kind(awaken_runtime_contract::resolved::ToolKind::DetachedOnly),
        ];
        let observer = BackgroundTaskTerminalObserver::new(
            runtime,
            remote_snapshot,
            RuntimeRunContext::new(),
            commit.clone(),
            background.clone(),
            supervisor.clone(),
            "session".into(),
            "generation".into(),
            Some(Arc::downgrade(&attention_port)),
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
                BackgroundTaskSupervisor::now_ms(),
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
                    awaken_runtime_contract::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                vec![task_state_cell(&task.id).write(task).expect("task state")],
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
                cause: awaken_runtime_contract::EndCause::NaturalEnd,
            })
            .await
            .expect("R1 observe");
        assert!(background.drain(Duration::from_secs(2)).await);
        assert_eq!(remote.starts.load(Ordering::SeqCst), 1, "R1");
        let waiting = supervisor
            .wait_candidate(&task.id)
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
                cause: awaken_runtime_contract::EndCause::NaturalEnd,
            })
            .await
            .expect("R2 observe");
        assert!(background.drain(Duration::from_secs(2)).await);
        assert_eq!(remote.starts.load(Ordering::SeqCst), 1, "R2 no replay");
        assert_eq!(remote.polls.load(Ordering::SeqCst), 1, "R2 one poll");
        assert!(
            matches!(
                supervisor
                    .completion(&task.id)
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
        assert_eq!(fence.epoch, 1);
    }
}
