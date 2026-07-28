//! `A2aRunExecutor`: a remote A2A agent (Coze / any A2A HTTP endpoint) driven as a
//! peer [`RunAttemptExecutor`]. Like the ACP executor it is *a second implementation*
//! of the one attempt port — no local process, no model loop of our own: it dials the
//! endpoint on `Backend::Remote { endpoint }`, sends the run's prompt as an A2A
//! `message:send`, and commits the returned task's reply through the same commit
//! boundary as the native and ACP paths.
//!
//! Boundaries: runtime plane; depends only on the foundation contracts + the A2A
//! protocol crate. The dial endpoint comes from the resolved backend, so this
//! executor stays config-free; a [`TransportResolver`] is the one injected seam.
//! The host resolves publication-pinned transport authentication behind that
//! port; tests may substitute an anonymous or scripted transport.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::{AwaitReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};
use awaken_agent_contract::agent::state::{
    Action as StateAction, Command as StateCommand, MergePolicy, Scope,
};
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_protocol_a2a::client::{get_task, send_message, try_cancel_task};
use awaken_protocol_a2a::{Task, TaskState};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    Cancellation, Error, ExecutorCapabilities, Result, RunAttemptExecutor, RunExecutor, Wait,
};
use awaken_runtime_contract::permission::ToolCapabilityNarrowing;
use awaken_runtime_contract::resolved::{Backend, ResolvedModelCandidate};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult, validate_resume};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{CommittedTerminalRun, deliver_committed_terminal};

mod task_driver;
pub use awaken_protocol_a2a::{HttpTransport, Transport};

/// Resolves one publication-pinned remote candidate into a live transport.
///
/// Implementations may materialize only the exact claim binding carried by
/// `context`; selecting credentials, endpoints or fallback candidates is outside
/// this port. The executor never sees plaintext material.
#[async_trait]
pub trait TransportResolver: Send + Sync {
    async fn resolve(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &RuntimeRunContext,
    ) -> std::result::Result<Arc<dyn Transport>, String>;
}

/// Drives a remote A2A agent as a [`RunAttemptExecutor`].
pub struct A2aRunExecutor {
    transport_resolver: Arc<dyn TransportResolver>,
}

const A2A_TASK_STATE_KEY: &str = "__a2a_task";
/// The opaque remote identity committed immediately after `message:send` returns.
/// It is Run-scoped state, so a replacement worker can reattach without sending a
/// second user message and durable cancellation can address the same remote task.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskReference {
    endpoint: String,
    task_id: String,
    context_id: String,
}

impl TaskReference {
    fn from_task(endpoint: &str, task: &Task) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
        }
    }
}

impl A2aRunExecutor {
    #[must_use]
    pub fn new(transport_resolver: Arc<dyn TransportResolver>) -> Self {
        Self { transport_resolver }
    }

    /// Anonymous HTTP is a test compatibility fixture. Production composition
    /// must inject a resolver that verifies the publication's Agent Card
    /// security fingerprint and claim-frozen credential authority.
    #[cfg(test)]
    #[must_use]
    pub fn over_http() -> Self {
        Self::new(Arc::new(AnonymousHttpTransportResolver))
    }
}

#[cfg(test)]
struct AnonymousHttpTransportResolver;

#[cfg(test)]
#[async_trait]
impl TransportResolver for AnonymousHttpTransportResolver {
    async fn resolve(
        &self,
        candidate: &ResolvedModelCandidate,
        _context: &RuntimeRunContext,
    ) -> std::result::Result<Arc<dyn Transport>, String> {
        let endpoint = candidate
            .binding
            .backend_ref
            .strip_prefix("a2a:")
            .ok_or_else(|| "anonymous A2A fixture received a non-remote candidate".to_string())?;
        Ok(Arc::new(HttpTransport::new(endpoint)))
    }
}

fn ensure_supported_narrowing(
    activation: &RunActivation,
    context: &RuntimeRunContext,
) -> Result<()> {
    if activation.tool_capability_narrowing == ToolCapabilityNarrowing::DenyAll
        || context.tool_permission_policy.is_some()
    {
        return Err(Error::Execution(
            "A2A cannot prove enforcement of this Run's deny-all tool capability".to_string(),
        ));
    }
    Ok(())
}

/// The turn's prompt: the concatenated text of the activation's input.
fn prompt_of(input: &[Message]) -> String {
    input
        .iter()
        .map(Message::text_content)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The agent's reply from a returned task: its durable artifacts, else the terminal
/// status message, else the last history message.
fn task_reply(task: &Task) -> String {
    let artifacts: Vec<String> = task
        .artifacts
        .iter()
        .map(|a| a.text())
        .filter(|t| !t.is_empty())
        .collect();
    if !artifacts.is_empty() {
        return artifacts.join("\n");
    }
    if let Some(message) = &task.status.message {
        let text = message.text();
        if !text.is_empty() {
            return text;
        }
    }
    task.history.last().map(|m| m.text()).unwrap_or_default()
}

/// The run's end derived from a task that reached a lifecycle boundary. Do not
/// collapse every returned task to a natural end: a `failed` remote task is an
/// execution fault and a `canceled` one is a cancellation. Pollable states remain
/// indeterminate until the shared task driver reaches a terminal or await boundary
/// and must never be projected as success (G26).
fn end_cause_of(state: &TaskState) -> EndCause {
    match state {
        TaskState::Completed => EndCause::NaturalEnd,
        TaskState::Failed => EndCause::Error(Failure::Inference {
            code: "a2a_task_failed".to_string(),
            message: "remote A2A task ended in the failed state".to_string(),
        }),
        TaskState::Canceled => EndCause::Cancelled,
        TaskState::Rejected => EndCause::Error(Failure::Inference {
            code: "a2a_task_rejected".to_string(),
            message: "remote A2A task ended in the rejected state".to_string(),
        }),
        TaskState::Submitted
        | TaskState::Working
        | TaskState::InputRequired
        | TaskState::AuthRequired
        | TaskState::Unknown => EndCause::Indeterminate,
    }
}

fn task_reference_state(reference: &TaskReference) -> StateCommand {
    StateCommand::set(
        Scope::Run,
        MergePolicy::Disjoint,
        A2A_TASK_STATE_KEY,
        serde_json::json!({
            "endpoint": reference.endpoint,
            "task_id": reference.task_id,
            "context_id": reference.context_id,
        }),
    )
}

fn decode_task_reference(value: &serde_json::Value) -> Result<TaskReference> {
    let field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| Error::Execution(format!("durable A2A task is missing {name}")))
    };
    Ok(TaskReference {
        endpoint: field("endpoint")?,
        task_id: field("task_id")?,
        context_id: field("context_id")?,
    })
}

fn clear_task_reference_state() -> StateCommand {
    StateCommand::remove(Scope::Run, MergePolicy::Disjoint, A2A_TASK_STATE_KEY)
}

fn restored_task_reference(
    context: &RuntimeRunContext,
    activation: &RunActivation,
) -> Result<Option<TaskReference>> {
    let Some(reader) = &context.reader else {
        return Ok(None);
    };
    for command in reader
        .committed_state(&activation.thread_id)
        .into_iter()
        .rev()
    {
        if command.scope != Scope::Run
            || command.run_id.as_ref() != Some(&activation.run_id)
            || command.key.0 != A2A_TASK_STATE_KEY
        {
            continue;
        }
        return match command.action {
            StateAction::Set(value) => decode_task_reference(&value).map(Some),
            StateAction::Remove => Ok(None),
        };
    }
    Ok(None)
}

fn remote_candidate_of(activation: &RunActivation) -> Result<&ResolvedModelCandidate> {
    let backend = Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref);
    if !matches!(backend, Backend::Remote { .. }) {
        return Err(Error::Execution(
            "A2A executor received a non-remote backend".to_string(),
        ));
    }
    Ok(&activation.snapshot.resolved_spec.model_binding)
}

fn endpoint_of(candidate: &ResolvedModelCandidate) -> Result<String> {
    Backend::from_ref(&candidate.binding.backend_ref)
        .remote_endpoint()
        .map(str::to_string)
        .ok_or_else(|| Error::Execution("A2A executor received a non-remote backend".to_string()))
}

fn ensure_endpoint(reference: &TaskReference, endpoint: &str) -> Result<()> {
    if reference.endpoint == endpoint {
        Ok(())
    } else {
        Err(Error::Execution(format!(
            "durable A2A task belongs to endpoint {:?}, not {:?}",
            reference.endpoint, endpoint
        )))
    }
}

fn resume_text(result: &ResumeResult) -> String {
    match result {
        ResumeResult::ToolResult(output) => output.content.clone(),
        ResumeResult::Input(text) => text.clone(),
        ResumeResult::Decision { allow, note } => note
            .clone()
            .unwrap_or_else(|| if *allow { "allow" } else { "deny" }.to_string()),
    }
}

fn awaiting_ticket(activation: &RunActivation, task: &Task) -> ResumeTicket {
    ResumeTicket {
        correlation_id: format!("a2a:{}:{:?}", task.id, task.status.state),
        run_id: activation.run_id.clone(),
        thread_id: activation.thread_id.clone(),
        snapshot_id: activation.snapshot.id.0.clone(),
        catalog_fingerprint: activation
            .snapshot
            .resolved_spec
            .catalog_fingerprint
            .0
            .clone(),
        delegation_origin: activation.delegation_origin.clone(),
        reason: match task.status.state {
            TaskState::InputRequired => AwaitReason::UserInput,
            TaskState::AuthRequired => AwaitReason::ExternalEvent,
            _ => unreachable!("only input/auth-required tasks await"),
        },
        call_id: Some(task.id.clone()),
        pending_tool: None,
        deadline_ms: None,
    }
}

#[async_trait]
impl RunExecutor for A2aRunExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            cancellation: Cancellation::RemoteAbort,
            wait: Wait::Both,
        }
    }

    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        ensure_supported_narrowing(&activation, &context)?;
        let Ok(candidate) = remote_candidate_of(&activation) else {
            // Reached without a remote backend — a wiring fault; fail closed.
            let mut messages = activation.input.clone();
            messages.push(Message::text(
                MessageId("a2a-err-1".to_string()),
                Role::Assistant,
                "backend is not an A2A endpoint".to_string(),
            ));
            return finish_terminal(
                &context,
                &activation,
                messages,
                EndCause::Error(Failure::Inference {
                    code: "a2a_config".to_string(),
                    message: "backend is not a2a".to_string(),
                }),
            )
            .await;
        };
        let endpoint = endpoint_of(candidate)?;

        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let task = match restored_task_reference(&context, &activation)? {
            Some(reference) => {
                ensure_endpoint(&reference, &endpoint)?;
                get_task(transport.as_ref(), &reference.task_id)
                    .await
                    .map_err(|error| Error::Execution(error.to_string()))?
            }
            None => {
                let prompt = prompt_of(&activation.input);
                let message_id = format!("a2a-msg-{}", activation.run_id.0);
                let task = match send_message(
                    transport.as_ref(),
                    None,
                    &activation.thread_id.0,
                    &message_id,
                    &prompt,
                )
                .await
                {
                    Ok(task) => task,
                    Err(err) => {
                        let mut messages = activation.input.clone();
                        messages.push(Message::text(
                            MessageId("a2a-err-1".to_string()),
                            Role::Assistant,
                            format!("remote agent error: {err}"),
                        ));
                        return finish_terminal(
                            &context,
                            &activation,
                            messages,
                            EndCause::Error(Failure::Inference {
                                code: "a2a_error".to_string(),
                                message: err.to_string(),
                            }),
                        )
                        .await;
                    }
                };
                commit_boundary(
                    &context,
                    &activation,
                    RunDisposition::running(activation.run_id.clone()),
                    activation.input.clone(),
                    vec![task_reference_state(&TaskReference::from_task(
                        &endpoint, &task,
                    ))],
                )
                .await?;
                task
            }
        };
        drive_task(transport.as_ref(), &endpoint, &activation, &context, task).await
    }
}

#[async_trait]
impl RunAttemptExecutor for A2aRunExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        ensure_supported_narrowing(&activation, &context)?;
        let reader = context
            .reader
            .as_ref()
            .ok_or_else(|| Error::Execution("A2A resume requires committed history".to_string()))?;
        let ticket = reader
            .resume_ticket(&activation.run_id)
            .ok_or_else(|| Error::Execution("A2A run is not awaiting a resume".to_string()))?;
        validate_resume(&ticket, &command)
            .map_err(|error| Error::Execution(format!("invalid A2A resume: {error}")))?;
        let candidate = remote_candidate_of(&activation)?;
        let endpoint = endpoint_of(candidate)?;
        let reference = restored_task_reference(&context, &activation)?.ok_or_else(|| {
            Error::Execution("A2A resume is missing its durable remote task".to_string())
        })?;
        ensure_endpoint(&reference, &endpoint)?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let message_id = format!(
            "a2a-resume-{}-{}",
            activation.run_id.0, ticket.correlation_id
        );
        let task = send_message(
            transport.as_ref(),
            None,
            &reference.context_id,
            &message_id,
            &resume_text(&command.result),
        )
        .await
        .map_err(|error| Error::Execution(error.to_string()))?;
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![task_reference_state(&TaskReference::from_task(
                &endpoint, &task,
            ))],
        )
        .await?;
        drive_task(transport.as_ref(), &endpoint, &activation, &context, task).await
    }

    async fn cancel(&self, activation: RunActivation, context: RuntimeRunContext) -> Result<()> {
        let candidate = remote_candidate_of(&activation)?;
        let endpoint = endpoint_of(candidate)?;
        let Some(reference) = restored_task_reference(&context, &activation)? else {
            return Ok(());
        };
        ensure_endpoint(&reference, &endpoint)?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let task = get_task(transport.as_ref(), &reference.task_id)
            .await
            .map_err(|error| Error::Execution(error.to_string()))?;
        if matches!(
            task.status.state,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        ) {
            return Ok(());
        }
        try_cancel_task(transport.as_ref(), &reference.task_id)
            .await
            .map_err(|error| Error::Execution(error.to_string()))
    }
}

async fn drive_task(
    transport: &dyn Transport,
    endpoint: &str,
    activation: &RunActivation,
    context: &RuntimeRunContext,
    task: Task,
) -> Result<RunState> {
    let task =
        match task_driver::poll_to_boundary(transport, task, context.cancellation.as_ref()).await {
            Ok(task) => task,
            Err(task_driver::PollError::Cancelled) => {
                return finish_terminal(context, activation, Vec::new(), EndCause::Cancelled).await;
            }
            Err(task_driver::PollError::Timeout) => {
                return Err(Error::Execution(
                    "remote A2A task did not reach a boundary in time".to_string(),
                ));
            }
            Err(task_driver::PollError::Client(error)) => {
                return Err(Error::Execution(error.to_string()));
            }
        };
    let reply = task_reply(&task);
    let messages = (!reply.is_empty())
        .then(|| {
            Message::text(
                MessageId(format!("a2a-{}-{}", activation.run_id.0, task.id)),
                Role::Assistant,
                reply,
            )
        })
        .into_iter()
        .collect();
    match task.status.state {
        TaskState::InputRequired | TaskState::AuthRequired => {
            let reference = TaskReference::from_task(endpoint, &task);
            commit_boundary(
                context,
                activation,
                RunDisposition::awaiting(awaiting_ticket(activation, &task)),
                messages,
                vec![task_reference_state(&reference)],
            )
            .await?;
            Ok(RunState::Awaiting)
        }
        TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected => {
            finish_terminal(
                context,
                activation,
                messages,
                end_cause_of(&task.status.state),
            )
            .await
        }
        TaskState::Submitted | TaskState::Working | TaskState::Unknown => {
            unreachable!("the shared task driver returns only a boundary")
        }
    }
}

async fn finish_terminal(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    messages: Vec<Message>,
    cause: EndCause,
) -> Result<RunState> {
    commit_boundary(
        context,
        activation,
        RunDisposition::ended(activation.run_id.clone(), cause.clone()),
        messages,
        vec![clear_task_reference_state()],
    )
    .await?;
    Ok(RunState::Ended(cause))
}

/// Commit one remote lifecycle boundary through the same atomic Run boundary as
/// Native and ACP. The task reference is therefore never ahead of its Run state.
async fn commit_boundary(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    disposition: RunDisposition,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
) -> Result<()> {
    if let Some(coordinator) = &context.commit {
        let terminal_cause = match disposition.state() {
            RunState::Ended(cause) => Some(cause),
            RunState::Running | RunState::Awaiting => None,
        };
        awaken_agent_contract::thread::commit::commit_run(
            coordinator.as_ref(),
            &activation.thread_id,
            disposition,
            messages,
            state,
        )
        .await
        .map_err(|error| Error::Commit(error.to_string()))?;

        if let Some(cause) = terminal_cause {
            let terminal = CommittedTerminalRun {
                run_id: activation.run_id.clone(),
                thread_id: activation.thread_id.clone(),
                cause,
            };
            // The shared helper isolates observer failures from the committed
            // remote Run result and permits recovery redelivery.
            let _ = deliver_committed_terminal(&context.terminal_observers, &terminal).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
    use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
    use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
    use awaken_protocol_a2a::Artifact;
    use awaken_protocol_a2a::client::Response;
    use awaken_protocol_a2a::types::{
        Message as A2aMessage, MessageKind, MessageRole, Part as A2aPart, TaskKind, TaskStatus,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_runtime_contract::terminal::{RunTerminalObserver, RunTerminalObserverError};
    use std::sync::Mutex;

    struct ScriptedTransport {
        replies: Mutex<std::collections::VecDeque<std::result::Result<Response, String>>>,
        seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl ScriptedTransport {
        fn new(replies: Vec<std::result::Result<Response, String>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
        ) -> std::result::Result<Response, String> {
            self.seen.lock().unwrap().push((
                method.to_string(),
                path.to_string(),
                body.map(|body| String::from_utf8_lossy(&body).into_owned()),
            ));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("script has a reply")
        }
    }

    fn response(body: &str) -> std::result::Result<Response, String> {
        Ok(Response::new(200, body.as_bytes().to_vec()))
    }

    struct FixedTransportResolver(Arc<dyn Transport>);

    #[async_trait]
    impl TransportResolver for FixedTransportResolver {
        async fn resolve(
            &self,
            _candidate: &ResolvedModelCandidate,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<Arc<dyn Transport>, String> {
            Ok(self.0.clone())
        }
    }

    fn scripted_executor(transport: Arc<ScriptedTransport>) -> A2aRunExecutor {
        A2aRunExecutor::new(Arc::new(FixedTransportResolver(transport)))
    }

    #[derive(Default)]
    struct Rec(Mutex<Vec<ThreadCommit>>);
    #[async_trait]
    impl Coordinator for Rec {
        async fn commit(&self, c: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
            let mut commits = self.0.lock().unwrap();
            commits.push(c);
            Ok(CommitRecord {
                sequence: commits.len() as u64,
            })
        }
    }

    impl ThreadReader for Rec {
        fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|commit| &commit.thread_id == thread_id)
                .flat_map(|commit| commit.messages.clone())
                .collect()
        }

        fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| commit.run.run_id() == run_id)
                .and_then(|commit| commit.run.resume_ticket().cloned())
        }

        fn run_state(&self, run_id: &RunId) -> Option<RunState> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| commit.run.run_id() == run_id)
                .map(|commit| commit.run.state())
        }

        fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|commit| &commit.thread_id == thread_id)
                .flat_map(|commit| commit.state.clone())
                .collect()
        }
    }

    #[derive(Default)]
    struct TerminalRec(Mutex<Vec<CommittedTerminalRun>>);

    #[async_trait]
    impl RunTerminalObserver for TerminalRec {
        fn observer_id(&self) -> &str {
            "a2a-terminal-test"
        }

        async fn observe(
            &self,
            terminal: &CommittedTerminalRun,
        ) -> std::result::Result<(), RunTerminalObserverError> {
            self.0.lock().unwrap().push(terminal.clone());
            Ok(())
        }
    }

    fn activation(backend_ref: &str) -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("t".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("p", "m", backend_ref),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: vec![Message::text(MessageId("u".into()), Role::User, "go")],
            delegation_origin: None,
            model_ref_override: None,
            tool_capability_narrowing: Default::default(),
        }
    }

    #[test]
    fn remote_execution_fails_closed_when_tool_denial_cannot_be_proven() {
        let restricted = activation("a2a:https://agent.example").without_tools();
        let error = ensure_supported_narrowing(&restricted, &RuntimeRunContext::new())
            .expect_err("an opaque remote Agent cannot enforce local tool denial");
        assert!(error.to_string().contains("cannot prove enforcement"));

        let context = RuntimeRunContext::new().with_tool_permission_policy(Arc::new(
            awaken_runtime_contract::permission::DenyAllTools::new("attempt restriction"),
        ));
        assert!(
            ensure_supported_narrowing(&activation("a2a:https://agent.example"), &context).is_err(),
            "process-local narrowing must not be silently ignored either"
        );
    }

    struct RejectingTransportResolver;

    #[async_trait]
    impl TransportResolver for RejectingTransportResolver {
        async fn resolve(
            &self,
            _candidate: &ResolvedModelCandidate,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<Arc<dyn Transport>, String> {
            Err("claim-frozen remote credential is unavailable".into())
        }
    }

    #[tokio::test]
    async fn transport_resolution_failure_never_falls_back_to_anonymous_http() {
        // Cause/effect: C1 snapshot selects Remote; C2 the injected host resolver
        // rejects its claim-frozen transport authority. Effect E1 return the
        // resolver error before any A2A request; forbidden effect E2 is building
        // an anonymous HttpTransport inside the executor.
        //
        // Decision table: C1+!C2 -> E1 and zero wire side effects. The successful
        // C1+C2 path is covered by every scripted transport execution test.
        let error = A2aRunExecutor::new(Arc::new(RejectingTransportResolver))
            .execute(
                activation("a2a:https://agent.example"),
                RuntimeRunContext::new(),
            )
            .await
            .expect_err("transport authority is mandatory");
        assert!(
            error
                .to_string()
                .contains("claim-frozen remote credential is unavailable")
        );
    }

    #[tokio::test]
    async fn a2a_delivers_the_same_post_commit_terminal_extension_contract() {
        let activation = activation("a2a:https://example.test");
        let observer = Arc::new(TerminalRec::default());
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(Rec::default()))
            .with_terminal_observer(observer.clone());

        let state = finish_terminal(&context, &activation, Vec::new(), EndCause::NaturalEnd)
            .await
            .unwrap();

        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            observer.0.lock().unwrap().as_slice(),
            &[CommittedTerminalRun {
                run_id: RunId("r".into()),
                thread_id: ThreadId("t".into()),
                cause: EndCause::NaturalEnd,
            }]
        );
    }

    // ---- pure translation units (prompt_of / task_reply / end_cause_of) ----

    /// A text agent message for the A2A wire shape (helper for task fixtures).
    fn a2a_msg(text: &str) -> A2aMessage {
        A2aMessage {
            kind: MessageKind::Message,
            task_id: None,
            context_id: None,
            message_id: "m".into(),
            role: MessageRole::Agent,
            parts: vec![A2aPart::text(text)],
            extensions: Vec::new(),
            metadata: None,
            reference_task_ids: Vec::new(),
        }
    }

    /// A returned task with the given status message, history, and artifacts (each
    /// artifact is its list of text parts). Every fixture completes; task_reply
    /// selection is what these exercise.
    fn task_with(status_msg: Option<&str>, history: &[&str], artifacts: &[&[&str]]) -> Task {
        Task {
            kind: TaskKind::Task,
            id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: status_msg.map(a2a_msg),
                timestamp: None,
            },
            history: history.iter().map(|t| a2a_msg(t)).collect(),
            artifacts: artifacts
                .iter()
                .map(|parts| Artifact {
                    artifact_id: "artifact".into(),
                    name: None,
                    description: None,
                    extensions: Vec::new(),
                    metadata: None,
                    parts: parts.iter().map(|t| A2aPart::text(*t)).collect(),
                })
                .collect(),
            metadata: None,
        }
    }

    #[test]
    fn prompt_of_joins_inputs_with_newlines_and_drops_empty_text() {
        // Multi-turn input: the non-empty texts are joined by "\n"; a message whose
        // text is empty contributes nothing (not a blank line).
        let input = vec![
            Message::text(MessageId("u0".into()), Role::User, "first"),
            Message::text(MessageId("u1".into()), Role::User, ""),
            Message::text(MessageId("u2".into()), Role::Assistant, "second"),
        ];
        assert_eq!(prompt_of(&input), "first\nsecond");
    }

    #[test]
    fn prompt_of_empty_input_is_the_empty_prompt() {
        assert_eq!(prompt_of(&[]), "");
    }

    #[test]
    fn task_reply_joins_multiple_artifacts_with_newlines() {
        // Two durable artifacts: their texts are the reply, joined by "\n".
        assert_eq!(
            task_reply(&task_with(None, &[], &[&["a1"], &["a2"]])),
            "a1\na2"
        );
    }

    #[test]
    fn task_reply_skips_text_empty_artifacts_and_falls_to_status() {
        // An artifact with no text parts is not a reply → fall to the status message.
        assert_eq!(
            task_reply(&task_with(Some("status body"), &["h"], &[&[]])),
            "status body"
        );
    }

    #[test]
    fn task_reply_prefers_status_message_over_history() {
        // No artifacts, but a non-empty status message wins over the last history msg.
        assert_eq!(
            task_reply(&task_with(Some("status"), &["h0", "h1"], &[])),
            "status"
        );
    }

    #[test]
    fn task_reply_empty_status_text_falls_through_to_last_history() {
        // A status message present but text-empty is not a reply → last history msg.
        assert_eq!(
            task_reply(&task_with(Some(""), &["h0", "last"], &[])),
            "last"
        );
    }

    #[test]
    fn task_reply_of_an_empty_task_is_the_empty_string() {
        // No artifacts, no status message, no history → empty reply (no panic).
        assert_eq!(task_reply(&task_with(None, &[], &[])), "");
    }

    #[test]
    fn end_cause_maps_each_task_state_honestly() {
        // Only a completed remote task is a natural end; the rest must not be
        // projected as success (G26).
        assert_eq!(end_cause_of(&TaskState::Completed), EndCause::NaturalEnd);
        assert_eq!(end_cause_of(&TaskState::Canceled), EndCause::Cancelled);
        assert_eq!(end_cause_of(&TaskState::Working), EndCause::Indeterminate);
        assert_eq!(
            end_cause_of(&TaskState::InputRequired),
            EndCause::Indeterminate
        );
        assert_eq!(
            end_cause_of(&TaskState::AuthRequired),
            EndCause::Indeterminate
        );
        assert!(matches!(
            end_cause_of(&TaskState::Failed),
            EndCause::Error(Failure::Inference { ref code, .. }) if code == "a2a_task_failed"
        ));
    }

    /// Drives the real executor against a **real A2A HTTP server** over a real TCP
    /// socket (no mock transport): `over_http()` dials the endpoint on
    /// `Backend::Remote`, the server answers a real A2A task, and the reply is
    /// committed. The server's fixed reply stands for the external remote agent —
    /// the transport, socket, and parse path are all real.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dials_a_real_http_a2a_server_and_commits_the_reply() {
        const REPLY: &str = r#"{"task":{"kind":"task","id":"t-1","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"a","role":"agent","parts":[{"kind":"text","text":"real remote reply"}]}}}}"#;

        // A real HTTP server on an ephemeral port; a fallback answers the A2A
        // `message:send` POST (the `:` in the path is a matchit param char, so a
        // fallback is simpler than a literal route and just as real).
        let app = axum::Router::new().fallback(|| async { REPLY });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // The production executor dials it over the real `HttpTransport` (ureq).
        let exec = A2aRunExecutor::over_http();
        let rec = Arc::new(Rec::default());
        let state = exec
            .execute(
                activation(&format!("a2a:http://{addr}")),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();

        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        let commits = rec.0.lock().unwrap();
        assert_eq!(
            commits.last().unwrap().messages[0].text_content(),
            "real remote reply"
        );
    }

    #[test]
    fn advertises_remote_abort_and_durable_wait() {
        let caps = A2aRunExecutor::over_http().capabilities();
        assert_eq!(caps.cancellation, Cancellation::RemoteAbort);
        assert_eq!(caps.wait, Wait::Both);
    }

    #[derive(Default)]
    struct FailingRec;
    #[async_trait]
    impl Coordinator for FailingRec {
        async fn commit(&self, _c: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
            Err(CommitError::Rejected("boom".into()))
        }
    }

    /// A real A2A HTTP server answering every `message:send` with `reply`; returns
    /// the `a2a:` backend ref that dials it.
    async fn serve(reply: &'static str) -> String {
        let app = axum::Router::new().fallback(move || async move { reply });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("a2a:http://{addr}")
    }

    #[tokio::test]
    async fn a_non_remote_backend_fails_closed() {
        // Reached without a remote endpoint is a wiring fault → fail closed, no dial.
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation("native"),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            state,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_config"
        ));
        assert_eq!(
            rec.0
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .messages
                .last()
                .unwrap()
                .text_content(),
            "backend is not an A2A endpoint"
        );
    }

    #[tokio::test]
    async fn a_commit_error_propagates() {
        let err = A2aRunExecutor::over_http()
            .execute(
                activation("native"),
                RuntimeRunContext::new().with_commit(Arc::new(FailingRec)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Commit(_)),
            "a rejected commit surfaces as Error::Commit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn artifacts_are_preferred_over_status_and_history() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"status msg"}]}},"artifacts":[{"artifactId":"a1","parts":[{"kind":"text","text":"artifact body"}]}]}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            rec.0.lock().unwrap().last().unwrap().messages[0].text_content(),
            "artifact body"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_last_is_the_final_reply_fallback() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed"},"history":[{"kind":"message","messageId":"h0","role":"agent","parts":[{"kind":"text","text":"first"}]},{"kind":"message","messageId":"h1","role":"agent","parts":[{"kind":"text","text":"last history"}]}]}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            rec.0.lock().unwrap().last().unwrap().messages[0].text_content(),
            "last history"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transport_error_ends_with_a2a_error() {
        // Bind then drop the listener: the port now refuses connections, so the dial
        // fails and the executor ends on a classified a2a_error (not a panic).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&format!("a2a:http://{addr}")),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            state,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_error"
        ));
        assert!(
            rec.0
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .messages
                .last()
                .unwrap()
                .text_content()
                .contains("remote agent error")
        );
    }

    /// A real A2A server that records the inbound request (uri + body) and answers a
    /// completed task; returns the backend ref and the shared capture slot.
    async fn serve_capturing() -> (String, Arc<Mutex<Option<(String, String)>>>) {
        const REPLY: &str = r#"{"task":{"kind":"task","id":"t-1","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"a","role":"agent","parts":[{"kind":"text","text":"ok"}]}}}}"#;
        let cap: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let slot = cap.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let slot = slot.clone();
            async move {
                let uri = req.uri().to_string();
                let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                *slot.lock().unwrap() = Some((uri, String::from_utf8_lossy(&bytes).into_owned()));
                REPLY
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("a2a:http://{addr}"), cap)
    }

    /// The outbound `message:send` carries the A2A correlation fields the remote
    /// agent keys on — a wrong contextId/messageId silently breaks real agents while
    /// a body-ignoring server test still passes, so assert the real serialized wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_outbound_message_send_carries_the_correlation_fields() {
        let (backend, cap) = serve_capturing().await;
        let rec = Arc::new(Rec::default());
        A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();

        let (uri, body) = cap.lock().unwrap().clone().expect("a request was captured");
        assert!(uri.ends_with("/v1/a2a/message:send"), "path: {uri}");
        assert!(
            body.contains(r#""contextId":"t""#),
            "contextId = thread id: {body}"
        );
        assert!(
            body.contains(r#""messageId":"a2a-msg-r""#),
            "messageId = a2a-msg-<run_id>: {body}"
        );
        assert!(
            body.contains(r#""kind":"message""#),
            "message union: {body}"
        );
        assert!(body.contains(r#""role":"user""#), "user role: {body}");
        assert!(body.contains(r#""text":"go""#), "the prompt text: {body}");
        assert!(
            body.contains(r#""agentId":null"#),
            "agentId is null: {body}"
        );
    }

    /// A real A2A server that answers every request with a fixed HTTP status + body.
    async fn serve_status(status: u16, body: &'static str) -> String {
        let code = axum::http::StatusCode::from_u16(status).unwrap();
        let app = axum::Router::new().fallback(move || async move { (code, body) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("a2a:http://{addr}")
    }

    /// A non-2xx HTTP status from the remote is classified `a2a_error` (not a panic),
    /// mirroring the connection-refused test but for a reachable-but-erroring server.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_http_500_ends_with_a2a_error() {
        let backend = serve_status(500, "upstream boom").await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            state,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_error"
        ));
    }

    /// A 2xx whose body is not a valid A2A task fails the parse and is classified
    /// `a2a_error` — a garbage remote reply fails closed rather than panicking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_malformed_2xx_body_ends_with_a2a_error() {
        let backend = serve("this is not a2a json at all").await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            state,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_error"
        ));
    }

    /// With no commit coordinator on the context, `execute` still returns the
    /// terminal state (the commit boundary is a no-op, not a failure).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_commit_coordinator_still_returns_the_terminal_phase() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"ok"}]}}}}"#,
        )
        .await;
        // RuntimeRunContext::new() carries no commit coordinator.
        let state = A2aRunExecutor::over_http()
            .execute(activation(&backend), RuntimeRunContext::new())
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    }

    /// A remote task returned in the `failed` state is an execution fault, not a
    /// natural end — regression against silently projecting a remote failure as
    /// success (G26). The agent's message is still committed for the reader.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_remote_task_ends_in_error_not_success() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"failed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"model exploded"}]}}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            state,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. }))
                if code == "a2a_task_failed"
        ));
        assert_eq!(
            rec.0.lock().unwrap().last().unwrap().messages[0].text_content(),
            "model exploded"
        );
    }

    /// A remote task returned in the `canceled` state ends as a cancellation, not a
    /// natural end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_canceled_remote_task_ends_cancelled() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"canceled"}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    }

    #[tokio::test]
    async fn a_working_remote_task_is_polled_to_its_real_terminal_state() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"working"}}}"#,
            ),
            response(
                r#"{"kind":"task","id":"t","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"finished"}]}}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let state = scripted_executor(transport.clone())
            .execute(
                activation("a2a:http://remote.invalid"),
                RuntimeRunContext::new()
                    .with_commit(rec.clone())
                    .with_reader(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            transport
                .seen
                .lock()
                .unwrap()
                .iter()
                .map(|(_, path, _)| path.as_str())
                .collect::<Vec<_>>(),
            vec!["/v1/a2a/message:send", "/v1/a2a/tasks/t"]
        );
    }

    #[tokio::test]
    async fn replacement_executor_reattaches_by_the_committed_task_id() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"remote-7","contextId":"ctx-7","status":{"state":"working"}}}"#,
            ),
            Err("connection dropped after task creation".to_string()),
            response(
                r#"{"kind":"task","id":"remote-7","contextId":"ctx-7","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"recovered"}]}}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };

        let first = scripted_executor(transport.clone())
            .execute(activation.clone(), context())
            .await;
        assert!(first.is_err(), "the first process loses its poll response");
        assert_eq!(rec.run_state(&activation.run_id), Some(RunState::Running));
        assert_eq!(
            restored_task_reference(&context(), &activation)
                .unwrap()
                .unwrap()
                .task_id,
            "remote-7"
        );

        let state = scripted_executor(transport.clone())
            .execute(activation, context())
            .await
            .expect("replacement reattaches");
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        let paths: Vec<String> = transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/v1/a2a/message:send",
                "/v1/a2a/tasks/remote-7",
                "/v1/a2a/tasks/remote-7"
            ],
            "recovery performs no second message:send"
        );
    }

    #[tokio::test]
    async fn replacement_executor_resumes_input_on_the_committed_context() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"remote-input","contextId":"ctx-input","status":{"state":"input-required","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"which file?"}]}}}}"#,
            ),
            response(
                r#"{"task":{"kind":"task","id":"remote-finished","contextId":"ctx-input","status":{"state":"completed","message":{"kind":"message","messageId":"m2","role":"agent","parts":[{"kind":"text","text":"done"}]}}}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };

        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );
        let ticket = rec
            .resume_ticket(&activation.run_id)
            .expect("durable ticket");
        let command =
            ResumeCommand::from_ticket(&ticket, ResumeResult::Input("README.md".into()), 0);

        assert_eq!(
            scripted_executor(transport.clone())
                .resume(activation, command, context())
                .await
                .expect("replacement resumes"),
            RunState::Ended(EndCause::NaturalEnd)
        );
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let body = seen[1].2.as_deref().expect("resume body");
        assert!(body.contains(r#""contextId":"ctx-input""#));
        assert!(body.contains(r#""text":"README.md""#));
    }

    #[tokio::test]
    async fn durable_cancel_addresses_the_committed_remote_task() {
        let active = r#"{"kind":"task","id":"remote-cancel","contextId":"ctx-cancel","status":{"state":"input-required"}}"#;
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(&format!(r#"{{"task":{active}}}"#)),
            response(active),
            Ok(Response::new(204, Vec::new())),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };
        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );

        scripted_executor(transport.clone())
            .cancel(activation, context())
            .await
            .expect("remote cancellation is delivered");
        let paths: Vec<String> = transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/v1/a2a/message:send",
                "/v1/a2a/tasks/remote-cancel",
                "/v1/a2a/tasks/remote-cancel:cancel"
            ]
        );
    }

    #[tokio::test]
    async fn failed_remote_cancel_is_retryable_against_the_same_committed_task() {
        let active = r#"{"kind":"task","id":"remote-retry","contextId":"ctx-retry","status":{"state":"working"}}"#;
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(&format!(r#"{{"task":{active}}}"#)),
            response(
                r#"{"kind":"task","id":"remote-retry","contextId":"ctx-retry","status":{"state":"input-required"}}"#,
            ),
            response(active),
            Err("remote cancel unavailable".to_string()),
            response(active),
            Ok(Response::new(204, Vec::new())),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };
        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );

        let error = scripted_executor(transport.clone())
            .cancel(activation.clone(), context())
            .await
            .expect_err("an unconfirmed remote cancel must remain retryable");
        assert!(error.to_string().contains("remote cancel unavailable"));
        scripted_executor(transport.clone())
            .cancel(activation, context())
            .await
            .expect("retry addresses the same durable task");

        let paths: Vec<String> = transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/v1/a2a/message:send",
                "/v1/a2a/tasks/remote-retry",
                "/v1/a2a/tasks/remote-retry",
                "/v1/a2a/tasks/remote-retry:cancel",
                "/v1/a2a/tasks/remote-retry",
                "/v1/a2a/tasks/remote-retry:cancel",
            ]
        );
    }

    /// A remote task in `input-required` becomes a durable await boundary. The
    /// executor must also commit the agent's partial message and resume ticket, so a
    /// reader sees the "I need more input" prompt and a replacement can continue it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_input_required_task_commits_the_partial_message() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"input-required","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"which file?"}]}}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Awaiting);
        let commits = rec.0.lock().unwrap();
        assert_eq!(
            commits.last().unwrap().messages[0].text_content(),
            "which file?",
            "the partial 'input-required' prompt is committed for the reader"
        );
        assert!(commits.last().unwrap().run.resume_ticket().is_some());
    }

    /// The `auth-required` twin: a durable external-event await whose partial
    /// message and resume ticket are committed together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_auth_required_task_commits_the_partial_message() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"auth-required","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"please authenticate"}]}}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Awaiting);
        let commits = rec.0.lock().unwrap();
        assert_eq!(
            commits.last().unwrap().messages[0].text_content(),
            "please authenticate",
            "the partial 'auth-required' prompt is committed for the reader"
        );
        assert_eq!(
            commits.last().unwrap().run.resume_ticket().unwrap().reason,
            AwaitReason::ExternalEvent
        );
    }

    #[test]
    fn durable_reference_and_resume_projection_fail_closed() {
        let valid = serde_json::json!({
            "endpoint": "http://remote.invalid",
            "task_id": "task-7",
            "context_id": "ctx-7",
        });
        let reference = decode_task_reference(&valid).unwrap();
        assert_eq!(reference.task_id, "task-7");
        for field in ["endpoint", "task_id", "context_id"] {
            let mut invalid = valid.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(decode_task_reference(&invalid).is_err(), "missing {field}");
        }
        assert!(ensure_endpoint(&reference, "http://other.invalid").is_err());

        assert_eq!(resume_text(&ResumeResult::Input("input".into())), "input");
        assert_eq!(
            resume_text(&ResumeResult::ToolResult(
                awaken_runtime_contract::tool::ToolOutput::ok("call", "tool")
            )),
            "tool"
        );
        assert_eq!(resume_text(&ResumeResult::allow()), "allow");
        assert_eq!(resume_text(&ResumeResult::deny(None)), "deny");
        assert_eq!(
            resume_text(&ResumeResult::deny(Some("because".into()))),
            "because"
        );
    }

    fn resume_command(activation: &RunActivation) -> ResumeCommand {
        ResumeCommand {
            correlation_id: "missing-ticket".into(),
            run_id: activation.run_id.clone(),
            thread_id: activation.thread_id.clone(),
            snapshot_id: activation.snapshot.id.clone(),
            catalog_fingerprint: activation
                .snapshot
                .resolved_spec
                .catalog_fingerprint
                .clone(),
            result: ResumeResult::Input("answer".into()),
            now_ms: 0,
        }
    }

    #[tokio::test]
    async fn restore_skips_unrelated_state_and_honors_a_later_remove() {
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![StateCommand::set(
                Scope::Run,
                MergePolicy::Disjoint,
                "unrelated",
                serde_json::json!(true),
            )],
        )
        .await
        .unwrap();
        assert_eq!(
            restored_task_reference(&context, &activation).unwrap(),
            None
        );

        let reference = TaskReference {
            endpoint: "http://remote.invalid".into(),
            task_id: "task-7".into(),
            context_id: "ctx-7".into(),
        };
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![task_reference_state(&reference)],
        )
        .await
        .unwrap();
        assert_eq!(
            restored_task_reference(&context, &activation).unwrap(),
            Some(reference)
        );
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![clear_task_reference_state()],
        )
        .await
        .unwrap();
        assert_eq!(
            restored_task_reference(&context, &activation).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn resume_requires_reader_ticket_and_durable_task_in_that_order() {
        let activation = activation("a2a:http://remote.invalid");
        let executor = A2aRunExecutor::over_http();
        let command = resume_command(&activation);
        assert!(
            executor
                .resume(
                    activation.clone(),
                    command.clone(),
                    RuntimeRunContext::new()
                )
                .await
                .is_err()
        );

        let empty = Arc::new(Rec::default());
        assert!(
            executor
                .resume(
                    activation.clone(),
                    command,
                    RuntimeRunContext::new().with_reader(empty),
                )
                .await
                .is_err()
        );

        let rec = Arc::new(Rec::default());
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        let mut waiting = task_with(None, &[], &[]);
        waiting.status.state = TaskState::InputRequired;
        let ticket = awaiting_ticket(&activation, &waiting);
        commit_boundary(
            &context,
            &activation,
            RunDisposition::awaiting(ticket.clone()),
            Vec::new(),
            Vec::new(),
        )
        .await
        .unwrap();
        assert!(
            executor
                .resume(
                    activation,
                    ResumeCommand::from_ticket(&ticket, ResumeResult::Input("answer".into()), 0),
                    context,
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn execute_rejects_a_task_pinned_to_another_endpoint() {
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![task_reference_state(&TaskReference {
                endpoint: "http://old.invalid".into(),
                task_id: "task-7".into(),
                context_id: "ctx-7".into(),
            })],
        )
        .await
        .unwrap();

        assert!(
            A2aRunExecutor::over_http()
                .execute(activation, context)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancel_without_a_reference_is_idempotent_and_terminal_is_a_noop() {
        let activation = activation("a2a:http://remote.invalid");
        let empty = Arc::new(Rec::default());
        scripted_executor(Arc::new(ScriptedTransport::new(Vec::new())))
            .cancel(
                activation.clone(),
                RuntimeRunContext::new().with_reader(empty),
            )
            .await
            .unwrap();

        let transport = Arc::new(ScriptedTransport::new(vec![response(
            r#"{"kind":"task","id":"task-7","contextId":"ctx-7","status":{"state":"completed"}}"#,
        )]));
        let rec = Arc::new(Rec::default());
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![task_reference_state(&TaskReference {
                endpoint: "http://remote.invalid".into(),
                task_id: "task-7".into(),
                context_id: "ctx-7".into(),
            })],
        )
        .await
        .unwrap();
        scripted_executor(transport.clone())
            .cancel(activation, context)
            .await
            .unwrap();
        assert_eq!(transport.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_pre_cancelled_poll_commits_a_cancelled_terminal_boundary() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"task-7","contextId":"ctx-7","status":{"state":"working"}}}"#,
            ),
            Ok(Response::new(204, Vec::new())),
        ]));
        let cancellation = awaken_runtime_contract::CancellationToken::new();
        cancellation.cancel();
        let rec = Arc::new(Rec::default());
        let state = scripted_executor(transport)
            .execute(
                activation("a2a:http://remote.invalid"),
                RuntimeRunContext::new()
                    .with_commit(rec.clone())
                    .with_reader(rec)
                    .with_cancellation(cancellation),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    }
}
