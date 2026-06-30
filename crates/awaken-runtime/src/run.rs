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
use awaken_runtime_contract::activation::{RunActivation, RunOptions};
use awaken_runtime_contract::catalog::RuntimeCatalogInstaller;
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::Runtime;

impl Runtime {
    /// Run one turn of `config` against `context`, returning the resulting phase.
    ///
    /// Installs the config's catalog (idempotent) and executes a fresh run on a new
    /// thread carrying `input`. For multi-turn or durable execution, install the
    /// catalog once and drive `execute`/`resume` directly.
    pub async fn run(
        &self,
        config: &RunnableConfig,
        input: impl Into<RunInput>,
        context: RuntimeRunContext,
    ) -> Result<Phase, Error> {
        self.install_catalog(config.install().clone())
            .map_err(|err| Error::Execution(err.to_string()))?;
        let activation = RunActivation {
            run_id: RunId(next_id("run")),
            thread_id: ThreadId(next_id("thread")),
            snapshot: config.snapshot().clone(),
            input: input.into().0,
            options: RunOptions::default(),
            trace: Default::default(),
        };
        self.execute(activation, context).await
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
