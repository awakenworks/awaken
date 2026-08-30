//! `Runtime::run`: the one-call embedded entry.
//!
//! Every entry consumes the same immutable `ExecutableAgentSnapshot`; embedded and
//! durable delivery differ only in transport and lifecycle ownership.

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::fresh_process_id;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_runtime_contract::terminal::{
    CommittedTerminalProjection, committed_terminal_projection, deliver_committed_terminal,
};

use crate::Runtime;

impl Runtime {
    /// Run `snapshot` once on a fresh thread, returning the resulting state.
    ///
    /// This is the single-shot entry: it registers the snapshot and executes once. If
    /// the run awaits on a tool approval it returns `RunState::Awaiting` — use
    /// [`Runtime::run_to_completion`] to answer approvals and drive to a terminal
    /// state, or for multi-run (a stable thread).
    pub async fn run(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<RunState, Error> {
        let (_run_id, activation) = self.prepare(snapshot, fresh_process_id("thread"), input);
        self.execute(activation, context).await
    }

    /// Run `snapshot` once on `thread`, driving it to a terminal state and
    /// asking `decide` for the answer each time it awaits on a tool approval.
    ///
    /// This owns the `execute → (await → decide → resume)* → end` loop, so callers
    /// never build activations, generate ids, or assemble resume commands. `decide`
    /// is the in-process twin of the durable queue's out-of-band decision delivery:
    /// it sees the [`ResumeTicket`] (what is asked) and returns a [`ResumeResult`]
    /// (the answer). Pass a stable `thread` across runs for a multi-run
    /// conversation. The context must carry a history reader to resume.
    pub async fn run_to_completion<F>(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
        mut decide: F,
    ) -> Result<RunState, Error>
    where
        F: FnMut(&ResumeTicket) -> ResumeResult,
    {
        let (run_id, activation) = self.prepare(snapshot, thread.into(), input);
        self.drive_to_completion(run_id, activation, context, &mut decide)
            .await
    }

    /// Drive an embedded Run to completion under a caller-supplied durable
    /// identity. Durable dispatch normally carries an explicit [`RunActivation`]
    /// through `RunAttemptExecutor`; this convenience remains for embedded callers.
    pub async fn run_to_completion_with_id<F>(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        run_id: RunId,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
        mut decide: F,
    ) -> Result<RunState, Error>
    where
        F: FnMut(&ResumeTicket) -> ResumeResult,
    {
        let activation = RunActivation::new(
            run_id.clone(),
            ThreadId(thread.into()),
            snapshot.clone(),
            input.into().0,
        );
        self.drive_to_completion(run_id, activation, context, &mut decide)
            .await
    }

    async fn drive_to_completion<F>(
        &self,
        run_id: RunId,
        activation: RunActivation,
        context: RuntimeRunContext,
        decide: &mut F,
    ) -> Result<RunState, Error>
    where
        F: FnMut(&ResumeTicket) -> ResumeResult,
    {
        // Stable-id entry is idempotent: an existing Ended run is returned, an
        // Awaiting run resumes through its committed ticket, and only a missing or
        // orphan Running run enters execution/recovery. No synthetic "continue"
        // message is added to the transcript.
        let terminal = context
            .reader
            .as_deref()
            .map(|reader| committed_terminal_projection(reader, &run_id, &activation.thread_id));
        let mut state = match terminal {
            Some(CommittedTerminalProjection::Exact(terminal)) => {
                let state = RunState::Ended(terminal.cause.clone());
                let _ = deliver_committed_terminal(&context.terminal_observers, &terminal).await;
                return Ok(state);
            }
            Some(CommittedTerminalProjection::IdentityConflict) => {
                return Err(Error::Execution(
                    "stable Run identity belongs to another Thread".to_string(),
                ));
            }
            Some(CommittedTerminalProjection::Nonterminal) | None => match context
                .reader
                .as_deref()
                .and_then(|reader| reader.run_state(&run_id))
            {
                Some(RunState::Awaiting) => RunState::Awaiting,
                Some(RunState::Running) | None => self.execute(activation, context.clone()).await?,
                Some(RunState::Ended(_)) => {
                    return Err(Error::Execution(
                        "committed Run changed while projecting terminal identity".to_string(),
                    ));
                }
            },
        };
        while state == RunState::Awaiting {
            let reader = context.reader.as_deref().ok_or_else(|| {
                Error::Execution("resuming an awaiting run needs a history reader".to_string())
            })?;
            let ticket = reader
                .resume_ticket(&run_id)
                .ok_or_else(|| Error::Execution("awaiting run has no ticket".to_string()))?;
            let command = ResumeCommand::from_ticket(&ticket, decide(&ticket), 0);
            state = self.resume(command, reader, context.clone()).await?;
        }
        Ok(state)
    }

    /// Start one run of `snapshot` on `thread` and drive it to its first pause or end,
    /// returning the run id and state. Unlike [`Runtime::run_to_completion`], it
    /// does not answer an await: it returns `RunState::Awaiting` so a durable caller
    /// (HITL, an out-of-band client) can read the [`ResumeTicket`] and later
    /// [`Runtime::resume`] the run by id. This is the public durable-await twin of
    /// `run_to_completion` (ADR-0033); the caller owns the await→resume loop.
    pub async fn start_run(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<(RunId, RunState), Error> {
        let (run_id, activation) = self.prepare(snapshot, thread.into(), input);
        let state = self.execute(activation, context).await?;
        Ok((run_id, state))
    }

    /// Build a fresh activation on `thread`. The execution ingress retains its
    /// immutable snapshot before the run can become resumable.
    pub fn prepare(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        thread: String,
        input: impl Into<RunInput>,
    ) -> (RunId, RunActivation) {
        let run_id = RunId(next_run_id());
        let activation = RunActivation {
            run_id: run_id.clone(),
            thread_id: ThreadId(thread),
            snapshot: snapshot.clone(),
            input: input.into().0,
            delegation_origin: None,
            model_ref_override: None,
            data_subject_id: None,
            tool_capability_narrowing: Default::default(),
        };
        (run_id, activation)
    }
}

/// The input to one [`Runtime::run`]: a string becomes a single user message;
/// explicit messages pass through unchanged.
pub struct RunInput(Vec<Message>);

impl From<&str> for RunInput {
    fn from(text: &str) -> Self {
        Self(vec![user_message(text)])
    }
}

impl From<String> for RunInput {
    fn from(text: String) -> Self {
        Self(vec![user_message(text)])
    }
}

impl From<Message> for RunInput {
    fn from(message: Message) -> Self {
        Self(vec![message])
    }
}

impl From<Vec<Message>> for RunInput {
    fn from(messages: Vec<Message>) -> Self {
        Self(messages)
    }
}

fn user_message(text: impl Into<String>) -> Message {
    Message {
        id: MessageId(fresh_process_id("msg")),
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }
}

/// A fresh Run id must remain unique across process restarts because mutating tools
/// derive their durable operation identity from it. Recovered/dispatched Runs supply
/// their already-persisted explicit id and never pass through this generator.
fn next_run_id() -> String {
    fresh_process_id("run")
}
