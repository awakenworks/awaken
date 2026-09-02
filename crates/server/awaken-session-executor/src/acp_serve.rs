//! Serve our brain *as* an ACP agent (the reverse of driving an external CLI): an
//! ACP client (a gateway / orchestrator) drives our neutral [`RunApplication`]
//! over the ACP session lifecycle. This is the anti-corruption adapter — it maps
//! ACP `initialize` / `session/new` / `session/prompt` onto `run`, and the
//! run's outcome back onto an ACP stop reason. The WS / JSON-RPC transport wraps
//! this driver (like the a2a router wraps the same port); the driver itself is
//! transport-agnostic and unit-testable.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_session_contract::{RunApplication, RunApplicationError};

/// The ACP stop reason a served prompt operation ended on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpStop {
    /// The Run ended naturally (ACP `end_turn`).
    EndTurn,
    /// The run is awaiting on a tool result from the client (ACP `requires_action`).
    RequiresAction,
    /// The run exhausted its step budget (ACP `max_turn_requests`).
    MaxTurns,
}

/// The result of one served ACP prompt operation: the assistant messages produced and
/// the stop reason.
#[derive(Debug, Clone)]
pub struct AcpTurn {
    pub messages: Vec<Message>,
    pub stop: AcpStop,
}

/// Serves a [`RunApplication`] (e.g. `RunApplicationHost` over the shared host) as an ACP
/// agent. One instance backs many ACP sessions, each keyed by a minted session id
/// (a Thread), so a Run served here is resumable/observable on the same Thread
/// through any other protocol adapter bound to the same host.
pub struct AcpServeHost {
    runtime: Arc<dyn RunApplication>,
}

impl AcpServeHost {
    #[must_use]
    pub fn new(runtime: Arc<dyn RunApplication>) -> Self {
        Self { runtime }
    }

    /// The model id advertised in the ACP `initialize` response.
    #[must_use]
    pub fn model(&self) -> String {
        self.runtime.model()
    }

    /// The served session's accumulated token usage `(input_tokens, output_tokens)`.
    /// Serving as an ACP agent runs our *native* engine to answer prompts, so a Run
    /// served over ACP records token usage exactly like a native Run — this is the
    /// one ACP direction where usage is real (driving an *external* ACP CLI cannot be,
    /// since the ACP wire carries no token counts). Zero until a Run has executed.
    pub async fn usage(&self, session: &str) -> Result<(u64, u64), RunApplicationError> {
        self.runtime.usage(session).await
    }

    /// ACP `session/new`: mint a fresh session id (a thread) for a served session.
    #[must_use]
    pub fn new_session(&self) -> String {
        awaken_runtime::fresh_process_id("acp-serve")
    }

    /// ACP `session/prompt`: run one Runtime Run on `session` with the prompt
    /// text, and map the outcome onto an ACP stop reason.
    pub async fn prompt(
        &self,
        session: &str,
        agent: Option<String>,
        text: &str,
    ) -> Result<AcpTurn, RunApplicationError> {
        let operation_id = awaken_runtime::fresh_process_id("acp-operation");
        let user = Message::text(
            MessageId(awaken_runtime::fresh_process_id("acp-u")),
            Role::User,
            text.to_string(),
        );
        let outcome = self
            .runtime
            .run(&operation_id, session, agent, vec![user])
            .await?;
        Ok(AcpTurn {
            stop: map_stop(outcome.state()),
            messages: outcome.new_messages,
        })
    }
}

/// Map a run's terminal flags onto the ACP stop reason (a pure decision).
fn map_stop(state: &RunState) -> AcpStop {
    match state {
        RunState::Awaiting => AcpStop::RequiresAction,
        RunState::Ended(EndCause::MaxSteps) => AcpStop::MaxTurns,
        // A natural end or a terminal fault both close the ACP prompt operation (ACP has no
        // distinct fault stop reason; the failure rides the committed record).
        RunState::Ended(_) => AcpStop::EndTurn,
        RunState::Running => unreachable!("a completed step cannot still be running"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunApplicationHost;
    use crate::host::SharedHost;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, TokenUsage,
    };

    /// A deterministic model (the external model dependency): the runtime it drives
    /// is the real `RunApplicationHost`/`SharedHost` engine loop, not a double. It reports a
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

    /// Cause/effect design: C0 the production Dispatch Session Runtime composition
    /// is installed; C1 ACP Serve wraps the real RunApplicationHost with a model
    /// returning `served reply` and usage 13/9; C2 two Sessions are created; C3
    /// only the first is prompted. Effects: E1 ids are distinct and the model
    /// identity is retained; E2 the prompted Session ends and surfaces the reply
    /// with usage 13/9; E3 the untouched Session remains 0/0. Decision table:
    /// S1=C0+C1+C2+C3=>E1+E2; S2=C0+C1+C2+!C3=>E3. Constraint/Invariant:
    /// ACP Serve uses the real RunApplicationHost and one Session authority.
    /// Decision rule: execute S1 and S2 to cover prompted and untouched Sessions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serves_a_real_run_over_the_real_run_application_host() {
        // Decision rule: S1 and S2 cover prompted and untouched Sessions.
        // The real production runtime: RunApplicationHost over a real SharedHost.
        let host = Arc::new(SharedHost::new(
            Arc::new(DeterministicModel),
            "served-model",
        ));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let serve = AcpServeHost::new(Arc::new(RunApplicationHost::new(host)));

        assert_eq!(serve.model(), "served-model");
        let s1 = serve.new_session();
        let s2 = serve.new_session();
        assert_ne!(s1, s2);

        let served_run = serve.prompt(&s1, None, "hi brain").await.unwrap();
        assert_eq!(served_run.stop, AcpStop::EndTurn);
        assert!(
            served_run
                .messages
                .iter()
                .any(|m| m.text_content().contains("served reply")),
            "the real brain's reply is surfaced: {:?}",
            served_run
                .messages
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>()
        );

        // Serving over ACP ran the native engine, so the Run's token usage was
        // recorded on the served Session exactly like a native Run.
        assert_eq!(
            serve.usage(&s1).await.expect("usage remains available"),
            (13, 9),
            "a Run served over ACP records native-engine token usage"
        );
        // A Session that never ran a Run has zero usage.
        assert_eq!(
            serve.usage(&s2).await.expect("usage remains available"),
            (0, 0)
        );
    }

    #[test]
    fn stop_reason_mapping_is_total() {
        use awaken_agent_contract::agent::run::Failure;

        // Cause/effect decision table: Awaiting -> ACP `requires_action`;
        // MaxSteps -> ACP `max_turn_requests`; every other committed end -> ACP `end_turn`.
        assert_eq!(map_stop(&RunState::Awaiting), AcpStop::RequiresAction);
        assert_eq!(
            map_stop(&RunState::Ended(EndCause::MaxSteps)),
            AcpStop::MaxTurns
        );
        assert_eq!(
            map_stop(&RunState::Ended(EndCause::NaturalEnd)),
            AcpStop::EndTurn
        );
        // A terminal fault closes the ACP prompt operation (no distinct fault stop reason).
        assert_eq!(
            map_stop(&RunState::Ended(EndCause::Error(Failure::Inference {
                code: "test".into(),
                message: "failed".into(),
            }))),
            AcpStop::EndTurn
        );
    }
}
