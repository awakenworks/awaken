//! Serve our brain *as* an ACP agent (the reverse of driving an external CLI): an
//! ACP client (a gateway / orchestrator) drives our neutral [`ProtocolRuntime`]
//! over the ACP session lifecycle. This is the anti-corruption adapter — it maps
//! ACP `initialize` / `session/new` / `session/prompt` onto `run_turn`, and the
//! run's outcome back onto an ACP stop reason. The WS / JSON-RPC transport wraps
//! this driver (like the a2a router wraps the same port); the driver itself is
//! transport-agnostic and unit-testable.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_protocol_transport::{DriverError, ProtocolRuntime};

/// The ACP stop reason a served turn ended on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpStop {
    /// The turn ended naturally (ACP `end_turn`).
    EndTurn,
    /// The run parked on a tool awaiting the client (ACP `requires_action`).
    RequiresAction,
    /// The run exhausted its step budget (ACP `max_turn_requests`).
    MaxTurns,
}

/// The result of one served ACP prompt turn: the assistant messages produced and
/// the stop reason.
#[derive(Debug, Clone)]
pub struct AcpTurn {
    pub messages: Vec<Message>,
    pub stop: AcpStop,
}

/// Serves a [`ProtocolRuntime`] (e.g. `ProtocolHost` over the shared host) as an ACP
/// agent. One instance backs many ACP sessions, each keyed by a minted session id
/// (a thread), so a turn served here is resumable/observable on the same thread
/// through any other protocol adapter bound to the same host.
pub struct AcpServeHost {
    runtime: Arc<dyn ProtocolRuntime>,
    seq: AtomicU64,
}

impl AcpServeHost {
    #[must_use]
    pub fn new(runtime: Arc<dyn ProtocolRuntime>) -> Self {
        Self {
            runtime,
            seq: AtomicU64::new(0),
        }
    }

    /// The model id advertised in the ACP `initialize` response.
    #[must_use]
    pub fn model(&self) -> String {
        self.runtime.model()
    }

    /// ACP `session/new`: mint a fresh session id (a thread) for a served session.
    #[must_use]
    pub fn new_session(&self) -> String {
        format!("acp-serve-{}", self.seq.fetch_add(1, Ordering::SeqCst))
    }

    /// ACP `session/prompt`: run one turn of our brain on `session` with the prompt
    /// text, and map the outcome onto an ACP stop reason.
    pub async fn prompt(
        &self,
        session: &str,
        agent: Option<String>,
        text: &str,
    ) -> Result<AcpTurn, DriverError> {
        let user = Message::text(
            MessageId(format!("acp-u-{}", self.seq.fetch_add(1, Ordering::SeqCst))),
            Role::User,
            text.to_string(),
        );
        let outcome = self.runtime.run_turn(session, agent, vec![user]).await?;
        let stop = if outcome.waiting {
            AcpStop::RequiresAction
        } else if outcome.exhausted {
            AcpStop::MaxTurns
        } else {
            AcpStop::EndTurn
        };
        Ok(AcpTurn {
            messages: outcome.new_messages,
            stop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_protocol_transport::{Pending, Resume, StepOutcome};

    /// A fake ProtocolRuntime: echoes the prompt, or parks/exhausts on command.
    struct FakeRt {
        waiting: bool,
        exhausted: bool,
    }
    #[async_trait::async_trait]
    impl ProtocolRuntime for FakeRt {
        async fn run_turn(
            &self,
            _thread: &str,
            _agent: Option<String>,
            messages: Vec<Message>,
        ) -> Result<StepOutcome, DriverError> {
            Ok(StepOutcome {
                new_messages: vec![Message::text(
                    MessageId("a".into()),
                    Role::Assistant,
                    format!("echo: {}", messages[0].text_content()),
                )],
                waiting: self.waiting,
                exhausted: self.exhausted,
                pending: None,
            })
        }
        async fn resume(
            &self,
            _t: &str,
            _id: &str,
            _r: Resume,
        ) -> Result<StepOutcome, DriverError> {
            unreachable!()
        }
        async fn pending(&self, _t: &str) -> Option<Pending> {
            None
        }
        async fn history(&self, _t: &str) -> Vec<Message> {
            Vec::new()
        }
        fn model(&self) -> String {
            "served-model".into()
        }
    }

    #[tokio::test]
    async fn serves_a_prompt_turn_and_maps_the_stop_reason() {
        let host = AcpServeHost::new(Arc::new(FakeRt {
            waiting: false,
            exhausted: false,
        }));
        assert_eq!(host.model(), "served-model");
        let s1 = host.new_session();
        let s2 = host.new_session();
        assert_ne!(s1, s2);

        let turn = host.prompt(&s1, None, "hi brain").await.unwrap();
        assert_eq!(turn.stop, AcpStop::EndTurn);
        assert_eq!(turn.messages[0].text_content(), "echo: hi brain");
    }

    #[tokio::test]
    async fn maps_waiting_and_exhausted_to_acp_stops() {
        let waiting = AcpServeHost::new(Arc::new(FakeRt {
            waiting: true,
            exhausted: false,
        }));
        assert_eq!(
            waiting.prompt("t", None, "x").await.unwrap().stop,
            AcpStop::RequiresAction
        );
        let exhausted = AcpServeHost::new(Arc::new(FakeRt {
            waiting: false,
            exhausted: true,
        }));
        assert_eq!(
            exhausted.prompt("t", None, "x").await.unwrap().stop,
            AcpStop::MaxTurns
        );
    }
}
