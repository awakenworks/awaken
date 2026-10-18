// @formal evidence memory.recall_config_contributes_authority_exactly_when_enabled
//! Skill and Memory/compaction extension assembly from one acquired context.

use super::{SemanticSkillTools, resources::ContextResources};
use crate::HostError;
use crate::compact::{
    compact_backend as build_compact_backend, compact_runner as build_compact_runner,
};
use crate::host::session::content_delivery;
use awaken_ext_compact::CompactPlugin;
use awaken_ext_skills::{SkillRegistry, SkillSpec};
use awaken_runtime::Runtime;
use awaken_runtime_contract::resolved::{ContextPolicy, ToolDescriptor};
use awaken_session_realization::peers::EnvironmentPeer as _;
use std::sync::Arc;

pub(super) struct SkillWiring {
    pub(super) runtime: Runtime,
    pub(super) skill_descriptors: Vec<ToolDescriptor>,
    pub(super) semantic_skill_tools: Option<SemanticSkillTools>,
    pub(super) skill_registry: Option<Arc<dyn SkillRegistry>>,
    pub(super) session_content_plugins: Vec<Arc<dyn awaken_runtime_contract::plugin::Plugin>>,
    pub(super) generated_skill_bindings: Option<Vec<awaken_agent_contract::AgentSkillBinding>>,
}

pub(super) async fn skills(
    host: &crate::RunAssembly,
    thread: &str,
    resources: &mut ContextResources,
    mut runtime: Runtime,
) -> Result<SkillWiring, HostError> {
    let realizations = host.realization_executor.sessions();
    let frozen_skill_versions = resources.plan.frozen_skill_versions.take();
    let installed = &resources.plan.installed;
    let workspace = &resources.plan.workspace;
    let session_dispatch = resources.plan.session_dispatch;
    let is_acp = resources.plan.execution_backend.is_acp();
    let env = &resources.env;
    let mcp = &resources.mcp;
    let authored_skills_subdir = &resources.authored_skills_subdir;
    let skill_source_roots = &resources.skill_source_roots;
    let content_delivery = resources.content_delivery;
    let base_gate = &resources.authorization.gate;
    let commit = &resources.commit;
    // One registry owns Skill discovery/body resolution. Managed Sessions
    // project exact attached bytes plus the realized-repository snapshot
    // only through the filesystem; direct callers may adapt the same
    // registry to the legacy two-tool contract.
    let mut skill_descriptors = Vec::new();
    let mut semantic_skill_tools = None;
    let mut skill_registry: Option<Arc<dyn SkillRegistry>> = None;
    let mut session_content_plugins: Vec<Arc<dyn awaken_runtime_contract::plugin::Plugin>> =
        Vec::new();
    // A Managed Session consumes exact frozen versions for attached Skills.
    // Its separate admitted repository source is derived below from the
    // already-realized Resource projection. Only a direct Session without a
    // frozen manifest may fall back to the live compatibility catalog.
    let delivered = frozen_skill_versions.or_else(|| {
        (!session_dispatch && host.capabilities.tools.skills.has_application())
            .then(|| host.capabilities.tools.skills.cache_snapshot_in(workspace))
    });
    // A published Agent receives exactly its selected Skills. Embedded direct
    // Sessions without a publication use the host-configured catalog.
    let selected_skills = content_delivery::published_skill_ids(installed.as_ref());
    let filtered_specs: Vec<SkillSpec> = if session_dispatch {
        // Managed attached-Skill truth is the resolved binding plus its
        // exact frozen version bytes. Host-static specs have neither and
        // remain a direct-session compatibility source only; realized
        // repository files enter through `skill_source_roots` instead.
        Vec::new()
    } else {
        match &selected_skills {
            Some(selected) => host
                .capabilities
                .tools
                .skills
                .specs()
                .iter()
                .filter(|skill| selected.contains(&skill.id))
                .cloned()
                .collect(),
            None => host.capabilities.tools.skills.specs().to_vec(),
        }
    };
    let filtered_delivered = match (delivered, &selected_skills) {
        (Some(skills), Some(selected)) => Some(
            skills
                .into_iter()
                .filter(|version| selected.contains(version.skill_id.as_str()))
                .collect(),
        ),
        (skills, None) => skills,
        (None, Some(_)) => None,
    };
    // An embedded/default Agent has no immutable publication to carry its
    // Session-selected host catalog. Freeze the exact configured/delivered
    // ids into the generated snapshot that crosses durable dispatch, so the
    // claimed rebuild applies the same selection filter instead of treating
    // an empty binding list as an explicit denial.
    let generated_skill_bindings = installed.is_none().then(|| {
        let ids = filtered_specs
            .iter()
            .map(|skill| skill.id.clone())
            .chain(
                filtered_delivered
                    .iter()
                    .flatten()
                    .map(|version| version.skill_id.to_string()),
            )
            .collect::<std::collections::BTreeSet<_>>();
        ids.iter()
            .map(|id| awaken_agent_contract::AgentSkillBinding::custom(id.clone()))
            .collect::<Vec<_>>()
    });
    if let Some(registry) = crate::skills::build_skill_registry(
        &filtered_specs,
        if session_dispatch {
            // Managed admission rejects prompts-as-skills, but keep the
            // construction boundary closed as well: lazy remote registries
            // cannot become a second Managed Skill authority after recovery.
            Vec::new()
        } else {
            mcp.skill_registries.clone()
        },
        filtered_delivered,
        env.clone(),
        authored_skills_subdir,
        skill_source_roots.as_deref(),
        content_delivery == crate::frozen_inputs::ManagedContentDelivery::ManagedFilesystem,
        session_dispatch,
    )
    .await
    .map_err(HostError::internal)?
    {
        if content_delivery == crate::frozen_inputs::ManagedContentDelivery::ManagedFilesystem {
            let prompt = crate::skill_delivery::filesystem_skill_prompt(registry.as_ref())
                .map_err(HostError::internal)?;
            realizations.update_realization(thread, |r| r.staging_mut().skill_prompt = prompt)?;
        } else {
            if session_dispatch {
                return Err(HostError::bad_request(
                    "Managed Agent Skills require filesystem progressive disclosure, but this Session disables every filesystem tool",
                ));
            }
            // The MCP-aware base gate keeps this direct Session's
            // pre-authorized MCP tools while the adapter narrows active
            // Skill tools. Managed Sessions never construct this value.
            // The context composes the existing fork tool; the registry adapter
            // receives that port and never selects another executor or clock.
            let agent_tool = env.as_ref().map(|env| {
                Arc::new(crate::skills::ForkAgentTool {
                    llm: host.capabilities.agent.llm.clone(),
                    model_ref: host.capabilities.agent.model_ref.clone(),
                    sandbox: env.clone(),
                    execution: commit.clone(),
                    clock: host.clock.clone(),
                }) as Arc<dyn awaken_tool_contract::tool::RawTool>
            });
            let adapter = crate::skills::semantic_skill_adapter(
                registry.clone(),
                thread,
                base_gate.clone(),
                agent_tool,
            );
            if installed.is_some() && !is_acp {
                session_content_plugins.push(Arc::new(
                    crate::session_tools::SessionToolPlugin::new(
                        "awaken.session.skills",
                        adapter.descriptors.clone(),
                        vec![adapter.list_tool.clone(), adapter.activate_tool.clone()],
                    )
                    .map_err(HostError::internal)?,
                ));
            }
            if !is_acp {
                runtime = runtime
                    .with_gate(adapter.gate)
                    .with_tool(adapter.list_tool.clone())
                    .with_tool(adapter.activate_tool.clone());
            }
            skill_descriptors = adapter.descriptors.clone();
            semantic_skill_tools = Some((
                adapter.descriptors,
                vec![adapter.list_tool, adapter.activate_tool],
            ));
        }
        skill_registry = Some(registry);
    } else {
        realizations.update_realization(thread, |r| r.staging_mut().skill_prompt = None)?;
    }
    Ok(SkillWiring {
        runtime,
        skill_descriptors,
        semantic_skill_tools,
        skill_registry,
        session_content_plugins,
        generated_skill_bindings,
    })
}

pub(super) struct MemoryWiring {
    pub(super) runtime: Runtime,
    pub(super) plugin_ids: Vec<String>,
    pub(super) acp_memory_recall: Option<awaken_ext_memory::MemoryRecall>,
    pub(super) context_policy: ContextPolicy,
}

pub(super) async fn memory(
    host: &crate::RunAssembly,
    thread: &str,
    resources: &ContextResources,
    mut runtime: Runtime,
) -> Result<MemoryWiring, HostError> {
    let installed = &resources.plan.installed;
    let fallback_model_ref = &resources.plan.fallback_model_ref;
    let publication_source = &resources.plan.publication_source;
    let workspace = &resources.plan.workspace;
    let commit = &resources.commit;
    // Memory recall is a plugin: it contributes a BeforeInference hook that
    // injects bounded recall as request-only context (never committed). Install
    // it and list its id so it is active for the run (G30).
    // Seed with host-registered plugins (e.g. the tool state machine via
    // `with_state_machine`), then append the per-run memory/compact plugins.
    let mut plugin_ids: Vec<String> = host.capabilities.tools.plugin_ids.clone();
    let authored_memory = installed
        .as_ref()
        .and_then(|snapshot| {
            snapshot
                .resolved_spec
                .plugin_config
                .get(awaken_ext_memory::MEMORY_PLUGIN_ID)
        })
        .and_then(|value| awaken_ext_memory::MemoryConfig::from_value(Some(value)).ok())
        .unwrap_or_default();
    let published_memory_selected = installed.as_ref().is_some_and(|snapshot| {
        snapshot
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_memory::MEMORY_PLUGIN_ID)
    });
    let selected_memory = if installed.is_some() {
        let binding_id = if published_memory_selected {
            Some(authored_memory.binding_id.as_deref().ok_or_else(|| {
                HostError::bad_request(
                    "the Awaken memory extension requires an explicit `memory.binding_id`",
                )
            })?)
        } else {
            None
        };
        host.realization_executor
            .staging()
            .select_thread_memory_binding(thread, binding_id)?
            .map(|binding| host.resources.memory.bound(binding))
    } else {
        // Direct embedders opt in through `bind_resolved_memory`; Managed
        // compatibility Sessions merely register standard mount bindings.
        host.memory_for_thread(thread)
    };
    // Recovery is owned by the frozen Resource bindings, not by whichever
    // Memory plugin the parent Agent selects. This also resumes delegated
    // child intents when the parent itself has no automatic Memory plugin.
    host.bind_thread_memory_recovery(thread, commit.clone())
        .await?;
    let recalled_memory = selected_memory
        .clone()
        .filter(|memory| memory.binding().recall_enabled && authored_memory.recall_enabled);
    let memory_selector = recalled_memory
        .as_ref()
        .map(
            |_| -> Result<Arc<dyn awaken_ext_memory::RecallSelector>, HostError> {
                let agent_id = authored_memory
                    .selector_agent_id
                    .as_deref()
                    .unwrap_or(awaken_ext_memory::SELECTOR_AGENT_ID);
                let fallback = awaken_ext_memory::default_selector_agent(
                    fallback_model_ref,
                    authored_memory
                        .selector_instructions
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(awaken_ext_memory::DEFAULT_SELECTOR_INSTRUCTIONS),
                )
                .map_err(|error| {
                    HostError::bad_request(format!(
                        "host default model reference `{fallback_model_ref}` for the \
                         Memory selector is not an executable binding: {error}"
                    ))
                })?;
                let snapshot = crate::agent_catalog::resolve_auxiliary_snapshot(
                    Some(publication_source.as_ref()),
                    workspace,
                    agent_id,
                    fallback,
                    authored_memory.selector_instructions.as_deref(),
                )
                .map_err(HostError::bad_request)?;
                Ok(Arc::new(crate::memory::AgentSelector::new(
                    host.capabilities.agent.llm.clone(),
                    snapshot,
                    commit.clone(),
                    host.clock.clone(),
                    host.realization_executor
                        .environments()
                        .providers()
                        .auxiliary_sandboxes
                        .provider("mem-select"),
                ))
                    as Arc<dyn awaken_ext_memory::RecallSelector>)
            },
        )
        .transpose()?;
    let acp_memory_recall = recalled_memory.as_ref().map(|mem| {
        let recall =
            awaken_ext_memory::MemoryRecall::new(mem.store(), authored_memory.recall.clone());
        match &memory_selector {
            Some(selector) => recall.with_selector(selector.clone()),
            None => recall,
        }
    });
    if let Some(mem) = selected_memory {
        let mut plugin = awaken_ext_memory::MemoryPlugin::from_handle(
            mem.store(),
            authored_memory.recall.clone(),
        );
        if let Some(selector) = &memory_selector {
            plugin = plugin.with_selector(selector.clone());
        }
        runtime = runtime.with_plugin(Arc::new(plugin));
        plugin_ids.push(awaken_ext_memory::MEMORY_PLUGIN_ID.to_string());
    }
    let (runtime, context_policy) = compact(host, thread, resources, runtime, &mut plugin_ids)?;
    Ok(MemoryWiring {
        runtime,
        plugin_ids,
        acp_memory_recall,
        context_policy,
    })
}

fn compact(
    host: &crate::RunAssembly,
    thread: &str,
    resources: &ContextResources,
    mut runtime: Runtime,
    plugin_ids: &mut Vec<String>,
) -> Result<(Runtime, ContextPolicy), HostError> {
    let installed = &resources.plan.installed;
    let fallback_model_ref = &resources.plan.fallback_model_ref;
    let publication_source = &resources.plan.publication_source;
    let workspace = &resources.plan.workspace;
    let commit = &resources.commit;
    // Compaction is a plugin too: a BeforeInference hook that folds the older
    // slice into a summary and injects it request-only. A successful fold
    // activates its matching Run-scoped window; before that history stays whole.
    // The selected plugin id/config pair is the sole compaction authority in
    // both an immutable publication and the generated dispatch snapshot. In
    // particular, a claimed Worker must not reconstruct host defaults after
    // the coordinator froze a non-default threshold.
    let compact_source = installed
        .as_ref()
        .map(|snapshot| &snapshot.resolved_spec)
        .filter(|spec| {
            spec.plugin_ids
                .iter()
                .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID)
        });
    let compact_config = match compact_source {
        Some(spec) => Some(
            spec.plugin_config
                .get(awaken_ext_compact::COMPACT_PLUGIN_ID)
                .cloned()
                .map(serde_json::from_value::<awaken_ext_compact::CompactConfig>)
                .transpose()
                .map_err(|error| {
                    HostError::bad_request(format!(
                        "invalid published compact plugin config: {error}"
                    ))
                })?
                .unwrap_or_default(),
        ),
        None if installed.is_none()
            && host
                .capabilities
                .tools
                .plugin_ids
                .iter()
                .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID) =>
        {
            Some(
                host.capabilities
                    .tools
                    .plugin_config
                    .get(awaken_ext_compact::COMPACT_PLUGIN_ID)
                    .cloned()
                    .map(serde_json::from_value::<awaken_ext_compact::CompactConfig>)
                    .transpose()
                    .map_err(|error| {
                        HostError::bad_request(format!(
                            "invalid host compact plugin config: {error}"
                        ))
                    })?
                    .unwrap_or_default(),
            )
        }
        None => None,
    };
    let context_policy = match compact_config {
        Some(compact_config) => {
            let fallback = awaken_ext_compact::default_compact_agent(
                fallback_model_ref,
                compact_config
                    .agent_instructions
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(awaken_ext_compact::DEFAULT_COMPACT_INSTRUCTIONS),
            )
            .map_err(|error| {
                HostError::bad_request(format!(
                    "host default model reference `{fallback_model_ref}` for the \
                     compactor is not an executable binding: {error}"
                ))
            })?;
            let snapshot = crate::agent_catalog::resolve_auxiliary_snapshot(
                Some(publication_source.as_ref()),
                workspace,
                &compact_config.agent_id,
                fallback,
                compact_config.agent_instructions.as_deref(),
            )
            .map_err(HostError::bad_request)?;
            let agent_tool = build_compact_runner(
                host.capabilities.agent.llm.clone(),
                snapshot,
                commit.clone(),
                host.clock.clone(),
                host.realization_executor
                    .environments()
                    .providers()
                    .auxiliary_sandboxes
                    .provider("compact"),
            );
            let backend = build_compact_backend(agent_tool, host.resources.memory.background());
            let plugin = CompactPlugin::new(compact_config).with_backend(thread, backend);
            runtime = runtime.with_plugin(Arc::new(plugin));
            if !plugin_ids
                .iter()
                .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID)
            {
                plugin_ids.push(awaken_ext_compact::COMPACT_PLUGIN_ID.to_string());
            }
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll
        }
        None => awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    };
    Ok((runtime, context_policy))
}
