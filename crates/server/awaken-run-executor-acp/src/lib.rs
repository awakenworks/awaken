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
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_protocol_acp::{
    AcpError, AcpFailure, AgentEvent, Injection, RawAcpError, RunEventSink, SinkError, Stage,
    SupervisePolicy, Supervisor, TerminationReason, classify_error,
};
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
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: SupervisePolicy) -> Self {
        self.policy = policy;
        self
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
        // The host opens the channel (sandbox launch / remote dial). A failure here
        // is a launch/config fault → classify at the Initialize stage.
        let mut session = match self.source.open(&activation).await {
            Ok(session) => session,
            Err(open) => {
                let failure = classify_error(Stage::Initialize, &RawAcpError::message(open.0));
                return finish_failure(&context, &activation, &failure).await;
            }
        };

        let prompt = prompt_of(&activation.input);
        let mut sink = CollectingSink::default();
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
            &mut sink,
            cancel,
            &mut injections,
            self.policy,
        )
        .await;

        match outcome {
            Ok(reason) => {
                let phase = Phase::Ended(end_cause(reason));
                commit(
                    &context,
                    &activation.thread_id,
                    activation.run_id,
                    sink.messages,
                    &phase,
                )
                .await?;
                Ok(phase)
            }
            // A driver error mid-turn: classify it (oversight taxonomy) and surface
            // its prompt. No retry/reschedule — that is a host concern above us.
            Err(err) => {
                let failure = classify_from_acp_error(&err);
                let mut messages = sink.messages;
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
        let commit = ThreadCommit {
            thread_id: thread_id.clone(),
            run_fact: RunFact {
                run_id,
                phase: phase.clone(),
            },
            messages,
            state: Vec::new(),
            events: Vec::new(),
            outbox: Vec::new(),
            waiting: None,
        };
        coordinator
            .commit(commit)
            .await
            .map_err(|e| Error::Commit(e.to_string()))?;
    }
    Ok(())
}

/// A [`RunEventSink`] that collects projected assistant text into committable
/// messages, enforcing the monotonic-seq contract. Tool calls are not committed
/// as messages in this slice; `TurnEnd` is carried by the returned reason.
#[derive(Default)]
struct CollectingSink {
    last: u64,
    messages: Vec<Message>,
}

#[async_trait]
impl RunEventSink for CollectingSink {
    async fn append(&mut self, seq: u64, event: &AgentEvent) -> std::result::Result<(), SinkError> {
        if seq <= self.last {
            return Err(SinkError::NonMonotonic {
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

/// Routes each run to the native or ACP executor by the resolved spec's
/// [`runtime_adapter`](awaken_runtime_contract::resolved::ResolvedSpec::runtime_adapter)
/// (R3). Both peers are `RunExecutor`s, so the selection is the entire "which
/// runtime serves this agent" mechanism — no separate backend trait, and an
/// ACP-backed agent is reachable over every wire adapter exactly like a native one.
pub struct DispatchRunExecutor {
    native: Arc<dyn RunExecutor>,
    acp: Arc<dyn RunExecutor>,
}

impl DispatchRunExecutor {
    pub fn new(native: Arc<dyn RunExecutor>, acp: Arc<dyn RunExecutor>) -> Self {
        Self { native, acp }
    }
}

#[async_trait]
impl RunExecutor for DispatchRunExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<Phase> {
        match activation.snapshot.resolved_spec.backend() {
            Backend::Native => self.native.execute(activation, context).await,
            Backend::Acp { .. } => self.acp.execute(activation, context).await,
        }
    }
}

mod subprocess;
pub use subprocess::{AcpLaunch, SubprocessChannelSource};

#[cfg(test)]
mod tests;
