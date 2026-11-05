//! Native-coordinator delegation scenarios, including Native, recursive-self,
//! and ACP child execution through the same child-Run lifecycle.

use std::sync::Arc;

use awaken_agent_config::{
    AgentConfig, ModelSelection, MultiagentConfig, MultiagentTarget, compile_published,
};
use awaken_agent_contract::{
    ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
    ToolsetSource,
};
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate, ToolKind};
use awaken_runtime_contract::snapshot::{
    AgentConfigRevisionRef, AgentId, AgentSnapshotMetadata, ExecutableAgentSnapshot,
};
use axum::Router;

use super::deployment::{resource_host_with_deployment, scenario_deployment};
use super::scenario_platform::{fixed_agent_publication, mount_with_agent_source_on_lifecycle};
use super::{DelegatingModel, FAKE_ACP_JSONRPC_SCRIPT, scenario_model, scenario_shell_argv};

/// A Native coordinator whose roster proves Native, recursive-self, and ACP
/// delegated attempts use one parent-mediated child-Run path.
pub async fn build_delegation_router(
    trace_file_present: bool,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
) -> Router {
    build_delegation_router_with_deployment(
        scenario_deployment(),
        trace_file_present,
        lifecycle.clone(),
    )
    .await
}

/// Build the delegation scenario against one explicit typed deployment.
/// Deterministic in-process tests select Local here; real-process scenarios use
/// [`build_delegation_router`] and exercise their configured sandbox tier.
pub async fn build_delegation_router_with_deployment(
    deployment: awaken_deployment_config::DeploymentConfig,
    trace_file_present: bool,

    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
) -> Router {
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
            let mut tools = awaken_session_run_delivery::authorable_tools();
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
            let toolsets = (owner_id == "researcher")
                .then(|| ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: ToolExecutionPolicy::default(),
                    overrides: ["write", "bash"]
                        .into_iter()
                        .map(|name| {
                            ToolPolicyOverride::new(
                                name,
                                ToolExecutionPolicy {
                                    enabled: true,
                                    permission: ToolPermissionRequirement::AlwaysAsk,
                                },
                            )
                        })
                        .collect(),
                })
                .into_iter()
                .collect();
            // `model_ref` comes from `scenario_model` (possibly `ANTHROPIC_MODEL`),
            // so it is admitted and a bad reference fails at scenario startup.
            let binding =
                ModelBinding::parse("default", &model_ref, backend_ref).unwrap_or_else(|error| {
                    panic!("scenario model `{model_ref}` on backend `{backend_ref}`: {error}")
                });
            builder
                .model(binding)
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
                    // Causal fixture boundary: the frozen child publication,
                    // not a root Session override, owns the two permission
                    // decisions exercised by managed_delegation_e2e M7-M11.
                    // All ordinary Agent-tool defaults stay compatible with
                    // the official always-allow Managed Agent surface.
                    toolsets,
                    ..Default::default()
                })
                .build()
        };
    // The production config compiler is the one owner of the reserved Advisor
    // descriptor and its frozen binding. Reusing it here keeps the real-process
    // scenario from hand-maintaining a second copy of that capability.
    let tool_catalog = awaken_session_run_delivery::authorable_tools();
    let primary = ResolvedModelCandidate::host(
        ModelBinding::parse("default", &model_ref, "default")
            .unwrap_or_else(|error| panic!("scenario model `{model_ref}`: {error}")),
    );
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
            max_steps: Some(20),
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
        |descriptor| {
            !awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor(descriptor)
        },
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
    let platform = crate::acp_scenarios::bind_acp_session_environment(
        resource_host_with_deployment(model, model_ref, deployment, trace_file_present).await,
        awaken_execution_composition::LaunchSource::FixedAcp(Box::new(launch)),
        None,
    )
    .await
    .map_dependencies(|host| host.with_agent_publications(publication.clone()));
    mount_with_agent_source_on_lifecycle(platform, publication, lifecycle.clone()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ToolDescriptor;
    use awaken_session_run_delivery::{MemoryWriteProposals, authorable_tools};

    fn publication_catalogs() -> (Vec<ToolDescriptor>, Vec<ToolDescriptor>, ToolDescriptor) {
        let current = authorable_tools();
        let mut proposals = current
            .iter()
            .filter(|tool| MemoryWriteProposals::matches_descriptor(tool));
        let proposal = proposals
            .next()
            .expect("the real catalog offers the proposal")
            .clone();
        assert!(
            proposals.next().is_none(),
            "one canonical proposal catalog entry"
        );
        // Test preimage only: the former catalog is the same ordered payload
        // without this one capability, selected by its original owner matcher.
        let previous = current
            .iter()
            .filter(|tool| !MemoryWriteProposals::matches_descriptor(tool))
            .cloned()
            .collect();
        (current, previous, proposal)
    }

    fn compile_memory_publication(
        catalog: &[ToolDescriptor],
        exact: &[&str],
        patterns: &[&str],
    ) -> ExecutableAgentSnapshot {
        let primary = ResolvedModelCandidate::host(
            ModelBinding::parse("default", "fixture-memory-model", "default")
                .expect("literal publication model"),
        );
        let config = AgentConfig {
            id: "ordinary-memory-agent".into(),
            instructions: "An ordinary Agent with terminal Memory extraction.".into(),
            max_steps: Some(2),
            model_binding: ModelSelection::Pinned(primary.binding().clone()),
            tool_ids: exact.iter().map(|id| (*id).to_owned()).collect(),
            tool_patterns: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            plugin_ids: vec![awaken_ext_memory::MEMORY_PLUGIN_ID.into()],
            plugin_config: std::collections::BTreeMap::from([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.into(),
                serde_json::json!({"binding_id":"memory"}),
            )]),
            ..Default::default()
        };
        let metadata = AgentSnapshotMetadata {
            source: AgentConfigRevisionRef {
                agent_id: AgentId(config.id.clone()),
                revision: 1,
            },
            ..Default::default()
        };
        // This is the production compiler used by the fixture above, with its
        // current signature. No router, model inference or sandbox is created.
        compile_published(
            &config,
            catalog,
            metadata,
            primary,
            Vec::new(),
            None,
            |descriptor| {
                !awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor(descriptor)
            },
        )
        .expect("compile the real Memory publication inputs")
    }

    fn assert_ordinary_glob_matches_previous_catalog(
        current: &[ToolDescriptor],
        previous: &[ToolDescriptor],
        exact: &[&str],
        pattern: &str,
    ) {
        let expected = compile_memory_publication(previous, exact, &[pattern]);
        let actual = compile_memory_publication(current, exact, &[pattern]);
        assert_eq!(
            actual.fingerprint, expected.fingerprint,
            "G/P a newly available proposal must not change an ordinary Agent fingerprint"
        );
        assert_eq!(
            actual, expected,
            "G/P complete compiled publication remains unchanged"
        );
        assert!(
            MemoryWriteProposals::descriptor(&actual).is_none(),
            "G/P ordinary terminal extraction must not acquire the proposal role"
        );
    }

    #[test]
    fn published_exact_memory_proposal_is_retained() {
        // E: exact proposal + no glob => canonical descriptor retained once,
        // so the existing stateless executor/settlement owner remains selected.
        let (current, _, proposal) = publication_catalogs();
        let snapshot = compile_memory_publication(&current, &[&proposal.id], &[]);
        assert_eq!(
            MemoryWriteProposals::descriptor(&snapshot),
            Some(&proposal),
            "E exact owner"
        );
        assert_eq!(
            snapshot.resolved_spec.tool_descriptors.len(),
            1,
            "E no extra grant"
        );
    }

    #[test]
    fn published_catalog_glob_preserves_ordinary_memory_role() {
        // G1 MC/DC: exact absent, glob * present, catalog adds only proposal.
        // Effect: full snapshot/fingerprint equal the previous catalog, no
        // proposal role. The old compiler must fail this semantic comparison.
        let (current, previous, _) = publication_catalogs();
        assert_ordinary_glob_matches_previous_catalog(&current, &previous, &[], "*");
    }

    #[test]
    fn published_write_glob_preserves_ordinary_memory_role() {
        // G2 changes only the pattern from G1 to write*: the rule applies to
        // every glob that matches the capability, not one spelling of *.
        let (current, previous, _) = publication_catalogs();
        assert_ordinary_glob_matches_previous_catalog(&current, &previous, &[], "write*");
    }

    #[test]
    fn published_exact_memory_proposal_and_glob_are_deduplicated() {
        // M: exact proposal + glob * => exactly one proposal plus every ordinary
        // glob result in its original order. Exact intent wins over the glob
        // exclusion; a shared matcher, not a repeated id rule, proves its role.
        let (current, previous, proposal) = publication_catalogs();
        let ordinary = compile_memory_publication(&previous, &[], &["*"]);
        let snapshot = compile_memory_publication(&current, &[&proposal.id], &["*"]);
        assert_eq!(
            MemoryWriteProposals::descriptor(&snapshot),
            Some(&proposal),
            "M exact owner"
        );
        assert_eq!(
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|tool| MemoryWriteProposals::matches_descriptor(tool))
                .count(),
            1,
            "M exact and glob do not duplicate the proposal"
        );
        let expected = std::iter::once(&proposal)
            .chain(ordinary.resolved_spec.tool_descriptors.iter())
            .collect::<Vec<_>>();
        assert_eq!(
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .collect::<Vec<_>>(),
            expected,
            "M exact-first order and ordinary glob payload remain intact"
        );
    }

    #[test]
    fn published_ordinary_exact_and_glob_preserve_memory_role() {
        // P: one ordinary exact declaration + glob * must not imply proposal
        // intent. Compare complete old/new results and preserve exact dedup.
        let (current, previous, _) = publication_catalogs();
        let ordinary = previous
            .iter()
            .find(|tool| tool.kind == ToolKind::Regular)
            .expect("the real catalog has an ordinary tool");
        assert_ordinary_glob_matches_previous_catalog(&current, &previous, &[&ordinary.id], "*");
        let snapshot = compile_memory_publication(&current, &[&ordinary.id], &["*"]);
        assert_eq!(
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|tool| tool.id == ordinary.id)
                .count(),
            1,
            "P exact/glob dedup preserved"
        );
    }
}
