//! `AcpRunExecutor`: an external ACP agent (Claude Code / Codex / …) as a peer
//! [`RunExecutor`].
//!
//! `Runtime` is the *native* [`RunExecutor`] (its own model/tool loop). This is a
//! *second* implementation whose brain is an opaque ACP CLI driven over a duplex
//! [`AgentChannel`] by the [`Supervisor`]. Because both are `RunExecutor`s, an
//! ACP-backed agent is reachable over every wire adapter exactly like a native
//! one — no separate "brain" abstraction is introduced (the driver *is* the
//! runtime).
//!
//! Boundaries (ADR-0043 D6/D9): this crate lives in the runtime plane and depends
//! only on foundation (contracts) + provisioning (channel) + the ACP protocol
//! crate. It never resolves config or names a secret — the host opens the channel
//! (local sandbox launch or remote dial) via an injected [`AgentChannelSource`]
//! and hands this executor an already-launched agent.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::run::{EndCause, Failure, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_protocol_acp::{
    AcpError, AcpFailure, AgentEvent, AppendError, Injection, LaunchSink, RawAcpError,
    RunFactAppender, Stage, SupervisePolicy, Supervisor, TerminationReason, classify_error,
};
// Re-exported (not just `use`d) so a host composition root selects the wire and
// observes agent bring-up without a direct dependency on the protocol crate. The
// executor also uses these names internally to emit lifecycle events.
pub use awaken_protocol_acp::{AcpLaunchEvent, AcpLaunchStage, Codec, LaunchObserver};
use awaken_provisioning_contract::ProcessHandle;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result, RunExecutor};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

/// An already-launched ACP agent: the duplex channel plus the process handle for
/// reaping. The host produces this — locally by launching the CLI into a sandbox,
/// remotely by dialing an intermediary — so this executor stays transport- and
/// provisioning-agnostic (it only drives a channel).
pub struct AgentSession {
    pub channel: Box<dyn AgentChannel>,
    pub process: Arc<dyn ProcessHandle>,
    /// Which wire to speak to this agent. The source declares it: a fixture/test
    /// agent speaks the newline stand-in ([`Codec::Newline`], the default); a real
    /// CLI opened by [`ProjectingChannelSource`] speaks official ACP JSON-RPC.
    pub codec: Codec,
}

/// Opens an [`AgentSession`] for a run. The one seam the host wires: local =
/// sandbox launch (provisioning), remote = connection dial. Injected so the
/// executor never depends on either directly.
#[async_trait]
pub trait AgentChannelSource: Send + Sync {
    async fn open(
        &self,
        activation: &RunActivation,
    ) -> std::result::Result<AgentSession, OpenError>;
}

/// Why opening the agent channel failed (launch/config/dial fault).
#[derive(Debug, thiserror::Error)]
#[error("agent channel open failed: {0}")]
pub struct OpenError(pub String);

/// How a backend handles a mid-conversation model switch (R7). Reported so the
/// host can gate an override fail-closed against a backend that cannot honor it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwitch {
    /// Native: each turn resolves its own executor; switching is free (O(1)).
    FreePerTurn,
    /// ACP CLI: a live session is a process, so switching relaunches it with the
    /// new env. Correct but not free — a fresh channel per turn.
    Relaunch,
    /// The backend cannot switch mid-conversation; a per-turn override must fail.
    Unsupported,
}

/// Drives an external ACP agent as a [`RunExecutor`].
///
/// Each [`execute`](RunExecutor::execute) opens a fresh channel via the source, so
/// an ACP thread relaunches its CLI every turn — which is exactly how a model
/// switch takes effect (R7): the host re-stages the model, evicts the context, and
/// the next turn's launch carries the new env. Reported as [`ModelSwitch::Relaunch`].
pub struct AcpRunExecutor {
    source: Arc<dyn AgentChannelSource>,
    policy: SupervisePolicy,
    observer: Option<Arc<dyn LaunchObserver>>,
}

impl AcpRunExecutor {
    /// This backend's mid-switch capability (R7): an ACP CLI relaunches.
    #[must_use]
    pub fn model_switch(&self) -> ModelSwitch {
        ModelSwitch::Relaunch
    }
}

impl AcpRunExecutor {
    pub fn new(source: Arc<dyn AgentChannelSource>) -> Self {
        Self {
            source,
            policy: SupervisePolicy::default(),
            observer: None,
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: SupervisePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Observe this executor's agent bring-up (install → launch → initialize →
    /// ready → failed), so a host can publish progress to a UI. Without one, the
    /// lifecycle notifications are dropped (no behavior change).
    #[must_use]
    pub fn with_launch_observer(mut self, observer: Arc<dyn LaunchObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// A scoped lifecycle sink for one run (`None` without a wired observer): the
    /// observer plus the thread scope, so a UI channel can route the events.
    fn launch_sink<'a>(&'a self, scope: &'a str) -> Option<LaunchSink<'a>> {
        self.observer
            .as_ref()
            .map(|observer| LaunchSink::new(observer.as_ref(), scope))
    }
}

/// Emit a lifecycle event to an optional scoped sink (no-op without an observer).
fn notify(sink: Option<LaunchSink<'_>>, event: AcpLaunchEvent) {
    if let Some(sink) = sink {
        sink.emit(event);
    }
}

/// Whether the run's ACP backend installs dynamically on launch (an `npx` adapter),
/// so the executor can surface an `Installing` phase before the process is usable.
fn dynamic_install_backend(activation: &RunActivation) -> bool {
    match Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref) {
        Backend::Acp { cli } => acp_cli(&cli).is_some_and(is_dynamic_install),
        _ => false,
    }
}

/// The prompt for this turn: the concatenated text of the activation's input.
fn prompt_of(input: &[Message]) -> String {
    input
        .iter()
        .map(Message::text_content)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Map a clean ACP turn outcome to a terminal cause.
fn end_cause(reason: TerminationReason) -> EndCause {
    match reason {
        TerminationReason::NaturalEnd => EndCause::NaturalEnd,
        TerminationReason::Cancelled => EndCause::Cancelled,
        TerminationReason::Refusal => EndCause::Stopped("agent refused".to_string()),
        TerminationReason::Error => EndCause::Error(Failure::Inference {
            code: "acp_error".to_string(),
            message: "agent reported an error".to_string(),
        }),
        TerminationReason::TimedOut => EndCause::Stopped("turn deadline exceeded".to_string()),
    }
}

/// Map a classified ACP failure to a terminal cause. The failure's prompt is
/// committed as an assistant message (below) so the run surface explains it.
fn failure_cause(failure: &AcpFailure) -> EndCause {
    match failure.termination() {
        TerminationReason::TimedOut => EndCause::Stopped("turn deadline exceeded".to_string()),
        TerminationReason::Refusal => EndCause::Stopped("agent refused".to_string()),
        _ => EndCause::Error(Failure::Inference {
            code: "acp_failure".to_string(),
            message: failure.message.clone(),
        }),
    }
}

#[async_trait]
impl RunExecutor for AcpRunExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<Phase> {
        // Lifecycle bring-up (observed for UI progress): an npx-wrapped adapter may
        // dynamically install on a cold cache (the slow step) before it launches.
        // Scope events to the thread so a per-session UI channel routes them.
        let scope = activation.thread_id.0.clone();
        let launch_sink = self.launch_sink(&scope);
        if dynamic_install_backend(&activation) {
            notify(
                launch_sink,
                AcpLaunchEvent::stage(AcpLaunchStage::Installing),
            );
        }
        notify(
            launch_sink,
            AcpLaunchEvent::stage(AcpLaunchStage::Launching),
        );

        // The host opens the channel (sandbox launch / remote dial). A failure here
        // is a launch/config fault → classify at the Initialize stage.
        let mut session = match self.source.open(&activation).await {
            Ok(session) => session,
            Err(open) => {
                notify(
                    launch_sink,
                    AcpLaunchEvent::with_detail(AcpLaunchStage::Failed, open.0.clone()),
                );
                let failure = classify_error(Stage::Initialize, &RawAcpError::message(open.0));
                return finish_failure(&context, &activation, &failure).await;
            }
        };

        let prompt = prompt_of(&activation.input);
        let mut appender = CollectingAppender::default();
        let (_tx, mut injections) = tokio::sync::mpsc::channel::<Injection>(1);
        let cancel = async {
            match &context.cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };

        let process = session.process.clone();
        let outcome = Supervisor::supervise(
            session.channel.as_mut(),
            process.as_ref(),
            &prompt,
            &mut appender,
            cancel,
            &mut injections,
            self.policy,
            session.codec,
            launch_sink,
        )
        .await;

        match outcome {
            Ok(reason) => {
                let phase = Phase::Ended(end_cause(reason));
                commit(
                    &context,
                    &activation.thread_id,
                    activation.run_id,
                    appender.messages,
                    &phase,
                )
                .await?;
                Ok(phase)
            }
            // A driver error mid-turn: classify it (oversight taxonomy) and surface
            // its prompt. No retry/reschedule — that is a host concern above us.
            Err(err) => {
                let failure = classify_from_acp_error(&err);
                let mut messages = appender.messages;
                messages.push(Message::text(
                    MessageId(format!("acp-err-{}", messages.len() + 1)),
                    Role::Assistant,
                    failure.prompt(),
                ));
                let phase = Phase::Ended(failure_cause(&failure));
                commit(
                    &context,
                    &activation.thread_id,
                    activation.run_id,
                    messages,
                    &phase,
                )
                .await?;
                Ok(phase)
            }
        }
    }
}

/// Classify a bridge/supervisor [`AcpError`] into a neutral [`AcpFailure`]. A
/// truncated stream or an IO drop mid-turn is the backend cutting the turn
/// (Prompt stage); a malformed frame is a permanent adapter fault.
fn classify_from_acp_error(err: &AcpError) -> AcpFailure {
    classify_error(Stage::Prompt, &RawAcpError::message(err.to_string()))
}

/// Commit a terminal failure that occurred before/instead of a turn (open fault):
/// one assistant message with the prompt, plus the terminal phase.
async fn finish_failure(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    failure: &AcpFailure,
) -> Result<Phase> {
    let phase = Phase::Ended(failure_cause(failure));
    let messages = vec![Message::text(
        MessageId("acp-err-1".to_string()),
        Role::Assistant,
        failure.prompt(),
    )];
    commit(
        context,
        &activation.thread_id,
        activation.run_id.clone(),
        messages,
        &phase,
    )
    .await?;
    Ok(phase)
}

/// Commit the turn's messages + terminal phase through the one boundary (G13).
async fn commit(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    messages: Vec<Message>,
    phase: &Phase,
) -> Result<()> {
    if let Some(coordinator) = &context.commit {
        awaken_agent_contract::commit::commit_run_turn(
            coordinator.as_ref(),
            thread_id,
            &run_id,
            messages,
            phase.clone(),
        )
        .await
        .map_err(|e| Error::Commit(e.to_string()))?;
    }
    Ok(())
}

/// A [`RunFactAppender`] that collects projected assistant text into committable
/// messages, enforcing the monotonic-seq contract. Tool calls are not committed
/// as messages in this slice; `TurnEnd` is carried by the returned reason.
#[derive(Default)]
struct CollectingAppender {
    last: u64,
    messages: Vec<Message>,
}

#[async_trait]
impl RunFactAppender for CollectingAppender {
    async fn append(&mut self, seq: u64, event: &AgentEvent) -> std::result::Result<(), AppendError> {
        if seq <= self.last {
            return Err(AppendError::NonMonotonic {
                got: seq,
                last: self.last,
            });
        }
        self.last = seq;
        if let AgentEvent::Message { text } = event {
            self.messages.push(Message::text(
                MessageId(format!("acp-{seq}")),
                Role::Assistant,
                text.clone(),
            ));
        }
        Ok(())
    }
}

/// Routes each run to the native / ACP / remote executor by its [`Backend`] (R3).
/// Each peer is a `RunExecutor` (the remote one held as a trait object, so this
/// crate needs no dependency on the A2A executor), so the match on the backend sum
/// is the entire "which runtime serves this agent" mechanism — every backend is
/// reachable over every wire adapter exactly like a native one.
pub struct DispatchRunExecutor {
    native: Arc<dyn RunExecutor>,
    acp: Arc<dyn RunExecutor>,
    remote: Arc<dyn RunExecutor>,
}

impl DispatchRunExecutor {
    pub fn new(
        native: Arc<dyn RunExecutor>,
        acp: Arc<dyn RunExecutor>,
        remote: Arc<dyn RunExecutor>,
    ) -> Self {
        Self {
            native,
            acp,
            remote,
        }
    }
}

#[async_trait]
impl RunExecutor for DispatchRunExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<Phase> {
        match Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref) {
            Backend::Native => self.native.execute(activation, context).await,
            Backend::Acp { .. } => self.acp.execute(activation, context).await,
            Backend::Remote { .. } => self.remote.execute(activation, context).await,
        }
    }
}

mod acp_cli;
mod subprocess;
pub use acp_cli::{
    AcpCli, McpInterface, ModelDelivery, ResolvedModel, acp_cli, is_dynamic_install, known_acp_clis,
};
pub use subprocess::{AcpLaunch, LaunchResolver, ProjectingChannelSource, SubprocessChannelSource};

#[cfg(test)]
mod tests;
