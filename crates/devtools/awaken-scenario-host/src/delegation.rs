//! Native-coordinator delegation scenarios, including Native, recursive-self,
//! and ACP child execution through the same child-Run lifecycle.

use std::sync::Arc;

use awaken_agent_config::{
    AgentConfig, ModelSelection, MultiagentConfig, MultiagentTarget, compile_published,
};
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate, ToolKind};
use awaken_runtime_contract::snapshot::{
    AgentConfigRevisionRef, AgentId, AgentSnapshotMetadata, ExecutableAgentSnapshot,
};
use axum::Router;

use super::deployment::resource_host;
use super::scenario_platform::{fixed_agent_publication, mount_with_agent_source};
use super::{DelegatingModel, FAKE_ACP_JSONRPC_SCRIPT, scenario_model, scenario_shell_argv};

/// A Native coordinator whose roster proves Native, recursive-self, and ACP
/// delegated attempts use one parent-mediated child-Run path.
pub fn build_delegation_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(DelegatingModel), "delegate");
    let metadata = |owner_id: &str| AgentSnapshotMetadata {
        source: AgentConfigRevisionRef {
            agent_id: AgentId(owner_id.into()),
            revision: 1,
        },
        ..Default::default()
    };
    let snapshot =
        |owner_id: &str, backend_ref: &str, delegates: Vec<AgentId>, recursive_self: bool| {
            let mut tools = awaken_runtime_host::authorable_tools();
            if delegates.is_empty() {
                tools.retain(|tool| tool.kind != ToolKind::AgentDelegation);
            }
            let mut builder = ExecutableAgentSnapshot::builder(owner_id);
            if let Some(runtime) = owner_id
                .strip_prefix("acp-")
                .and_then(|value| value.strip_suffix("-worker"))
            {
                builder = builder.instructions(format!("matrix-runtime={runtime}"));
            }
            builder
                .model(ModelBinding::new("default", &model_ref, backend_ref))
                .tools(tools)
                .metadata(metadata(owner_id))
                .agent_bindings(AgentBindings {
                    delegates: delegates
                        .into_iter()
                        .map(|agent_id| AgentDelegateBinding {
                            recursive_self: recursive_self && agent_id.0 == owner_id,
                            agent_id,
                            source_revision: Some(1),
                        })
                        .collect(),
                    ..Default::default()
                })
                .build()
        };
    // The production config compiler is the one owner of the reserved Advisor
    // descriptor and its frozen binding. Reusing it here keeps the real-process
    // scenario from hand-maintaining a second copy of that capability.
    let tool_catalog = awaken_runtime_host::authorable_tools();
    let primary = ResolvedModelCandidate::host(ModelBinding::new("default", &model_ref, "default"));
    let advisor =
        ResolvedModelCandidate::host(ModelBinding::new("default", "fake-advisor", "default"));
    let mut targets = vec![
        MultiagentTarget::Agent {
            id: "researcher".into(),
            version: Some(1),
        },
        MultiagentTarget::Agent {
            id: "acp-worker".into(),
            version: Some(1),
        },
    ];
    targets.extend(awaken_run_executor_acp::known_acp_clis().iter().map(|cli| {
        MultiagentTarget::Agent {
            id: format!("acp-{}-worker", cli.id),
            version: Some(1),
        }
    }));
    targets.extend([
        MultiagentTarget::SelfReference,
        MultiagentTarget::Advisor {
            model: "claude-opus-4-8".into(),
        },
    ]);
    let assistant = compile_published(
        &AgentConfig {
            id: "assistant".into(),
            instructions: String::new(),
            max_steps: 20,
            model_binding: ModelSelection::Pinned(primary.binding().clone()),
            tool_ids: tool_catalog
                .iter()
                .filter(|tool| tool.kind != ToolKind::AgentDelegation)
                .map(|tool| tool.id.clone())
                .collect(),
            multiagent: Some(MultiagentConfig { agents: targets }),
            ..Default::default()
        },
        &tool_catalog,
        metadata("assistant"),
        primary,
        Vec::new(),
        Some(advisor),
    )
    .expect("compile the fixed coordinator and reserved Advisor capability");
    let mut snapshots = vec![
        assistant,
        snapshot("researcher", "default", Vec::new(), false),
        snapshot("acp-worker", "acp:claude", Vec::new(), false),
    ];
    snapshots.extend(awaken_run_executor_acp::known_acp_clis().iter().map(|cli| {
        snapshot(
            &format!("acp-{}-worker", cli.id),
            &format!("acp:{}", cli.id),
            Vec::new(),
            false,
        )
    }));
    let publication = fixed_agent_publication(snapshots);
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        scenario_shell_argv(FAKE_ACP_JSONRPC_SCRIPT),
        vec![],
    );
    let source = Arc::new(
        awaken_run_executor_acp::SubprocessChannelSource::new(launch)
            .with_codec(awaken_run_executor_acp::Codec::Acp),
    );
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    let platform = resource_host(model, model_ref).map_host(|host| {
        host.with_acp(acp)
            .with_agent_publications(publication.clone())
    });
    mount_with_agent_source(platform, publication)
}
