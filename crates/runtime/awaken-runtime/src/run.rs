// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: This source implements the repository-controlled agent loop product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
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
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

use crate::Runtime;

impl Runtime {
    /// Bind a Snapshot, Thread and store once for an embedded conversation.
    pub fn session<'a>(
        &'a self,
        snapshot: &'a ExecutableAgentSnapshot,
        thread: impl Into<String>,
        context: RuntimeRunContext,
    ) -> Result<crate::EmbeddedSession<'a>, Error> {
        crate::EmbeddedSession::new(self, snapshot, ThreadId(thread.into()), context)
    }

    /// Single-shot entry returning a named projection of the committed record.
    pub async fn run_with_outcome(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<crate::RunOutcome, Error> {
        self.session(snapshot, fresh_process_id("thread"), context)?
            .run(input)
            .await
    }

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
        crate::configuration::validate_for(self, snapshot, &context)?;
        let (_run_id, activation) =
            self.prepare_with_context(snapshot, fresh_process_id("thread"), input, &context);
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
        let (run_id, activation) =
            self.prepare_with_context(snapshot, thread.into(), input, &context);
        crate::approval::drive_to_completion(
            self,
            run_id,
            activation,
            context,
            &mut |ticket| std::future::ready(Ok(decide(&ticket))),
            &crate::ApprovalOptions::default(),
        )
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
        )
        .with_inherited_toolsets(context.inherited_toolsets.clone());
        crate::approval::drive_to_completion(
            self,
            run_id,
            activation,
            context,
            &mut |ticket| std::future::ready(Ok(decide(&ticket))),
            &crate::ApprovalOptions::default(),
        )
        .await
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
        crate::configuration::validate_for(self, snapshot, &context)?;
        let (run_id, activation) =
            self.prepare_with_context(snapshot, thread.into(), input, &context);
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
        let activation = RunActivation::new(
            run_id.clone(),
            ThreadId(thread),
            snapshot.clone(),
            input.into().0,
        );
        (run_id, activation)
    }

    /// The convenience entries freeze the caller's execution input before the
    /// process-local context is consumed. Explicit dispatch builders may use
    /// `prepare` and set the same value when they hold a frozen authority.
    pub(crate) fn prepare_with_context(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        thread: String,
        input: impl Into<RunInput>,
        context: &RuntimeRunContext,
    ) -> (RunId, RunActivation) {
        let (run_id, activation) = self.prepare(snapshot, thread, input);
        (
            run_id,
            activation.with_inherited_toolsets(context.inherited_toolsets.clone()),
        )
    }
}

/// The input to one [`Runtime::run`]: a string becomes a single user message;
/// explicit messages pass through unchanged.
pub struct RunInput(pub(crate) Vec<Message>);

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
