use super::*;
use crate::registry::resolver::ResolvedAgent;
use crate::state::StateStore;
use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::inference::StreamResult;

struct DummyExecutor;

#[async_trait]
impl LlmExecutor for DummyExecutor {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        unimplemented!("test fixture; never invoked")
    }
    fn name(&self) -> &str {
        "dummy"
    }
}

#[test]
fn make_ctx_populates_agent_spec_in_production() {
    // Regression for F1: prior `make_ctx` left `agent_spec` as the
    // `Arc::new(AgentSpec::default())` default, with an empty
    // `system_prompt`. Observability hooks then silently emitted
    // None for prompt_id. This test pins the contract that
    // make_ctx now stamps the resolved agent's spec onto the
    // PhaseContext, which is the load-bearing field the prompt_id
    // path reads.
    let store = StateStore::new();
    let agent = ResolvedAgent::new(
        "weather",
        "test-model",
        "You are a forecaster.",
        Arc::new(DummyExecutor),
    );
    let ctx = make_ctx(
        Phase::RunStart,
        &[],
        &RunIdentity::default(),
        &store,
        None,
        &agent,
    );
    assert_eq!(ctx.agent_spec.id, "weather");
    assert_eq!(ctx.agent_spec.system_prompt, "You are a forecaster.");
}
