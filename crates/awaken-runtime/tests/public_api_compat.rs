use std::sync::Arc;

use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::RunRequestOrigin;
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_runtime::RunRequest;
use awaken_runtime::RuntimeError;
use awaken_runtime::backend::{BackendControl, BackendRootRunRequest};
use awaken_runtime::registry::{
    AgentResolver, ExecutionResolver, ResolvedAgent, ResolvedExecution,
};

struct CompatResolver;

impl AgentResolver for CompatResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        unreachable!("compat test only checks struct construction")
    }
}

impl ExecutionResolver for CompatResolver {
    fn resolve_execution(&self, _agent_id: &str) -> Result<ResolvedExecution, RuntimeError> {
        unreachable!("compat test only checks struct construction")
    }
}

#[test]
fn run_request_struct_literal_keeps_0_2_fields() {
    let request = RunRequest {
        messages: vec![Message::user("hello")],
        messages_already_persisted: false,
        thread_id: "thread-compat".to_string(),
        agent_id: None,
        overrides: None,
        decisions: Vec::new(),
        frontend_tools: Vec::new(),
        origin: RunRequestOrigin::User,
        run_mode: RunMode::Foreground,
        adapter: AdapterKind::Internal,
        parent_run_id: None,
        parent_thread_id: None,
        continue_run_id: None,
        run_id_hint: None,
        dispatch_id_hint: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        run_inbox: None,
    };

    assert_eq!(request.thread_id, "thread-compat");
    assert_eq!(request.messages.len(), 1);
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
    };

    assert_eq!(request.agent_id, "agent");
    assert!(!request.is_continuation);
}
