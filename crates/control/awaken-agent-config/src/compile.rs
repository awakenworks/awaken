//! Compilation: a pure config → executable-snapshot function.

use awaken_runtime_contract::resolved::ResolvedModelCandidate;
use awaken_runtime_contract::resolved::{ToolDescriptor, ToolKind};
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_tool_pattern::tool_id_match;

use crate::config::{AgentConfig, AgentKind, MultiagentConfig};

mod agent_bindings;
mod executable_projection;
mod fingerprint;
mod processing_geography;
mod tool_presentation;

use fingerprint::fingerprint_of;

/// A compilation failure, before anything is published (the design's Failure
/// Rules: reject, never partially publish).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompileError {
    #[error("agent {agent} references unknown tool {tool:?}")]
    UnknownTool { agent: String, tool: String },
    #[error("serialize config: {0}")]
    Serialize(String),
    #[error("invalid resolution manifest: {0}")]
    InvalidResolution(String),
    /// The config's model is still `ModelSelection::Auto` (ADR-0052 D5): compile is
    /// pure and cannot reach the provider catalog, so an `Auto` binding must be
    /// resolved to a concrete one *before* compile (publish does this). Fail-closed.
    #[error("agent {agent} has an unresolved (auto) model binding; resolve it before compiling")]
    UnresolvedModel { agent: String },
    /// A tool override (ADR-0053) is inconsistent: its `target` names no selected tool
    /// (and is not an MCP id), or its `alias` collides with another tool's model-facing
    /// id. Fail-closed — a presentation that would show the model a phantom or duplicate
    /// tool is rejected at compile, not silently dropped.
    #[error("agent {agent} has an invalid tool override: {reason}")]
    InvalidToolOverride { agent: String, reason: String },
    #[error("agent {agent} has an invalid tool-exposure policy: {reason}")]
    InvalidToolExposure { agent: String, reason: String },
    #[error("agent {agent} has invalid tool-discovery settings: {reason}")]
    InvalidToolDiscovery { agent: String, reason: String },
    #[error("agent {agent} has an invalid tool recovery policy: {reason}")]
    InvalidToolRecovery { agent: String, reason: String },
    /// A Managed-Agents integration union cannot be normalized into an executable
    /// binding. Reject it at publish instead of preserving UI-only configuration.
    #[error("agent {agent} has an invalid {axis} binding: {reason}")]
    InvalidBinding {
        agent: String,
        axis: &'static str,
        reason: String,
    },
    /// The config declares a capability its execution kind cannot honor (ADR-0057 D2):
    /// an A2A (remote) agent owns its own skills/MCP on the far side, so declaring them
    /// locally is a silent no-op at runtime — rejected at publish instead. `axis` is the
    /// unsupported capability (`"skills"` / `"mcp_servers"`).
    #[error("agent {agent} is a remote (a2a) agent and cannot honor local {axis}")]
    UnsupportedCapability { agent: String, axis: &'static str },
    #[error("agent {agent} has inconsistent published model candidates: {reason}")]
    InvalidResolvedModels { agent: String, reason: String },
}

impl CompileError {
    /// The config field this error is about — the domain saying which part failed, so the
    /// validate surface can route the issue to the right section instead of re-deriving it
    /// from the message string. `""` means the whole config.
    #[must_use]
    pub fn field_path(&self) -> &'static str {
        match self {
            CompileError::UnknownTool { .. } => "tools",
            CompileError::UnresolvedModel { .. } => "model",
            CompileError::InvalidToolOverride { .. } => "tool_overrides",
            CompileError::InvalidToolExposure { .. } => "tool_exposure",
            CompileError::InvalidToolDiscovery { .. } => "tool_discovery",
            CompileError::InvalidToolRecovery { .. } => "recovery_policies",
            CompileError::InvalidBinding { axis, .. } => axis,
            CompileError::UnsupportedCapability { axis, .. } => axis,
            CompileError::InvalidResolvedModels { .. } => "model",
            CompileError::Serialize(_) | CompileError::InvalidResolution(_) => "",
        }
    }
}

/// Assemble the one immutable publication produced by configuration resolution.
/// `metadata` records every input the configuration plane read; runtime code never
/// calls this function and therefore cannot re-resolve or broaden those inputs.
pub fn compile_resolved(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
    metadata: AgentSnapshotMetadata,
) -> Result<ExecutableAgentSnapshot, CompileError> {
    let primary = config
        .model_binding
        .resolved()
        .cloned()
        .map(ResolvedModelCandidate::host);
    let candidates = config
        .model_fallbacks
        .iter()
        .cloned()
        .map(ResolvedModelCandidate::host)
        .collect();
    compile_with_models(config, tools, metadata, primary, candidates, None)
}

/// Compile a publication whose complete model candidates were resolved by the
/// configuration plane. This is the production publish path; direct examples use
/// [`compile_resolved`] and receive explicit host-executor candidates.
pub fn compile_published(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
    metadata: AgentSnapshotMetadata,
    primary: ResolvedModelCandidate,
    candidates: Vec<ResolvedModelCandidate>,
    advisor: Option<ResolvedModelCandidate>,
) -> Result<ExecutableAgentSnapshot, CompileError> {
    compile_with_models(config, tools, metadata, Some(primary), candidates, advisor)
}

fn compile_with_models(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
    mut metadata: AgentSnapshotMetadata,
    primary: Option<ResolvedModelCandidate>,
    candidates: Vec<ResolvedModelCandidate>,
    advisor: Option<ResolvedModelCandidate>,
) -> Result<ExecutableAgentSnapshot, CompileError> {
    if let Some(configuration) = config.model_binding.acp_configuration()
        && let Err(reason) = configuration.validate_working_directory()
    {
        return Err(CompileError::InvalidResolvedModels {
            agent: config.id.clone(),
            reason: reason.into(),
        });
    }
    config
        .validate_tool_bindings()
        .map_err(|reason| CompileError::InvalidBinding {
            agent: config.id.clone(),
            axis: "tools",
            reason,
        })?;
    let mut descriptors = Vec::with_capacity(config.tool_ids.len());
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Exact tool ids: each must resolve (unknown references are rejected, fail-closed).
    for id in &config.tool_ids {
        let descriptor =
            tools
                .iter()
                .find(|t| &t.id == id)
                .ok_or_else(|| CompileError::UnknownTool {
                    agent: config.id.clone(),
                    tool: id.clone(),
                })?;
        descriptors.push(descriptor.clone());
        seen.insert(id.clone());
    }
    // Toolsets are resolved against the exact publication catalog. Unsupported
    // built-ins become disabled in the snapshot; enabled supported members join
    // the same descriptor set as exact `tool_ids` (one execution path).
    let resolved_toolsets = agent_bindings::resolve_toolsets(config, tools)?;
    for toolset in &resolved_toolsets {
        if toolset.source != awaken_runtime_contract::agent_bindings::ToolsetSource::Agent {
            continue;
        }
        for entry in &toolset.overrides {
            if !entry.policy.enabled || !seen.insert(entry.name.clone()) {
                continue;
            }
            if let Some(descriptor) = tools.iter().find(|tool| tool.id == entry.name) {
                descriptors.push(descriptor.clone());
            }
        }
    }
    // Client-executed tools are exact inline capabilities, never aliases for a
    // host executor. Reject identity overlap instead of choosing one execution
    // owner based on insertion order.
    for descriptor in &config.client_tools {
        if descriptor.kind != ToolKind::ClientExecuted {
            return Err(CompileError::InvalidBinding {
                agent: config.id.clone(),
                axis: "client_tools",
                reason: format!("tool {:?} is not client_executed", descriptor.id),
            });
        }
        if !seen.insert(descriptor.id.clone()) {
            return Err(CompileError::InvalidBinding {
                agent: config.id.clone(),
                axis: "client_tools",
                reason: format!("duplicate tool identity {:?}", descriptor.id),
            });
        }
        descriptors.push(descriptor.clone());
    }
    // Glob patterns: permissively add catalog tools whose id matches, in catalog
    // order, skipping any already selected. A pattern that matches nothing is not
    // an error — it is a filter over the catalog, not a reference to a tool.
    for descriptor in tools {
        if seen.contains(&descriptor.id) {
            continue;
        }
        if config
            .tool_patterns
            .iter()
            .any(|pattern| tool_id_match(pattern, &descriptor.id))
        {
            descriptors.push(descriptor.clone());
            seen.insert(descriptor.id.clone());
        }
    }

    // A provider-server tool executes inside the model provider. Awaken never
    // receives a callable function boundary that it could durably pause before
    // dispatch, so `always_ask` would be a false safety promise. Reject that
    // combination during publication for every execution backend; selecting an
    // Awaken-hosted provider is the explicit way to retain per-call HITL.
    if let Some(agent_toolset) = resolved_toolsets.iter().find(|policy| {
        policy.source == awaken_runtime_contract::agent_bindings::ToolsetSource::Agent
    }) {
        for descriptor in &descriptors {
            let policy = agent_toolset.policy_for(&descriptor.id);
            if descriptor.provider_server_tool.is_some()
                && policy.enabled
                && matches!(
                    policy.permission,
                    awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk
                )
            {
                return Err(CompileError::InvalidBinding {
                    agent: config.id.clone(),
                    axis: "tools",
                    reason: format!(
                        "PROVIDER_SERVER_TOOL_APPROVAL_UNSUPPORTED: provider-native tool {:?} cannot satisfy per-call Awaken approval; select an Awaken-hosted provider or use always_allow",
                        descriptor.id
                    ),
                });
            }
        }
    }

    // Delegation capability follows the typed target declaration. Authors never
    // need to repeat `agent_run` in `tool_ids`, and a target-less Agent cannot
    // accidentally publish the delegation tool. The semantic role, not a concrete
    // builtin id, joins the config domain to the extension catalog.
    let has_delegation_targets = config
        .multiagent
        .as_ref()
        .is_some_and(MultiagentConfig::has_delegation_target);
    if has_delegation_targets {
        let mut delegation = tools
            .iter()
            .filter(|tool| tool.kind == ToolKind::AgentDelegation);
        let descriptor = delegation
            .next()
            .ok_or_else(|| CompileError::InvalidBinding {
                agent: config.id.clone(),
                axis: "multiagent",
                reason: "the tool catalog provides no Agent-delegation capability".into(),
            })?;
        if delegation.next().is_some() {
            return Err(CompileError::InvalidBinding {
                agent: config.id.clone(),
                axis: "multiagent",
                reason: "the tool catalog provides more than one Agent-delegation capability"
                    .into(),
            });
        }
        if seen.insert(descriptor.id.clone()) {
            descriptors.push(descriptor.clone());
        }
    } else {
        descriptors.retain(|tool| tool.kind != ToolKind::AgentDelegation);
    }

    let has_advisor = config
        .multiagent
        .as_ref()
        .is_some_and(MultiagentConfig::has_advisor_target);
    if has_advisor {
        if !seen.insert(awaken_runtime_contract::resolved::ADVISOR_TOOL_ID.into()) {
            return Err(CompileError::InvalidBinding {
                agent: config.id.clone(),
                axis: "multiagent",
                reason: "advisor reserved tool identity collides with an authored tool".into(),
            });
        }
        descriptors.push(
            ToolDescriptor::pinned(
                "managed:advisor",
                awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                "Consult the configured advisor model for a second opinion before continuing.",
                serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            )
            .with_kind(ToolKind::Advisor)
            // The host materializes an advisor consultation as an ordinary
            // durable child request. ToolBatch recovery reconnects to that
            // request; it never samples a second inline advisor executor.
            .with_recovery(awaken_runtime_contract::tool::ToolRecoveryPolicy::durable_request()),
        );
    }

    // Execution recovery is keyed by canonical identity and is resolved into the
    // descriptor carried by the executable snapshot. Capability is intentionally
    // not trusted here: only the executor can attest it, so runtime resolution/
    // recovery performs the fail-closed capability check.
    for (target, policy) in &config.recovery_policies {
        let Some(index) = descriptors.iter().position(|d| &d.id == target) else {
            return Err(CompileError::InvalidToolRecovery {
                agent: config.id.clone(),
                reason: format!("target {target:?} is not a selected static tool"),
            });
        };
        descriptors[index] = descriptors[index].clone().with_recovery(policy.clone());
    }

    let presentation = tool_presentation::compile(config, &descriptors)?;

    // The model must be concrete by now: `Auto` is resolved to a first-offering in
    // `ConfigService::publish` before compile (ADR-0052 D5). A bare compile of an
    // `Auto` config is fail-closed (`UnresolvedModel`), never a silent empty binding.
    let authored_model = config
        .model_binding
        .resolved()
        .ok_or_else(|| CompileError::UnresolvedModel {
            agent: config.id.clone(),
        })?
        .clone();
    let model = primary.ok_or_else(|| CompileError::UnresolvedModel {
        agent: config.id.clone(),
    })?;
    if model.binding() != &authored_model {
        return Err(CompileError::InvalidResolvedModels {
            agent: config.id.clone(),
            reason: "primary candidate does not match the resolved authoring binding".into(),
        });
    }
    let authored_candidates = &config.model_fallbacks;
    if candidates.len() != authored_candidates.len()
        || candidates
            .iter()
            .zip(authored_candidates)
            .any(|(resolved, authored)| resolved.binding() != authored)
    {
        return Err(CompileError::InvalidResolvedModels {
            agent: config.id.clone(),
            reason: "fallback candidates do not match the resolved authoring order".into(),
        });
    }
    if let Some(advisor) = &advisor
        && let Some(existing) = std::iter::once(&model)
            .chain(candidates.iter())
            .find(|candidate| candidate.binding() == advisor.binding())
        && existing != advisor
    {
        return Err(CompileError::InvalidResolvedModels {
            agent: config.id.clone(),
            reason: "advisor and model pool resolve the same binding to different provisioning"
                .into(),
        });
    }

    processing_geography::validate_candidate_pool(
        config.inference.inference_geo,
        &config.id,
        &model,
        &candidates,
        advisor.as_ref(),
    )?;

    // Capability gate (ADR-0057 D2): the execution kind — derived from the now-concrete
    // `backend_ref` — must be able to honor the declared capabilities. A remote (a2a)
    // agent runs everything on the far side, so local skills/MCP would be a silent
    // runtime no-op; reject at publish so the mistake surfaces at authoring time.
    if matches!(config.kind(), AgentKind::A2a(_)) {
        if !config.skills.is_empty() {
            return Err(CompileError::UnsupportedCapability {
                agent: config.id.clone(),
                axis: "skills",
            });
        }
        if !config.mcp_servers.is_empty() {
            return Err(CompileError::UnsupportedCapability {
                agent: config.id.clone(),
                axis: "mcp_servers",
            });
        }
    }
    // Background tool execution retains an identity-bound prepared executor in
    // the Native process. An external ACP or outbound A2A backend cannot honor
    // that local executor contract.
    // Reject the incompatible publication here instead of allowing a reviewed
    // Agent to fail only when its first Session is constructed.
    if matches!(config.kind(), AgentKind::Acp(_) | AgentKind::A2a(_))
        && config.plugin_ids.iter().any(|id| id == "background_task")
    {
        return Err(CompileError::UnsupportedCapability {
            agent: config.id.clone(),
            axis: "background_task",
        });
    }
    // State-machine transitions are executed by the Native model/tool loop.
    // ACP owns that loop externally, so merely exporting tools cannot make the
    // transition engine authoritative. Reject instead of publishing a silent
    // no-op configuration.
    if matches!(config.kind(), AgentKind::Acp(_) | AgentKind::A2a(_))
        && config.plugin_ids.iter().any(|id| id == "state_machine")
    {
        return Err(CompileError::UnsupportedCapability {
            agent: config.id.clone(),
            axis: "state_machine",
        });
    }

    // The normalized binding consumes the resolved advisor candidate, while
    // the content address must independently prove that exact route. Retain one
    // immutable copy for fingerprinting; runtime never re-resolves it.
    let advisor_for_fingerprint = advisor.clone();
    let bindings = agent_bindings::normalize(config, resolved_toolsets, advisor)?;

    if !metadata.is_legacy_default() {
        let mut inputs = std::mem::take(&mut metadata.resolution.inputs);
        inputs.extend(
            descriptors
                .iter()
                .map(|tool| awaken_runtime_contract::ResolvedInputRef {
                    kind: "tool".into(),
                    id: tool.id.clone(),
                    version: awaken_runtime_contract::ResolvedInputVersion::ContentHash(
                        tool.content_hash(),
                    ),
                }),
        );
        metadata.resolution = awaken_runtime_contract::ResolutionManifest::new(inputs)
            .map_err(|error| CompileError::InvalidResolution(error.to_string()))?;
    }

    // Compile authoring integrations into the typed runtime contract. Empty
    // bindings are explicit capability absence. Only executable plugin sections
    // enter the snapshot/content address; inactive authoring residue remains in
    // AgentConfig for lossless editing but cannot perturb runtime identity.
    let mut executable_config = config.clone();
    executable_config.plugin_config = executable_projection::plugin_config(config);
    let fingerprint = fingerprint_of(
        &executable_config,
        &descriptors,
        &metadata,
        &model,
        &candidates,
        advisor_for_fingerprint.as_ref(),
    )?;
    Ok(ExecutableAgentSnapshot::builder(&config.id)
        .instructions(config.instructions.clone())
        .resolved_model(model)
        .resolved_model_candidates(candidates)
        .max_steps(config.max_steps)
        .delegation_limits(config.delegation_limits)
        .tools(descriptors)
        .plugins(config.plugin_ids.clone())
        .plugin_config(executable_config.plugin_config)
        .agent_bindings(bindings)
        .inference_options(config.inference.clone())
        .context_policy(config.context_policy.clone())
        .tool_presentation(presentation)
        .fingerprint(fingerprint)
        .metadata(metadata)
        .build())
}

#[cfg(test)]
#[path = "compile/tests.rs"]
mod tests;
