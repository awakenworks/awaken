// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: SDK protocol composition drives the existing Runtime and reads
// @formal reason: committed Run records; it owns no alternate lifecycle or Sandbox.
//! In-process conversation Runs for direct SDK and HTTP protocol composition.
//!
//! Admission keys are process-local. Committed Run state, pending tickets and
//! history always come from the supplied Runtime store. Durable admission across
//! process replacement belongs to a hosted composition implementing `OwnedRuns`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::sink::Sink;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::{ExecutableAgentSnapshot, Runtime, RuntimeRunContext};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runs::{Pending, RunError, RunResume, Runs, StepOutcome};
use awaken_sync::LeafLock;
use awaken_tool_contract::tool::ToolOutput;

fn execution_error(error: awaken_runtime::Error) -> RunError {
    use awaken_runtime_contract::diagnostic::RuntimeErrorKind;
    match error.kind() {
        RuntimeErrorKind::InvalidConfiguration | RuntimeErrorKind::InvalidRequest => {
            RunError::bad_request_classified(error.code(), error.to_string())
        }
        RuntimeErrorKind::Conflict => {
            RunError::conflict_classified(error.code(), error.to_string())
        }
        RuntimeErrorKind::Unavailable => {
            RunError::unavailable_classified(error.code(), error.to_string())
        }
        _ => RunError::classified(error.code(), error.to_string()),
    }
}

struct Admission {
    input: String,
    run: RunId,
    start: usize,
    end: Option<usize>,
}

#[derive(Default)]
struct ThreadAdmissions {
    receipts: HashMap<(bool, String), Admission>,
}

impl ThreadAdmissions {
    fn observe_boundary(&mut self, run: &RunId, end: usize) {
        // A resume extends the original Run's committed projection. Every
        // receipt for that Run must see the extension, while later Runs keep
        // their own message interval. Lifecycle truth still comes from Runtime.
        for receipt in self
            .receipts
            .values_mut()
            .filter(|receipt| receipt.run == *run)
        {
            receipt.end = Some(end);
        }
    }
}

/// One selected Agent and Runtime exposed as ordinary conversation Runs.
/// No Session, deployment, credential broker or Sandbox is constructed.
pub struct ConversationRuns {
    runtime: Arc<Runtime>,
    snapshot: ExecutableAgentSnapshot,
    context: RuntimeRunContext,
    reader: Arc<dyn CommittedThreadView>,
    threads: LeafLock<HashMap<String, Arc<tokio::sync::Mutex<ThreadAdmissions>>>>,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl ConversationRuns {
    /// Use the same host-selected clock as the Runtime and its committed store.
    pub fn new(
        runtime: Arc<Runtime>,
        snapshot: ExecutableAgentSnapshot,
        context: RuntimeRunContext,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, RunError> {
        runtime
            .conversation(&snapshot, "configuration", context.clone())
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        let reader = context.reader.clone().ok_or_else(|| {
            RunError::bad_request("conversation requires a committed history reader")
        })?;
        runtime.register_snapshot(snapshot.clone());
        Ok(Self {
            runtime,
            snapshot,
            context,
            reader,
            threads: LeafLock::new(HashMap::new()),
            clock,
        })
    }

    fn thread(&self, thread: &str) -> Result<Arc<tokio::sync::Mutex<ThreadAdmissions>>, RunError> {
        if thread.trim().is_empty() {
            return Err(RunError::bad_request(
                "conversation Thread must be nonempty",
            ));
        }
        Ok(self
            .threads
            .lock()
            .entry(thread.into())
            .or_default()
            .clone())
    }

    fn project(&self, thread: &ThreadId, admission: &Admission) -> Result<StepOutcome, RunError> {
        let record = self
            .reader
            .run(&admission.run)
            .ok_or_else(|| RunError::conflict("the admitted Run has no committed outcome"))?;
        if record.thread_id != *thread {
            return Err(RunError::conflict(
                "the admitted Run belongs to another Thread",
            ));
        }
        let messages = self.reader.committed_messages(thread);
        let end = admission.end.unwrap_or(messages.len());
        let messages = messages
            .get(admission.start..end)
            .ok_or_else(|| RunError::conflict("committed conversation prefix changed"))?
            .to_vec();
        let outcome = match record.state {
            RunState::Awaiting => {
                let ticket = self
                    .reader
                    .resume_ticket(&record.id)
                    .ok_or_else(|| RunError::conflict("Awaiting Run has no committed ticket"))?;
                if ticket.run_id != record.id || ticket.thread_id != *thread {
                    return Err(RunError::conflict("resume ticket identity changed"));
                }
                StepOutcome::awaiting(messages, Pending::from_resume_ticket(&ticket))
                    .with_await_reason(ticket.target().reason())
            }
            RunState::Ended(cause) => StepOutcome::ended(messages, cause),
            RunState::Running => {
                return Err(RunError::conflict("the admitted Run is still running"));
            }
        };
        Ok(outcome.with_run_id(record.id))
    }

    async fn start(
        &self,
        operation: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Option<Arc<dyn Sink>>,
    ) -> Result<StepOutcome, RunError> {
        if operation.trim().is_empty() {
            return Err(RunError::bad_request("Run operation must be nonempty"));
        }
        if agent
            .as_ref()
            .is_some_and(|agent| agent != &self.snapshot.root_agent_id.0)
        {
            return Err(RunError::bad_request_classified(
                "not_found",
                "Agent is not selected by this conversation host",
            ));
        }
        let input = serde_json::to_string(&(&self.snapshot.root_agent_id, &messages))
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        let thread_id = ThreadId(thread.into());
        let state = self.thread(thread)?;
        let mut state = state.lock().await;
        let key = (false, operation.into());
        if let Some(admission) = state.receipts.get(&key) {
            if admission.input != input {
                return Err(RunError::conflict(
                    "Run operation was reused with different input",
                ));
            }
            return self.project(&thread_id, admission);
        }
        if self.reader.open_wait_for_thread(&thread_id).is_some() {
            return Err(RunError::conflict(
                "the conversation awaits a matching resume",
            ));
        }
        let (run, activation) = self
            .runtime
            .prepare(&self.snapshot, thread.into(), messages);
        let activation =
            activation.with_inherited_toolsets(self.context.inherited_toolsets.clone());
        state.receipts.insert(
            key.clone(),
            Admission {
                input,
                run: run.clone(),
                start: self.reader.committed_messages(&thread_id).len(),
                end: None,
            },
        );
        let mut context = self.context.clone();
        context.stream_sink = sink;
        awaken_runtime::DirectAttemptDriver::new(self.runtime.clone())
            .start(activation, context)
            .await
            .map_err(execution_error)?;
        state.observe_boundary(&run, self.reader.committed_messages(&thread_id).len());
        let admission = state.receipts.get(&key).expect("the held admission exists");
        self.project(&thread_id, admission)
    }

    async fn answer(
        &self,
        operation: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Option<Arc<dyn Sink>>,
    ) -> Result<StepOutcome, RunError> {
        if operation.trim().is_empty() {
            return Err(RunError::bad_request("resume operation must be nonempty"));
        }
        let result = match resume {
            RunResume::Permission(decision) => ResumeResult::Permission(decision),
            RunResume::ClientResult { content, is_error } => ResumeResult::ToolResult(ToolOutput {
                call_id: tool_use_id.into(),
                content,
                is_error,
                state: Vec::new(),
            }),
        };
        let input = serde_json::to_string(&(tool_use_id, &result))
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        let thread_id = ThreadId(thread.into());
        let state = self.thread(thread)?;
        let mut state = state.lock().await;
        let key = (true, operation.into());
        if let Some(admission) = state.receipts.get(&key) {
            if admission.input != input {
                return Err(RunError::conflict(
                    "resume operation was reused with different input",
                ));
            }
            return self.project(&thread_id, admission);
        }
        let (run, ticket) = self
            .reader
            .open_wait_for_thread(&thread_id)
            .ok_or_else(|| RunError::conflict("the conversation has no open wait"))?;
        let pending = Pending::from_resume_ticket(&ticket)
            .ok_or_else(|| RunError::bad_request("the wait cannot be answered by this protocol"))?;
        if pending.tool_use_id != tool_use_id {
            return Err(RunError::bad_request(
                "the tool result does not match the committed wait",
            ));
        }
        let result = match (ticket.target(), result) {
            (
                awaken_agent_contract::agent::awaiting::AwaitTarget::RemoteInput { .. },
                ResumeResult::ToolResult(output),
            ) => {
                if output.is_error {
                    return Err(RunError::bad_request(
                        "remote input must be an answer, not a failed tool result",
                    ));
                }
                ResumeResult::Input(awaken_runtime_contract::runs::blocks_text(&output.content))
            }
            (_, result) => result,
        };
        let now = self.clock.now().as_epoch_ms();
        let command = ResumeCommand::from_ticket(&ticket, result, now);
        state.receipts.insert(
            key.clone(),
            Admission {
                input,
                run: run.clone(),
                start: self.reader.committed_messages(&thread_id).len(),
                end: None,
            },
        );
        let mut context = self.context.clone();
        context.stream_sink = sink;
        self.runtime
            .resume(command, self.reader.as_ref(), context)
            .await
            .map_err(execution_error)?;
        state.observe_boundary(&run, self.reader.committed_messages(&thread_id).len());
        let admission = state.receipts.get(&key).expect("the held admission exists");
        self.project(&thread_id, admission)
    }
}

#[async_trait]
impl Runs for ConversationRuns {
    async fn run(
        &self,
        operation: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        self.start(operation, thread, agent, messages, None).await
    }
    async fn run_streaming(
        &self,
        operation: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.start(operation, thread, agent, messages, Some(sink))
            .await
    }
    async fn resume(
        &self,
        operation: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        self.answer(operation, thread, tool_use_id, resume, None)
            .await
    }
    async fn resume_streaming(
        &self,
        operation: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Arc<dyn Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.answer(operation, thread, tool_use_id, resume, Some(sink))
            .await
    }
    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        let thread = ThreadId(thread.into());
        if let Some(run) = self.runtime.active_attempt_run_id(&thread).await {
            awaken_runtime::DirectAttemptDriver::new(self.runtime.clone())
                .cancel(&run)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
        } else if let Some((run, _)) = self.reader.open_wait_for_thread(&thread) {
            self.runtime
                .cancel_run(run, thread, self.context.clone())
                .await
                .map_err(execution_error)?;
        }
        Ok(())
    }
    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(self
            .reader
            .open_wait_for_thread(&ThreadId(thread.into()))
            .and_then(|(_, ticket)| Pending::from_resume_ticket(&ticket)))
    }
    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(self.reader.committed_messages(&ThreadId(thread.into())))
    }
    async fn usage(&self, thread: &str) -> Result<(u64, u64), RunError> {
        let usage = awaken_agent_contract::thread::usage::ThreadUsage::from_committed_state(
            &self.reader.committed_state(&ThreadId(thread.into())),
        )
        .total();
        Ok((usage.prompt_tokens, usage.completion_tokens))
    }
    fn model(&self) -> String {
        self.snapshot
            .resolved_spec
            .model_binding
            .binding()
            .identity()
            .1
            .to_owned()
    }
}
