//! `Runtime::run`: the one-call embedded entry.
//!
//! It is sugar over `install_catalog` + `execute` for the in-process case; the
//! durable path drives those directly. `install_catalog` is idempotent, so calling
//! `run` repeatedly re-registers the same catalog harmlessly, and the fingerprint
//! gate still holds — the snapshot is resolved against the catalog that was just
//! installed (a mismatched `RunnableConfig` still fails closed).

use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::catalog::RuntimeCatalogInstaller;
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::Runtime;

impl Runtime {
    /// Run `config` once on a fresh thread, returning the resulting state.
    ///
    /// This is the single-shot entry: it installs the config and executes once. If
    /// the run awaits on a tool approval it returns `RunState::Awaiting` — use
    /// [`Runtime::run_to_completion`] to answer approvals and drive to a terminal
    /// state, or for multi-run (a stable thread).
    pub async fn run(
        &self,
        config: &RunnableConfig,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<RunState, Error> {
        let (_run_id, activation) = self.prepare(config, next_id("thread"), input)?;
        self.execute(activation, context).await
    }

    /// Run `config` once on `thread`, driving it to a terminal state and
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
        config: &RunnableConfig,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
        mut decide: F,
    ) -> Result<RunState, Error>
    where
        F: FnMut(&ResumeTicket) -> ResumeResult,
    {
        let (run_id, activation) = self.prepare(config, thread.into(), input)?;
        self.drive_to_completion(run_id, activation, context, &mut decide)
            .await
    }

    /// Drive a Run to completion under a caller-supplied durable identity.
    /// Delegation uses this entry so the child identity committed by the parent
    /// is the identity executed by the child Runtime, including after a retry.
    pub async fn run_to_completion_with_id<F>(
        &self,
        config: &RunnableConfig,
        run_id: RunId,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
        mut decide: F,
    ) -> Result<RunState, Error>
    where
        F: FnMut(&ResumeTicket) -> ResumeResult,
    {
        self.install_catalog(config.install().clone())
            .map_err(|err| Error::Execution(err.to_string()))?;
        self.register_snapshot(config.snapshot().clone());
        let activation = RunActivation::new(
            run_id.clone(),
            ThreadId(thread.into()),
            config.snapshot().clone(),
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
        let mut state = self.execute(activation, context.clone()).await?;
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

    /// Start one run of `config` on `thread` and drive it to its first pause or end,
    /// returning the run id and state. Unlike [`Runtime::run_to_completion`], it
    /// does not answer an await: it returns `RunState::Awaiting` so a durable caller
    /// (HITL, an out-of-band client) can read the [`ResumeTicket`] and later
    /// [`Runtime::resume`] the run by id. This is the public durable-await twin of
    /// `run_to_completion` (ADR-0033); the caller owns the await→resume loop.
    pub async fn start_run(
        &self,
        config: &RunnableConfig,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<(RunId, RunState), Error> {
        let (run_id, activation) = self.prepare(config, thread.into(), input)?;
        let state = self.execute(activation, context).await?;
        Ok((run_id, state))
    }

    /// Idempotently install a config's catalog and register its snapshot, so a run
    /// awaiting under this config can be resumed without a prior `start_run` — e.g.
    /// after a restart, when a session is rebuilt from a durable store and the
    /// awaiting ticket's snapshot must resolve. Safe to call repeatedly.
    pub fn install_for_resume(&self, config: &RunnableConfig) -> Result<(), Error> {
        self.install_catalog(config.install().clone())
            .map_err(|err| Error::Execution(err.to_string()))?;
        self.register_snapshot(config.snapshot().clone());
        Ok(())
    }

    /// Install the config's catalog and register its snapshot (both idempotent),
    /// then build a fresh activation on `thread` — the shared prefix of `run` and
    /// `run_to_completion`. Registering the snapshot lets a resume resolve it by id.
    pub fn prepare(
        &self,
        config: &RunnableConfig,
        thread: String,
        input: impl Into<RunInput>,
    ) -> Result<(RunId, RunActivation), Error> {
        self.install_catalog(config.install().clone())
            .map_err(|err| Error::Execution(err.to_string()))?;
        self.register_snapshot(config.snapshot().clone());
        let run_id = RunId(next_id("run"));
        let activation = RunActivation {
            run_id: run_id.clone(),
            thread_id: ThreadId(thread),
            snapshot: config.snapshot().clone(),
            input: input.into().0,
            model_ref_override: None,
        };
        Ok((run_id, activation))
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
