//! `Runtime::run`: the one-call embedded entry.
//!
//! It is sugar over `install_catalog` + `execute` for the in-process case; the
//! durable path drives those directly. `install_catalog` is idempotent, so calling
//! `run` repeatedly re-registers the same catalog harmlessly, and the fingerprint
//! gate still holds — the snapshot is resolved against the catalog that was just
//! installed (a mismatched `RunnableConfig` still fails closed).

use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::catalog::RuntimeCatalogInstaller;
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::Runtime;

impl Runtime {
    /// Run one turn of `config` on a fresh thread, returning the resulting phase.
    ///
    /// This is the single-shot entry: it installs the config and executes once. If
    /// the run parks on a tool approval it returns `Phase::Waiting` — use
    /// [`Runtime::run_to_completion`] to answer approvals and drive to a terminal
    /// phase, or for multi-turn (a stable thread).
    pub async fn run(
        &self,
        config: &RunnableConfig,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<Phase, Error> {
        let (_run_id, activation) = self.prepare(config, next_id("thread"), input)?;
        self.execute(activation, context).await
    }

    /// Run one turn of `config` on `thread`, driving it to a terminal phase and
    /// asking `decide` for the answer each time it parks on a tool approval.
    ///
    /// This owns the `execute → (park → decide → resume)* → end` loop, so callers
    /// never build activations, generate ids, or assemble resume commands. `decide`
    /// is the in-process twin of the durable queue's out-of-band decision delivery:
    /// it sees the [`WaitingTicket`] (what is asked) and returns a [`ResumeResult`]
    /// (the answer). Pass a stable `thread` across turns for a multi-turn
    /// conversation. The context must carry a history reader to resume.
    pub async fn run_to_completion<F>(
        &self,
        config: &RunnableConfig,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
        mut decide: F,
    ) -> Result<Phase, Error>
    where
        F: FnMut(&WaitingTicket) -> ResumeResult,
    {
        let (run_id, activation) = self.prepare(config, thread.into(), input)?;
        let mut phase = self.execute(activation, context.clone()).await?;
        while phase == Phase::Waiting {
            let reader = context.reader.as_deref().ok_or_else(|| {
                Error::Execution("resuming a parked run needs a history reader".to_string())
            })?;
            let ticket = reader
                .waiting_ticket(&run_id)
                .ok_or_else(|| Error::Execution("waiting run has no ticket".to_string()))?;
            let command = ResumeCommand::from_ticket(&ticket, decide(&ticket), 0);
            phase = self.resume(command, reader, context.clone()).await?;
        }
        Ok(phase)
    }

    /// Start one turn of `config` on `thread` and run it to its first pause or end,
    /// returning the run id and phase. Unlike [`Runtime::run_to_completion`], it
    /// does not answer a park: it returns `Phase::Waiting` so a durable caller
    /// (HITL, an out-of-band client) can read the [`WaitingTicket`] and later
    /// [`Runtime::resume`] the run by id. This is the public durable-park twin of
    /// `run_to_completion` (ADR-0033); the caller owns the park→resume loop.
    pub async fn start_turn(
        &self,
        config: &RunnableConfig,
        thread: impl Into<String>,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<(RunId, Phase), Error> {
        let (run_id, activation) = self.prepare(config, thread.into(), input)?;
        let phase = self.execute(activation, context).await?;
        Ok((run_id, phase))
    }

    /// Install the config's catalog and register its snapshot (both idempotent),
    /// then build a fresh activation on `thread` — the shared prefix of `run` and
    /// `run_to_completion`. Registering the snapshot lets a resume resolve it by id.
    fn prepare(
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
            trace: Default::default(),
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
