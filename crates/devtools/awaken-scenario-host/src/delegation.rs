//! Native-coordinator delegation scenarios, including Native, recursive-self,
//! and ACP child execution through the same child-Run lifecycle.

use std::sync::Arc;

use awaken_runtime_contract::StaticPublishedAgentSnapshots;
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::resolved::{ModelBinding, ToolKind};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use axum::Router;

use super::composition::mount;
use super::deployment::resource_host;
use super::{DelegatingModel, FAKE_ACP_SCRIPT, scenario_model, scenario_shell_argv};

/// A Native coordinator whose roster proves Native, recursive-self, and ACP
/// delegated attempts use one parent-mediated child-Run path.
pub fn build_delegation_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(DelegatingModel), "delegate");
    let snapshot =
        |owner_id: &str, backend_ref: &str, delegates: Vec<AgentId>, recursive_self: bool| {
            let mut tools = awaken_runtime_host::authorable_tools();
            if delegates.is_empty() {
                tools.retain(|tool| tool.kind != ToolKind::AgentDelegation);
            }
            ExecutableAgentSnapshot::builder(owner_id)
                .model(ModelBinding::new("default", &model_ref, backend_ref))
                .tools(tools)
                .agent_bindings(AgentBindings {
                    delegates: delegates
                        .into_iter()
                        .map(|agent_id| AgentDelegateBinding {
                            recursive_self: recursive_self && agent_id.0 == owner_id,
                            agent_id,
                            source_revision: None,
                        })
                        .collect(),
                    ..Default::default()
                })
                .build()
        };
    let publications = StaticPublishedAgentSnapshots::try_new([
        snapshot(
            "assistant",
            "default",
            vec![
                AgentId("researcher".into()),
                AgentId("acp-worker".into()),
                AgentId("assistant".into()),
            ],
            true,
        ),
        snapshot("researcher", "default", Vec::new(), false),
        snapshot("acp-worker", "acp:claude", Vec::new(), false),
    ])
    .expect("valid scenario Agent publications");
    let launch =
        awaken_run_executor_acp::AcpLaunch::custom(scenario_shell_argv(FAKE_ACP_SCRIPT), vec![]);
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        launch,
    ));
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    let host = resource_host(model, model_ref)
        .with_acp(acp)
        .with_agent_publications(Arc::new(publications));
    mount(Arc::new(host))
}
