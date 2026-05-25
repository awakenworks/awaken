//! Resolution pipeline: `agent_id` + `RegistrySet` -> `ResolvedAgent` /
//! `ExecutionPlan`.

mod catalog;
mod filter;
mod manifest;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::error::RuntimeError;
use crate::execution::SequentialToolExecutor;
use crate::phase::ExecutionEnv;
use crate::plugins::Plugin;
#[cfg(feature = "a2a")]
use crate::registry::ResolvedBackendAgent;
use crate::registry::{AgentResolver, ResolvedAgent};
use crate::resolution::{
    BackendProfile, BackendRequirements, ExecutionPlan, ExecutionRole, LiveOnlyScope,
    ReplayableScope, ResolutionRequest, ResolutionTarget, ResolveError as RunResolveError,
    ResolvedModelBinding, ResolvedRun, ResolvedRunPlan, ResolvedTool, Resolver,
};
use async_trait::async_trait;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::run::RunResolutionScope;
use awaken_contract::contract::tool::Tool;
use awaken_contract::contract::versioned_registry::{PinnedRegistryEntry, PinnedRegistryManifest};
use awaken_contract::registry_spec::{AgentSpec, ModelSpec};
use awaken_contract::{REGISTRY_KIND_AGENT, REGISTRY_KIND_MODEL_POOL};

use crate::registry::snapshot::RegistryHandle;
use crate::registry::traits::RegistrySet;
use manifest::{collect_model_manifest_entries, insert_manifest_entry};

use self::filter::filter_tools;
use super::error::ResolveError;

// ---------------------------------------------------------------------------
// inject_default_plugins()
// ---------------------------------------------------------------------------

/// Inject runtime-required default plugins into a plugin list.
///
/// These plugins are always needed for the agent loop to function correctly.
/// Called from both the resolve pipeline and `build_agent_env()`.
pub(crate) fn inject_default_plugins(
    mut plugins: Vec<Arc<dyn Plugin>>,
    max_rounds: usize,
) -> Vec<Arc<dyn Plugin>> {
    plugins.push(Arc::new(
        crate::loop_runner::actions::LoopActionHandlersPlugin,
    ));
    plugins.push(Arc::new(crate::policies::MaxRoundsPlugin::new(max_rounds)));
    plugins
}

// ---------------------------------------------------------------------------
// resolve()
// ---------------------------------------------------------------------------

/// Resolve an agent by ID from registries into a fully wired local [`ResolvedAgent`].
///
/// Three-stage pipeline:
/// 1. **Lookup** — fetch spec, model, executor from registries.
/// 2. **Plugin pipeline** — resolve plugins, inject defaults, validate config.
/// 3. **Tool pipeline** — collect global + delegate + plugin tools, filter.
pub(crate) fn resolve_registry_set(
    registries: &RegistrySet,
    agent_id: &str,
) -> Result<ResolvedAgent, ResolveError> {
    // Stage 1: Lookup
    let spec = lookup_spec(registries, agent_id)?;
    #[cfg(feature = "a2a")]
    if spec.endpoint.is_some() {
        return Err(ResolveError::RemoteAgentNotDirectlyRunnable(
            spec.id.clone(),
        ));
    }
    let (executor, upstream_model, model) = resolve_model_and_executor(registries, &spec)?;

    // Stage 2: Plugin pipeline
    let plugins = build_plugin_chain(registries, &spec, &model)?;
    let env = ExecutionEnv::from_plugins(&plugins, &spec.active_hook_filter)?;

    // Stage 3: Tool pipeline
    let tools = build_tool_set(registries, &spec, &env)?;

    // Build ResolvedAgent with all fields
    let spec_arc = Arc::new(spec);

    Ok(ResolvedAgent {
        spec: spec_arc,
        upstream_model,
        tools,
        llm_executor: executor,
        tool_executor: Arc::new(SequentialToolExecutor),
        context_summarizer: None,
        #[cfg(feature = "background")]
        background_manager: None,
        stream_checkpoint_store: None,
        env,
    })
}

/// Resolve an agent into a local or non-local execution plan.
pub(crate) fn resolve_execution_registry_set(
    registries: &RegistrySet,
    agent_id: &str,
) -> Result<ExecutionPlan, ResolveError> {
    let spec = lookup_spec(registries, agent_id)?;

    #[cfg(feature = "a2a")]
    if let Some(endpoint) = spec.endpoint.clone() {
        let factory = registries
            .backends
            .get_backend_factory(&endpoint.backend)
            .ok_or_else(|| ResolveError::UnsupportedRemoteBackend {
                agent_id: spec.id.clone(),
                backend: endpoint.backend.clone(),
            })?;
        factory
            .validate(&endpoint)
            .map_err(|error| ResolveError::InvalidRemoteEndpointConfig {
                agent_id: spec.id.clone(),
                backend: endpoint.backend.clone(),
                message: error.to_string(),
            })?;
        return Ok(ExecutionPlan::Remote(ResolvedBackendAgent::with_factory(
            Arc::new(spec),
            factory,
            endpoint,
        )));
    }

    resolve_local_spec(registries, spec).map(|agent| ExecutionPlan::from_resolved_agent(&agent))
}

#[cfg(test)]
fn resolve(registries: &RegistrySet, agent_id: &str) -> Result<ResolvedAgent, ResolveError> {
    resolve_registry_set(registries, agent_id)
}

// ---------------------------------------------------------------------------
// Stage 1: Lookup
// ---------------------------------------------------------------------------

/// Fetch and validate the agent spec from registry.
fn lookup_spec(registries: &RegistrySet, agent_id: &str) -> Result<AgentSpec, ResolveError> {
    registries
        .agents
        .get_agent(agent_id)
        .ok_or_else(|| ResolveError::AgentNotFound(agent_id.into()))
}

fn resolve_local_spec(
    registries: &RegistrySet,
    spec: AgentSpec,
) -> Result<ResolvedAgent, ResolveError> {
    let (executor, upstream_model, model) = resolve_model_and_executor(registries, &spec)?;
    let plugins = build_plugin_chain(registries, &spec, &model)?;
    let env = ExecutionEnv::from_plugins(&plugins, &spec.active_hook_filter)?;
    let tools = build_tool_set(registries, &spec, &env)?;
    let spec_arc = Arc::new(spec);

    Ok(ResolvedAgent {
        spec: spec_arc,
        upstream_model,
        tools,
        llm_executor: executor,
        tool_executor: Arc::new(SequentialToolExecutor),
        context_summarizer: None,
        #[cfg(feature = "background")]
        background_manager: None,
        stream_checkpoint_store: None,
        env,
    })
}

/// Resolve model and LLM executor, applying the agent retry policy.
///
/// Returns the resolved executor, upstream model name, and the resolved
/// [`ModelSpec`] so downstream stages (e.g. `build_plugin_chain`) can use
/// the model's capabilities without re-querying the registry.
fn resolve_model_and_executor(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<(Arc<dyn LlmExecutor>, String, ModelSpec), ResolveError> {
    let policy = spec
        .config::<crate::engine::RetryConfigKey>()
        .map_err(|error| match error {
            awaken_contract::StateError::KeyDecode { key, message } => {
                ResolveError::InvalidPluginConfig {
                    plugin: "retry".into(),
                    key,
                    message,
                }
            }
            other => ResolveError::EnvBuild(other),
        })?;

    // A model id may name a pool; pools share the model id namespace and the
    // agent id is the deterministic home key.
    if let Some(pool) = registries.models.get_pool(&spec.model_id) {
        return super::pool::build_pool_executor(registries, &pool, &spec.id, &policy);
    }

    let model = registries
        .models
        .get_model(&spec.model_id)
        .ok_or_else(|| ResolveError::ModelNotFound(spec.model_id.clone()))?;

    let executor = registries
        .providers
        .get_provider(&model.provider_id)
        .ok_or_else(|| ResolveError::ProviderNotFound(model.provider_id.clone()))?;

    let executor = if policy.max_retries > 0 {
        Arc::new(crate::engine::RetryingExecutor::new(executor, policy)) as Arc<dyn LlmExecutor>
    } else {
        executor
    };

    let upstream_model = model.upstream_model.clone();
    Ok((executor, upstream_model, model))
}

// ---------------------------------------------------------------------------
// Stage 2: Plugin pipeline
// ---------------------------------------------------------------------------

/// Resolve plugins by ID, inject defaults, add conditional plugins, validate.
///
/// `model` is the already-resolved [`ModelSpec`] from stage 1; the conditional
/// context-policy plugins clamp against its capabilities without re-querying
/// the registry.
fn build_plugin_chain(
    registries: &RegistrySet,
    spec: &AgentSpec,
    model: &ModelSpec,
) -> Result<Vec<Arc<dyn Plugin>>, ResolveError> {
    // User-declared plugins
    let plugins = resolve_plugins(registries, spec)?;

    // Runtime-required default plugins
    let mut plugins = inject_default_plugins(plugins, spec.max_rounds);

    // Conditional plugins (only when context_policy is set)
    if let Some(ref policy) = spec.context_policy {
        let effective = crate::context::effective_policy(policy, model);
        let compaction_config = spec
            .config::<crate::context::CompactionConfigKey>()
            .unwrap_or_default();
        plugins.push(Arc::new(crate::context::CompactionPlugin::new(
            compaction_config,
        )));
        plugins.push(Arc::new(crate::context::ContextTransformPlugin::new(
            effective,
        )));
    }

    // Validate spec sections against plugin-declared schemas
    validate_sections(spec, &plugins)?;

    Ok(plugins)
}

// ---------------------------------------------------------------------------
// Stage 3: Tool pipeline
// ---------------------------------------------------------------------------

/// Collect tools from all sources, detect conflicts, apply filters.
///
/// Tool sources (merged in order):
/// 1. Global tools from `ToolRegistry` (builder-registered)
/// 2. Delegate agent tools (A2A, created from `spec.delegates`)
/// 3. Plugin-registered tools (from `ExecutionEnv`)
///
/// After merging, `allowed_tools`/`excluded_tools` filtering is applied.
fn build_tool_set(
    registries: &RegistrySet,
    spec: &AgentSpec,
    env: &ExecutionEnv,
) -> Result<HashMap<String, Arc<dyn Tool>>, ResolveError> {
    let mut tools = collect_global_tools(registries);

    // Merge delegate agent tools
    resolve_delegate_tools(registries, spec, &mut tools)?;

    // Merge plugin-registered tools (conflict with global = error)
    for (tool_id, tool) in &env.tools {
        if tools.contains_key(tool_id) {
            return Err(ResolveError::ToolIdConflict {
                tool_id: tool_id.clone(),
                source_a: "global".into(),
                source_b: "plugin".into(),
            });
        }
        tools.insert(tool_id.clone(), Arc::clone(tool));
    }

    // Capture the registered tool ids BEFORE filtering, so unmatched-pattern
    // diagnostics aren't confused by tools the catalog itself just removed.
    let pre_filter_ids: Vec<String> = tools.keys().cloned().collect();
    filter_tools(&mut tools, spec);
    let pre_refs: Vec<&str> = pre_filter_ids.iter().map(String::as_str).collect();
    for (field, pattern) in catalog::unmatched_patterns(spec, &pre_refs) {
        tracing::warn!(
            agent_id = %spec.id,
            catalog_field = field,
            catalog_pattern = %pattern,
            "catalog pattern matches no registered tool"
        );
    }
    for (field, entry) in catalog::argument_pattern_misuse(spec) {
        tracing::warn!(
            agent_id = %spec.id,
            catalog_field = field,
            catalog_entry = %entry,
            "catalog entry looks like a permission argument pattern; \
             move to sections[\"permission\"] instead"
        );
    }
    let surviving: Vec<&str> = tools.keys().map(String::as_str).collect();
    for name in catalog::permission_rules_without_catalog_match(spec, &surviving) {
        tracing::warn!(
            agent_id = %spec.id,
            permission_tool = %name,
            "permission rule references a tool filtered out by the agent's catalog"
        );
    }

    Ok(tools)
}

/// Create delegate agent tools from `spec.delegates`.
#[cfg_attr(not(feature = "a2a"), allow(unused_variables))]
fn resolve_delegate_tools(
    registries: &RegistrySet,
    spec: &AgentSpec,
    tools: &mut HashMap<String, Arc<dyn Tool>>,
) -> Result<(), ResolveError> {
    #[cfg(feature = "a2a")]
    if !spec.delegates.is_empty() {
        let resolver: Arc<dyn crate::registry::AgentResolver> =
            Arc::new(RegistrySetResolver::new(registries.clone()));
        for delegate_id in &spec.delegates {
            let delegate_spec = registries
                .agents
                .get_agent(delegate_id)
                .ok_or_else(|| ResolveError::AgentNotFound(delegate_id.clone()))?;

            let description: String = delegate_spec.system_prompt.chars().take(100).collect();
            if let Some(endpoint) = &delegate_spec.endpoint {
                let factory = registries
                    .backends
                    .get_backend_factory(&endpoint.backend)
                    .ok_or_else(|| ResolveError::UnsupportedRemoteBackend {
                        agent_id: delegate_id.clone(),
                        backend: endpoint.backend.clone(),
                    })?;
                factory.validate(endpoint).map_err(|error| {
                    ResolveError::InvalidRemoteEndpointConfig {
                        agent_id: delegate_id.clone(),
                        backend: endpoint.backend.clone(),
                        message: error.to_string(),
                    }
                })?;
            }

            let tool: Arc<dyn Tool> =
                Arc::new(crate::extensions::a2a::AgentTool::with_execution_resolver(
                    delegate_id,
                    &description,
                    resolver.clone(),
                ));
            let tool_id = tool.descriptor().id;
            tools.insert(tool_id, tool);
        }
    }
    #[cfg(not(feature = "a2a"))]
    if !spec.delegates.is_empty() {
        tracing::warn!(
            agent_id = %spec.id,
            "agent has delegates but 'a2a' feature is disabled; delegates ignored"
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// AgentResolver implementation
// ---------------------------------------------------------------------------

/// Resolver that bridges a fixed `RegistrySet` into `AgentResolver`.
///
/// Separates the registry aggregation concern (`RegistrySet`) from the
/// resolution logic. `RegistrySet` stays a pure data container.
pub struct RegistrySetResolver {
    registries: RegistrySet,
    snapshot_version: Option<u64>,
}

impl RegistrySetResolver {
    #[must_use]
    pub fn new(registries: RegistrySet) -> Self {
        Self {
            registries,
            snapshot_version: None,
        }
    }

    #[must_use]
    pub(crate) fn with_snapshot_version(registries: RegistrySet, snapshot_version: u64) -> Self {
        Self {
            registries,
            snapshot_version: Some(snapshot_version),
        }
    }
}

impl AgentResolver for RegistrySetResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        resolve_registry_set(&self.registries, agent_id).map_err(|e| RuntimeError::ResolveFailed {
            message: e.to_string(),
        })
    }

    fn resolve_execution(&self, agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        resolve_execution_registry_set(&self.registries, agent_id).map_err(|error| {
            RuntimeError::ResolveFailed {
                message: error.to_string(),
            }
        })
    }

    fn agent_ids(&self) -> Vec<String> {
        self.registries.agents.agent_ids()
    }
}

#[async_trait]
impl Resolver for RegistrySetResolver {
    async fn resolve(
        &self,
        request: ResolutionRequest,
    ) -> Result<ResolvedRunPlan, RunResolveError> {
        let (agent_id, role) = target_agent_and_role(&request.target);
        let agent_id = agent_id.to_string();
        let execution = resolve_execution_registry_set(&self.registries, &agent_id)
            .map_err(|error| RunResolveError::Runtime(error.to_string()))?;
        let requirements = BackendRequirements::from_features(&request.features);
        let profile = backend_profile_for_execution(&execution)?;
        let resolution_scope =
            self.resolution_scope_for_request(&agent_id, request.resolution_scope, &requirements)?;
        let plan = resolved_run(
            execution,
            role,
            request.overrides,
            requirements,
            profile,
            resolution_scope,
        )?;
        Ok(plan)
    }
}

impl RegistrySetResolver {
    fn resolution_scope_for_request(
        &self,
        agent_id: &str,
        scope: RunResolutionScope,
        requirements: &BackendRequirements,
    ) -> Result<RunResolutionScope, RunResolveError> {
        match scope {
            RunResolutionScope::Live if requirements.persistence.is_some() => Ok(
                RunResolutionScope::Pinned(self.in_memory_manifest(agent_id)?),
            ),
            RunResolutionScope::Pinned(manifest) => {
                self.validate_manifest_snapshot(&manifest)?;
                Ok(RunResolutionScope::Pinned(manifest))
            }
            RunResolutionScope::Live => Ok(RunResolutionScope::Live),
        }
    }

    fn validate_manifest_snapshot(
        &self,
        manifest: &PinnedRegistryManifest,
    ) -> Result<(), RunResolveError> {
        let (Some(expected), Some(actual)) =
            (manifest.registry_snapshot_version, self.snapshot_version)
        else {
            return Ok(());
        };
        if expected != actual {
            return Err(RunResolveError::UnsupportedPersistence(format!(
                "pinned registry snapshot version {expected} is not active in this runtime \
                 (current snapshot version {actual}); configure a versioned registry store \
                 to replay across registry updates"
            )));
        }
        Ok(())
    }

    fn in_memory_manifest(
        &self,
        root_agent_id: &str,
    ) -> Result<PinnedRegistryManifest, RunResolveError> {
        let mut entries = BTreeMap::<(String, String), PinnedRegistryEntry>::new();
        let mut visiting = HashSet::<String>::new();
        self.collect_agent_manifest_entries(root_agent_id, &mut entries, &mut visiting)?;
        Ok(PinnedRegistryManifest {
            publication_id: None,
            registry_snapshot_version: self.snapshot_version,
            entries: entries.into_values().collect(),
        })
    }

    fn collect_agent_manifest_entries(
        &self,
        agent_id: &str,
        entries: &mut BTreeMap<(String, String), PinnedRegistryEntry>,
        visiting: &mut HashSet<String>,
    ) -> Result<(), RunResolveError> {
        if !visiting.insert(agent_id.to_string()) {
            return Ok(());
        }
        let agent = self.registries.agents.get_agent(agent_id).ok_or_else(|| {
            RunResolveError::Runtime(format!(
                "agent not found while pinning registry: {agent_id}"
            ))
        })?;
        insert_manifest_entry(entries, REGISTRY_KIND_AGENT, &agent.id, &agent);

        if let Some(model) = self.registries.models.get_model(&agent.model_id) {
            collect_model_manifest_entries(entries, &agent.model_id, model);
        } else if let Some(pool) = self.registries.models.get_pool(&agent.model_id) {
            insert_manifest_entry(entries, REGISTRY_KIND_MODEL_POOL, &pool.id, &pool);
            for member in &pool.members {
                if let Some(model) = self.registries.models.get_model(&member.model_id) {
                    collect_model_manifest_entries(entries, &member.model_id, model);
                }
            }
        }

        for delegate_id in &agent.delegates {
            self.collect_agent_manifest_entries(delegate_id, entries, visiting)?;
        }
        visiting.remove(agent_id);
        Ok(())
    }
}

fn target_agent_and_role(target: &ResolutionTarget) -> (&str, ExecutionRole) {
    match target {
        ResolutionTarget::Root { agent_id, .. } => (agent_id.as_str(), ExecutionRole::Root),
        ResolutionTarget::Delegate { agent_id, .. } => (agent_id.as_str(), ExecutionRole::Delegate),
        ResolutionTarget::Handoff { agent_id, .. } => (agent_id.as_str(), ExecutionRole::Handoff),
    }
}

fn backend_profile_for_execution(
    execution: &ExecutionPlan,
) -> Result<BackendProfile, RunResolveError> {
    match execution {
        ExecutionPlan::Local(_) => Ok(BackendProfile::full_local()),
        ExecutionPlan::Remote(agent) => Ok(agent.backend()?.capabilities()),
    }
}

fn resolved_run(
    execution: ExecutionPlan,
    role: ExecutionRole,
    overrides: Option<awaken_contract::contract::inference::InferenceOverride>,
    requirements: BackendRequirements,
    backend_profile: BackendProfile,
    resolution_scope: RunResolutionScope,
) -> Result<ResolvedRunPlan, RunResolveError> {
    let agent_spec = execution.spec().clone();
    let upstream_model = match &execution {
        ExecutionPlan::Local(agent) => agent.upstream_model.clone(),
        ExecutionPlan::Remote(agent) => agent.spec.model_id.clone(),
    };
    let tools = match &execution {
        ExecutionPlan::Local(agent) => agent
            .tool_descriptors()
            .into_iter()
            .map(|descriptor| ResolvedTool { descriptor })
            .collect(),
        ExecutionPlan::Remote(_) => Vec::new(),
    };
    match resolution_scope {
        RunResolutionScope::Pinned(manifest) => Ok(ResolvedRunPlan::Replayable(ResolvedRun {
            agent_spec,
            role,
            execution,
            model: ResolvedModelBinding { upstream_model },
            tools,
            overrides,
            backend_profile,
            requirements,
            scope: ReplayableScope { manifest },
        })),
        RunResolutionScope::Live => {
            if requirements.persistence.is_some() {
                return Err(RunResolveError::UnsupportedPersistence(
                    "live RegistrySetResolver cannot materialize a replayable registry scope"
                        .into(),
                ));
            }
            Ok(ResolvedRunPlan::LiveOnly(ResolvedRun {
                agent_spec,
                role,
                execution,
                model: ResolvedModelBinding { upstream_model },
                tools,
                overrides,
                backend_profile,
                requirements,
                scope: LiveOnlyScope,
            }))
        }
    }
}

/// Resolver backed by a versioned registry handle.
///
/// Each call resolves against the current published registry snapshot,
/// allowing callers to swap registry contents without replacing the runtime.
pub(crate) struct DynamicRegistryResolver {
    handle: RegistryHandle,
}

impl DynamicRegistryResolver {
    pub(crate) fn new(handle: RegistryHandle) -> Self {
        Self { handle }
    }
}

impl AgentResolver for DynamicRegistryResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let snapshot = self.handle.snapshot();
        resolve_registry_set(snapshot.registries(), agent_id).map_err(|e| {
            RuntimeError::ResolveFailed {
                message: e.to_string(),
            }
        })
    }

    fn resolve_execution(&self, agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        let snapshot = self.handle.snapshot();
        resolve_execution_registry_set(snapshot.registries(), agent_id).map_err(|error| {
            RuntimeError::ResolveFailed {
                message: error.to_string(),
            }
        })
    }

    fn agent_ids(&self) -> Vec<String> {
        self.handle.snapshot().registries().agents.agent_ids()
    }
}

#[async_trait]
impl Resolver for DynamicRegistryResolver {
    async fn resolve(
        &self,
        request: ResolutionRequest,
    ) -> Result<ResolvedRunPlan, RunResolveError> {
        let snapshot = self.handle.snapshot();
        let version = snapshot.version();
        let resolver =
            RegistrySetResolver::with_snapshot_version(snapshot.into_registries(), version);
        Resolver::resolve(&resolver, request).await
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Validate spec sections against plugin-declared JSON Schemas.
///
/// For each plugin that declares `config_schemas()`, validates the
/// corresponding section in `AgentSpec.sections` against its JSON Schema.
/// Missing sections are fine (plugins fall back to defaults). Invalid
/// sections produce `ResolveError::InvalidPluginConfig`.
///
/// Also logs a warning for any section keys not claimed by any plugin.
fn validate_sections(spec: &AgentSpec, plugins: &[Arc<dyn Plugin>]) -> Result<(), ResolveError> {
    let mut claimed_keys: HashSet<&str> = HashSet::new();

    for plugin in plugins {
        let schemas = plugin.config_schemas();
        for schema in &schemas {
            claimed_keys.insert(schema.key);
            if let Some(value) = spec.sections.get(schema.key) {
                jsonschema::validate(&schema.json_schema, value).map_err(|e| {
                    ResolveError::InvalidPluginConfig {
                        plugin: plugin.descriptor().name.into(),
                        key: schema.key.into(),
                        message: e.to_string(),
                    }
                })?;
            }
        }
    }

    // Warn about unclaimed section keys
    for key in spec.sections.keys() {
        if !claimed_keys.contains(key.as_str()) {
            tracing::warn!(
                agent_id = %spec.id,
                key = %key,
                "section key not claimed by any plugin — possible typo"
            );
        }
    }

    Ok(())
}

/// Collect all global (builder-registered) tools from the registry.
fn collect_global_tools(registries: &RegistrySet) -> HashMap<String, Arc<dyn Tool>> {
    let mut tools = HashMap::new();
    for id in registries.tools.tool_ids() {
        if let Some(tool) = registries.tools.get_tool(&id) {
            tools.insert(id, tool);
        }
    }
    tools
}

/// Resolve plugins by IDs from the spec.
fn resolve_plugins(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<Vec<Arc<dyn Plugin>>, ResolveError> {
    spec.plugin_ids
        .iter()
        .map(|id| {
            registries
                .plugins
                .get_plugin(id)
                .ok_or_else(|| ResolveError::PluginNotFound(id.clone()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
