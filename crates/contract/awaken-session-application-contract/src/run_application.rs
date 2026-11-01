// @formal entrypoint session_time.utc_projection_domain_is_total_exact_and_idempotent
//! Protocol-neutral Run application API shared by every public adapter.
//!
//! This module is the one application waist for submit/resume/control/query.
//! Protocol crates own only wire translation; Runtime and Coordinator adapters
//! implement this port over the same committed Run authority.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::event::AgentEvent;
use awaken_agent_contract::page::{HistoryPage, UnknownCursor, paginate_by_id};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink as StreamSink};

use crate::{Pending, RunError, StepOutcome};
use awaken_agent_contract::agent::run::Id as RunId;

/// The application error surface consumed by public protocol adapters.
/// This is an alias, not a second error authority.
pub type RunApplicationError = RunError;

/// One neutral resume command for a pending Run.
#[derive(Debug, Clone)]
pub enum RunResume {
    Permission(PermissionDecision),
    ClientResult {
        content: Vec<ContentBlock>,
        is_error: bool,
    },
}

/// Receipt of one explicit Stop command, distinct from a passive stream
/// hang-up: every accepted Stop gets its own id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StopReceipt {
    pub id: String,
    pub thread_id: String,
}

impl StopReceipt {
    /// Issue the receipt for one accepted Stop on `thread`. The id joins a
    /// per-process random nonce with a per-process sequence, so it never
    /// repeats within a process and does not collide across restarts.
    #[must_use]
    pub fn issue(thread: &str) -> Self {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NONCE: OnceLock<u64> = OnceLock::new();
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let nonce = *NONCE.get_or_init(|| {
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        });
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            id: format!("stop_{nonce:016x}{sequence:08x}"),
            thread_id: thread.to_owned(),
        }
    }
}

/// The one neutral application port driven by AG-UI, AI SDK, A2A and ACP-facing
/// adapters. Session creation and realization stay on their narrower Session
/// application ports.
#[async_trait]
pub trait RunApplication: Send + Sync {
    async fn run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError>;

    async fn run_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let _ = sink;
        self.run(operation_id, thread, agent, messages).await
    }

    async fn resume(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError>;

    /// Same resume command, with best-effort live observation. Implementations
    /// without live support still return the authoritative committed outcome.
    async fn resume_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        _sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.resume(operation_id, thread, tool_use_id, resume).await
    }

    async fn interrupt(&self, _thread: &str) -> Result<(), RunApplicationError> {
        Ok(())
    }

    /// An explicit client Stop: the same interrupt command, answered with its
    /// own receipt so two Stops are two attributable commands (ADR-0103).
    async fn stop(&self, thread: &str) -> Result<StopReceipt, RunApplicationError> {
        self.interrupt(thread).await?;
        Ok(StopReceipt::issue(thread))
    }

    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunApplicationError>;
    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError>;
    fn model(&self) -> String;

    async fn usage(&self, _thread: &str) -> Result<(u64, u64), RunApplicationError> {
        Ok((0, 0))
    }
}

/// Required capability for a protocol whose authenticated request binds an
/// exact Workspace and an already-existing Session. A composition root must
/// install this port explicitly; an unscoped Run port cannot silently accept
/// or downgrade such a request. Both methods reuse the Session application's
/// ordinary admission and resume state machines. A production composition
/// must back this port with durable SessionRun admission: public adapters may
/// detach best-effort streams without cancelling its already-owned Run.
#[async_trait]
pub trait OwnerScopedRunApplication: RunApplication {
    async fn run_streaming_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError>;

    async fn resume_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError>;
    /// Owner authorization remains required for observed continuations.
    async fn resume_streaming_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        _sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.resume_for_owner(owner_scope, operation_id, thread, tool_use_id, resume)
            .await
    }
}

/// The existing public stream adapters select their one Run port here. A local
/// turn is connection-bound; an owner-scoped Session Run has a durable owner
/// beyond the lifetime of its best-effort browser stream.
#[derive(Clone)]
pub enum RunMode {
    Local,
    OwnerScoped(Arc<dyn OwnerScopedRunApplication>),
}

impl RunMode {
    /// A passive stream disconnect is not an explicit Run cancellation. Only
    /// the local, connection-bound path interrupts its in-flight turn; the
    /// owner-scoped path leaves settlement to Session/Dispatch authority.
    pub async fn on_stream_disconnect(
        &self,
        runtime: &dyn RunApplication,
        thread: &str,
    ) -> Result<(), RunApplicationError> {
        match self {
            Self::Local => runtime.interrupt(thread).await,
            Self::OwnerScoped(_) => Ok(()),
        }
    }
}

/// Narrow application port for an explicit newest-wins Session Run.
///
/// Operational HTTP may expose this command, but only the Session application
/// may turn it into a reservation/activity/activation sequence. Keeping it
/// separate from ordinary [`RunApplication`] prevents public protocols from
/// silently changing append semantics.
#[async_trait]
pub trait SessionRunReplacementApplication: Send + Sync {
    async fn supersede_session_run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError>;
}

/// Narrow application port for publishing a Session Run without observing it.
///
/// Background submission changes only the caller's observation policy. It must
/// still reserve the Run, commit the exact Session activity receipt, and publish
/// that reservation through the same Session-owned admission state machine as a
/// foreground protocol request.
#[async_trait]
pub trait SessionRunBackgroundApplication: Send + Sync {
    async fn submit_session_run_background(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
    ) -> Result<RunId, RunApplicationError>;

    /// Admit the background Run that delivers one Task observation, through
    /// the same reservation, activity and activation as
    /// [`Self::submit_session_run_background`]. The Run's dispatch row carries
    /// `attention.input` privately. The acknowledgement is returned only after
    /// the Run is durably activated, so its sender may stop retrying; the same
    /// operation id with the same input returns the same Run.
    async fn submit_task_attention(
        &self,
        attention: crate::TaskAttention,
    ) -> Result<crate::TaskAttentionAck, RunApplicationError>;
}

/// Concatenate direct text blocks, dropping non-text blocks. Nested tool-result
/// content is deliberately not flattened into the enclosing protocol message.
#[must_use]
pub fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn paginate_history<'a>(
    history: &'a [Message],
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<HistoryPage<'a, Message>, UnknownCursor> {
    paginate_by_id(history, cursor, limit, |message| message.id.0.as_str())
}

/// Shared best-effort live-event channel adapter.
pub struct EventForwardingSink {
    forward: Arc<dyn Fn(AgentEvent) -> Result<(), SinkError> + Send + Sync>,
}

impl EventForwardingSink {
    #[must_use]
    pub fn new(
        forward: impl Fn(AgentEvent) -> Result<(), SinkError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            forward: Arc::new(forward),
        }
    }
}

#[async_trait]
impl StreamSink for EventForwardingSink {
    async fn send(&self, event: Event) -> Result<(), SinkError> {
        (self.forward)(event.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DisconnectProbe(AtomicUsize);

    #[async_trait]
    impl RunApplication for DisconnectProbe {
        async fn run(
            &self,
            _: &str,
            _: &str,
            _: Option<String>,
            _: Vec<Message>,
        ) -> Result<StepOutcome, RunApplicationError> {
            unreachable!()
        }

        async fn resume(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: RunResume,
        ) -> Result<StepOutcome, RunApplicationError> {
            unreachable!()
        }

        async fn interrupt(&self, _: &str) -> Result<(), RunApplicationError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn pending(&self, _: &str) -> Result<Option<Pending>, RunApplicationError> {
            Ok(None)
        }

        async fn history(&self, _: &str) -> Result<Vec<Message>, RunApplicationError> {
            Ok(Vec::new())
        }

        fn model(&self) -> String {
            "disconnect-probe".into()
        }
    }

    #[async_trait]
    impl OwnerScopedRunApplication for DisconnectProbe {
        async fn run_streaming_for_owner(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<String>,
            _: Vec<Message>,
            _: Arc<dyn StreamSink>,
        ) -> Result<StepOutcome, RunApplicationError> {
            unreachable!()
        }

        async fn resume_for_owner(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: RunResume,
        ) -> Result<StepOutcome, RunApplicationError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn stream_disconnect_preserves_only_durable_owner_scoped_runs() {
        // Causes: C1 Local stream closes; C2 owner-scoped stream closes; C3 an
        // explicit stop targets the owner-scoped Run. Effects: E1 C1 invokes
        // interrupt once; E2 C2 adds no interrupt; E3 C3 still invokes the
        // same interrupt port. Rules R1=C1->E1, R2=C2->E2, R3=C3->E3.
        let runtime = Arc::new(DisconnectProbe(AtomicUsize::new(0)));
        RunMode::Local
            .on_stream_disconnect(runtime.as_ref(), "thread")
            .await
            .unwrap();
        assert_eq!(runtime.0.load(Ordering::SeqCst), 1, "R1");
        RunMode::OwnerScoped(runtime.clone())
            .on_stream_disconnect(runtime.as_ref(), "thread")
            .await
            .unwrap();
        assert_eq!(runtime.0.load(Ordering::SeqCst), 1, "R2");
        runtime.interrupt("thread").await.unwrap();
        assert_eq!(runtime.0.load(Ordering::SeqCst), 2, "R3");
    }

    #[test]
    fn terminal_fact_is_derived_from_one_run_state_authority() {
        // Cause/effect decision table:
        // R1 awaiting + pending -> Awaiting fact;
        // R2 MaxSteps -> exhausted finish;
        // R3 classified failure -> RunFailed with the same code;
        // R4 natural end -> ordinary finish; R5 indeterminate -> explicit
        // RunFailed, never success. No second Terminal enum exists.
        // Constraints/invariants: RunState is the sole terminal authority and
        // projection cannot turn Indeterminate or a classified error into success.
        let pending = Pending {
            tool_use_id: "call-1".into(),
            name: "tool".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let awaiting = StepOutcome::awaiting(Vec::new(), Some(pending.clone()));
        assert_eq!(awaiting.state(), &RunState::Awaiting, "R1");
        assert!(
            matches!(
                awaiting.terminal_event(),
                awaken_agent_contract::event::Fact::Awaiting { .. }
            ),
            "R1"
        );

        let exhausted = StepOutcome::ended(Vec::new(), EndCause::MaxSteps);
        assert_eq!(
            exhausted.state(),
            &RunState::Ended(EndCause::MaxSteps),
            "R2"
        );
        assert_eq!(
            exhausted.terminal_event(),
            awaken_agent_contract::event::Fact::RunFinished { exhausted: true },
            "R2"
        );

        let failed = StepOutcome::ended(
            Vec::new(),
            EndCause::Error(Failure::Inference {
                code: "upstream".into(),
                message: "down".into(),
            }),
        );
        assert!(
            matches!(failed.terminal_event(), awaken_agent_contract::event::Fact::RunFailed { ref code, .. } if code == "upstream"),
            "R3"
        );

        let finished = StepOutcome::ended(Vec::new(), EndCause::NaturalEnd);
        assert_eq!(
            finished.state(),
            &RunState::Ended(EndCause::NaturalEnd),
            "R4"
        );

        let indeterminate = StepOutcome::ended(Vec::new(), EndCause::Indeterminate);
        assert!(
            matches!(
                indeterminate.terminal_event(),
                awaken_agent_contract::event::Fact::RunFailed { ref code, .. }
                    if code == "indeterminate"
            ),
            "R5"
        );
    }
}
