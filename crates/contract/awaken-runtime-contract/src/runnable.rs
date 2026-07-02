//! `RunnableConfig`: a runtime-ready agent configuration.
//!
//! It is what [`Runtime::run`](../../awaken_runtime/struct.Runtime.html) consumes:
//! built directly by hand with [`RunnableConfig::builder`], or produced by an
//! external compiler (`awaken-config-store::compile`). Either way it bundles the
//! executable snapshot and the catalog install it was built against under **one
//! fingerprint** — the producer stamps it once, the consumer never juggles
//! snapshot/install/fingerprint by hand.
//!
//! Direct construction needs no config store and no hashing: the builder stamps
//! the agent id as the consistency token (enough for in-process use). A compiler
//! overrides it with a content hash via [`RunnableConfigBuilder::fingerprint`].

use std::collections::BTreeMap;

use crate::capability::{PluginCapability, RuntimeCapabilityCatalog};
use crate::catalog::RuntimeCatalogInstall;
use crate::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use crate::snapshot::{AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId};

/// A loop-step ceiling used when the builder is not told otherwise.
const DEFAULT_MAX_STEPS: usize = 16;

/// A runtime-ready agent configuration: the executable snapshot plus the catalog
/// install it was built against, sharing one fingerprint. Build it with
/// [`RunnableConfig::builder`], or get one from an external compiler. The snapshot
/// and install are kept internal so they cannot drift apart — the only way to make
/// one is through a path that stamps a consistent fingerprint.
#[derive(Debug, Clone)]
pub struct RunnableConfig {
    snapshot: ExecutableAgentSnapshot,
    install: RuntimeCatalogInstall,
}

impl RunnableConfig {
    /// Start building a config for the agent identified by `id`.
    pub fn builder(id: impl Into<String>) -> RunnableConfigBuilder {
        RunnableConfigBuilder::new(id)
    }

    /// The executable snapshot the runtime resolves and runs.
    pub fn snapshot(&self) -> &ExecutableAgentSnapshot {
        &self.snapshot
    }

    /// The catalog install the runtime registers before running.
    pub fn install(&self) -> &RuntimeCatalogInstall {
        &self.install
    }

    /// Consume the config into its parts, for a consumer that persists them (a
    /// config store). The two carry the same fingerprint by construction.
    pub fn into_parts(self) -> (ExecutableAgentSnapshot, RuntimeCatalogInstall) {
        (self.snapshot, self.install)
    }
}

/// Fluent builder for [`RunnableConfig`]. `build` stamps one fingerprint into the
/// snapshot and the install, so the two always agree — the property the runtime
/// re-checks on resolution (fail-closed). This is the single assembly path: a
/// compiler feeds resolved tool descriptors plus a content-hash `fingerprint`; a
/// direct caller feeds descriptors and lets the id stand in as the token.
#[derive(Debug, Clone)]
pub struct RunnableConfigBuilder {
    id: String,
    instructions: String,
    max_steps: usize,
    model_binding: ModelBinding,
    tools: Vec<ToolDescriptor>,
    plugin_ids: Vec<String>,
    plugin_config: BTreeMap<String, serde_json::Value>,
    plugin_capabilities: Vec<PluginCapability>,
    context_policy: ContextPolicy,
    fingerprint: Option<String>,
}

impl RunnableConfigBuilder {
    fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            instructions: String::new(),
            max_steps: DEFAULT_MAX_STEPS,
            model_binding: ModelBinding::default(),
            tools: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: BTreeMap::new(),
            plugin_capabilities: Vec::new(),
            context_policy: ContextPolicy::default(),
            fingerprint: None,
        }
    }

    /// The behavior text injected as the leading system message.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// The provider instance / model / backend this agent runs on.
    #[must_use]
    pub fn model(mut self, model_binding: ModelBinding) -> Self {
        self.model_binding = model_binding;
        self
    }

    /// The ceiling on model/tool loop steps for one run.
    #[must_use]
    pub fn max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Add one tool the agent may call.
    #[must_use]
    pub fn tool(mut self, tool: ToolDescriptor) -> Self {
        self.tools.push(tool);
        self
    }

    /// Add several tools at once.
    #[must_use]
    pub fn tools(mut self, tools: impl IntoIterator<Item = ToolDescriptor>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Select the plugins active for this run by id. A plugin installed on the
    /// runtime only contributes when its id is listed here (G30).
    #[must_use]
    pub fn plugins(mut self, plugin_ids: impl IntoIterator<Item = String>) -> Self {
        self.plugin_ids.extend(plugin_ids);
        self
    }

    /// Per-plugin configuration sections, keyed by plugin id. Each active plugin
    /// reads its own section at resolve; a plugin whose id is absent uses its
    /// defaults.
    #[must_use]
    pub fn plugin_config(
        mut self,
        sections: impl IntoIterator<Item = (String, serde_json::Value)>,
    ) -> Self {
        self.plugin_config.extend(sections);
        self
    }

    /// The plugin capabilities advertised in the catalog (id + config schema), so
    /// a config frontend can discover and author each plugin's section.
    #[must_use]
    pub fn plugin_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = PluginCapability>,
    ) -> Self {
        self.plugin_capabilities.extend(capabilities);
        self
    }

    /// Bound the model-visible context window (default [`ContextPolicy::KeepAll`]).
    #[must_use]
    pub fn context_policy(mut self, policy: ContextPolicy) -> Self {
        self.context_policy = policy;
        self
    }

    /// Set the fingerprint explicitly — a content hash from a compiler. When unset,
    /// the agent id is used as the consistency token, which is enough for direct,
    /// in-process use where content-addressing is not needed.
    #[must_use]
    pub fn fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.fingerprint = Some(fingerprint.into());
        self
    }

    /// Assemble the [`RunnableConfig`], stamping the fingerprint into the snapshot
    /// and the install so they agree.
    pub fn build(self) -> RunnableConfig {
        let fingerprint = self.fingerprint.unwrap_or_else(|| self.id.clone());
        let fp = CatalogFingerprint(fingerprint.clone());
        let snapshot = ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId(self.id.clone()),
            root_agent_id: AgentId(self.id.clone()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fp.clone(),
                instructions: self.instructions,
                max_steps: self.max_steps,
                model_binding: self.model_binding,
                tool_descriptors: self.tools,
                plugin_ids: self.plugin_ids,
                plugin_config: self.plugin_config,
                context_policy: self.context_policy,
            },
            fingerprint: fp.clone(),
        };
        let install = RuntimeCatalogInstall {
            publication_id: fingerprint,
            fingerprint: fp.clone(),
            source_revisions: vec![self.id],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fp,
                runtime_version: env!("CARGO_PKG_VERSION").to_string(),
                tools: Vec::new(),
                plugins: self.plugin_capabilities,
            },
        };
        RunnableConfig { snapshot, install }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_stamps_one_consistent_fingerprint() {
        let config = RunnableConfig::builder("assistant")
            .instructions("be concise")
            .model(ModelBinding::new("demo", "stub", "stub"))
            .max_steps(8)
            .build();

        // Default token is the id, stamped into every fingerprint slot.
        let snap = config.snapshot();
        let install = config.install();
        assert_eq!(snap.fingerprint.0, "assistant");
        assert_eq!(snap.resolved_spec.catalog_fingerprint.0, "assistant");
        assert_eq!(install.fingerprint.0, "assistant");
        assert_eq!(install.capabilities.catalog_fingerprint.0, "assistant");
        assert_eq!(snap.resolved_spec.instructions, "be concise");
        assert_eq!(snap.resolved_spec.max_steps, 8);
    }

    #[test]
    fn explicit_fingerprint_overrides_the_id_token() {
        let config = RunnableConfig::builder("assistant")
            .fingerprint("sha256:abc")
            .build();
        assert_eq!(config.snapshot().fingerprint.0, "sha256:abc");
        assert_eq!(config.install().fingerprint.0, "sha256:abc");
        // The snapshot id stays the agent id, not the fingerprint.
        assert_eq!(config.snapshot().id.0, "assistant");
    }
}
