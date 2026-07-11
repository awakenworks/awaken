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

    /// The served session's accumulated token usage `(input_tokens, output_tokens)`.
    /// Serving as an ACP agent runs our *native* engine to answer prompts, so a turn
    /// served over ACP records token usage exactly like a native turn — this is the
    /// one ACP direction where usage is real (driving an *external* ACP CLI cannot be,
    /// since the ACP wire carries no token counts). Zero until a turn has run.
    pub async fn usage(&self, session: &str) -> (u64, u64) {
        self.runtime.usage(session).await
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
        Ok(AcpTurn {
            stop: map_stop(outcome.waiting, outcome.exhausted),
            messages: outcome.new_messages,
        })
    }
}

/// Map a run's terminal flags onto the ACP stop reason (a pure decision).
fn map_stop(waiting: bool, exhausted: bool) -> AcpStop {
    if waiting {
        AcpStop::RequiresAction
    } else if exhausted {
        AcpStop::MaxTurns
    } else {
        AcpStop::EndTurn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtocolHost;
    use crate::host::SharedHost;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, TokenUsage,
    };

    /// A deterministic model (the external model dependency): the runtime it drives
    /// is the real `ProtocolHost`/`SharedHost` engine loop, not a double. It reports a
    /// fixed token usage, standing for what a real provider returns.
    struct DeterministicModel;
    #[async_trait::async_trait]
    impl LlmExecutor for DeterministicModel {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("served reply"),
                usage: Some(TokenUsage {
                    prompt_tokens: 13,
                    completion_tokens: 9,
                    ..Default::default()
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serves_a_real_turn_over_the_real_protocol_host() {
        // The real production runtime: ProtocolHost over a real SharedHost.
        let host = Arc::new(SharedHost::new(
            Arc::new(DeterministicModel),
            "served-model",
        ));
        let serve = AcpServeHost::new(Arc::new(ProtocolHost::new(host)));

        assert_eq!(serve.model(), "served-model");
        let s1 = serve.new_session();
        let s2 = serve.new_session();
        assert_ne!(s1, s2);

        let turn = serve.prompt(&s1, None, "hi brain").await.unwrap();
        assert_eq!(turn.stop, AcpStop::EndTurn);
        assert!(
            turn.messages
                .iter()
                .any(|m| m.text_content().contains("served reply")),
            "the real brain's reply is surfaced: {:?}",
            turn.messages
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>()
        );

        // Serving over ACP ran the native engine, so the turn's token usage was
        // recorded on the served session exactly like a native turn.
        assert_eq!(
            serve.usage(&s1).await,
            (13, 9),
            "a turn served over ACP records native-engine token usage"
        );
        // A session that never ran a turn has zero usage.
        assert_eq!(serve.usage(&s2).await, (0, 0));
    }

    #[test]
    fn stop_reason_mapping_is_total() {
        assert_eq!(map_stop(false, false), AcpStop::EndTurn);
        assert_eq!(map_stop(true, false), AcpStop::RequiresAction);
        assert_eq!(map_stop(false, true), AcpStop::MaxTurns);
        // waiting takes precedence over exhausted.
        assert_eq!(map_stop(true, true), AcpStop::RequiresAction);
    }
}
