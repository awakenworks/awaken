use std::sync::Arc;

use awaken_runtime::RunActivation;
use awaken_runtime::RuntimeError;
use awaken_runtime::backend::{BackendControl, BackendRootRunRequest};
use awaken_runtime::loop_runner::CommitWiring;
use awaken_runtime::registry::{AgentResolver, ResolvedAgent};
use awaken_runtime::resolution::ExecutionPlan;
use awaken_runtime_contract::contract::event_sink::NullEventSink;
use awaken_runtime_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_runtime_contract::contract::message::Message;
use awaken_runtime_contract::contract::storage::RunRequestOrigin;
use awaken_runtime_contract::contract::tool_intercept::{AdapterKind, RunMode};

struct CompatResolver;

impl AgentResolver for CompatResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        unreachable!("compat test only checks struct construction")
    }

    fn resolve_execution(&self, _agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        unreachable!("compat test only checks struct construction")
    }
}

#[test]
fn run_activation_constructor_sets_input_and_trace() {
    let request = RunActivation::new("thread-compat", vec![Message::user("hello")])
        .with_origin(RunRequestOrigin::User)
        .with_run_mode(RunMode::Foreground)
        .with_adapter(AdapterKind::Internal);
    assert_eq!(request.thread_id(), "thread-compat");
    assert_eq!(request.messages().len(), 1);
}

#[test]
fn backend_root_run_request_struct_literal_keeps_0_2_fields() {
    let resolver = CompatResolver;
    let request = BackendRootRunRequest {
        agent_id: "agent",
        messages: vec![Message::user("hello")],
        new_messages: vec![Message::user("hello")],
        sink: Arc::new(NullEventSink),
        resolver: &resolver,
        run_identity: RunIdentity::new(
            "thread-compat".to_string(),
            None,
            "run-compat".to_string(),
            None,
            "agent".to_string(),
            RunOrigin::User,
        ),
        checkpoint_store: None,
        control: BackendControl::default(),
        decisions: Vec::new(),
        overrides: None,
        frontend_tools: Vec::new(),
        local: None,
        inbox: None,
        is_continuation: false,
        commit: CommitWiring::default(),
    };

    assert_eq!(request.agent_id, "agent");
    assert!(!request.is_continuation);
}
