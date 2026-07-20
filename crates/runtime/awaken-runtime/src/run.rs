//! `Runtime::run`: the one-call embedded entry.
//!
//! Every entry consumes the same immutable `ExecutableAgentSnapshot`; embedded and
//! durable delivery differ only in transport and lifecycle ownership.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

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
        let (_run_id, activation) = self.prepare(snapshot, next_id("thread"), input);
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

    /// Drive a Run to completion under a caller-supplied durable identity.
    /// Delegation uses this entry so the child identity committed by the parent
    /// is the identity executed by the child Runtime, including after a retry.
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
        self.run_with_identity(snapshot, run_id, thread, input, context, None, &mut decide)
            .await
    }

    /// Drive a delegated child through the ordinary lifecycle under its stable
    /// Run id while retaining the tool-call origin in durable activation data.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_delegated_to_completion<F>(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        run_id: RunId,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
        delegation_origin: DelegationOrigin,
        mut decide: F,
    ) -> Result<RunState, Error>
    where
        F: FnMut(&ResumeTicket) -> ResumeResult,
    {
        self.run_with_identity(
            snapshot,
            run_id,
            thread,
            input,
            context,
            Some(delegation_origin),
            &mut decide,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_with_identity<F>(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        run_id: RunId,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
        delegation_origin: Option<DelegationOrigin>,
        decide: &mut F,
    ) -> Result<RunState, Error>
    where
        F: FnMut(&ResumeTicket) -> ResumeResult,
    {
        let mut activation = RunActivation::new(
            run_id.clone(),
            ThreadId(thread.into()),
            snapshot.clone(),
            input.into().0,
        );
        activation.delegation_origin = delegation_origin;
        self.drive_to_completion(run_id, activation, context, decide)
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
        let mut state = match context
            .reader
            .as_deref()
            .and_then(|reader| reader.run_state(&run_id))
        {
            Some(state @ RunState::Ended(_)) => return Ok(state),
            Some(RunState::Awaiting) => RunState::Awaiting,
            Some(RunState::Running) | None => self.execute(activation, context.clone()).await?,
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
        id: MessageId(next_id("msg")),
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }
}

/// A process-unique id with a readable prefix — enough for in-process ergonomic
/// runs. The durable path supplies explicit, stable ids instead.
fn next_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// A fresh Run id must remain unique across process restarts because mutating tools
/// derive their durable operation identity from it. Recovered/dispatched Runs supply
/// their already-persisted explicit id and never pass through this generator.
fn next_run_id() -> String {
    static PROCESS_NAMESPACE: OnceLock<String> = OnceLock::new();
    static RUN_COUNTER: AtomicU64 = AtomicU64::new(1);
    let namespace = PROCESS_NAMESPACE.get_or_init(|| {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        format!("{}-{started:x}", std::process::id())
    });
    let sequence = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("run-{namespace}-{sequence}")
}
