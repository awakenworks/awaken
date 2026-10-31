// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Selection of the tool, Skill, plugin, and gate offering registered on every thread;
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Tool dependencies of a Host: client tools, the Skill offering, runtime
//! plugins and their configuration, WebSearch providers, the gate override,
//! and management tools installed on every thread.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use crate::host::*;

impl SessionExecutorBuilder {
    /// Replace the built-in registry with one process-owned registry. Use
    /// the same clone for capability projection and semantic publication
    /// validation to keep discovery, validation, and dispatch identical.
    #[must_use]
    pub fn with_web_search_provider_registry(
        mut self,
        providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    ) -> Self {
        self.tools.web_search_providers = providers;
        self
    }

    /// Register the management assistant's tool executables globally (ADR-0052). They
    /// run on every thread's runtime, but only the reserved-scope assistant's config
    /// names them, so only its runs can invoke them; their ids are auto-allowed on the
    /// gate (the tools are read-only).
    #[must_use]
    pub fn with_admin_tools(
        mut self,
        tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    ) -> Self {
        self.tools.admin_tools = tools;
        self
    }

    /// Select the already-derived effective compaction window used by embedded
    /// test/development Hosts and wire the `compactor` sub-agent. Production
    /// obtains the same value from Config publication; this seam must not
    /// derive a ratio or message-count fallback. The generic plugin id/config
    /// pair is the single configuration authority: it both activates local
    /// execution and crosses a durable dispatch snapshot.
    #[must_use]
    pub fn with_compaction_tokens(mut self, max_tokens: u32, keep_last: usize) -> Self {
        let config = CompactConfig {
            max_tokens: Some(max_tokens),
            keep_last,
            ..CompactConfig::default()
        };
        if !self
            .tools
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID)
        {
            self.tools
                .plugin_ids
                .push(awaken_ext_compact::COMPACT_PLUGIN_ID.to_string());
        }
        self.tools.plugin_config.insert(
            awaken_ext_compact::COMPACT_PLUGIN_ID.to_string(),
            serde_json::to_value(config).expect("CompactConfig is serializable"),
        );
        self
    }

    /// Replace the default authorization gate on every thread with `gate` (slice
    /// E): the scheduled-action server uses a gate that defers tool calls so the
    /// durable worker performs them (ADR-0020).
    #[must_use]
    pub fn with_gate_override(
        mut self,
        gate: Arc<dyn awaken_runtime_contract::permission::ToolGateHook>,
    ) -> Self {
        self.tools.gate_override = Some(gate);
        self
    }

    /// Add client-executed tools: those ids are model-visible but unregistered, so
    /// a call awaits and the client supplies the result.
    #[must_use]
    pub fn with_client_tools(mut self, client_tools: HashSet<String>) -> Self {
        self.tools.client_tools.extend(client_tools);
        self
    }

    /// Activate the tool state machine on every thread with `config` (its
    /// `{"machines":[…]}` section). The plugin gates and advances tool calls per
    /// the declared transitions (ADR tool-state-machine).
    #[must_use]
    pub fn with_state_machine(mut self, config: serde_json::Value) -> Self {
        self.tools
            .plugin_ids
            .push(awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.to_string());
        self.tools.plugin_config.insert(
            awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.to_string(),
            config,
        );
        self
    }

    /// Offer host-static Skills to direct/non-Managed threads (ADR-0036). The
    /// compatibility adapter lists and activates them through the one registry.
    /// Managed Sessions require durable versioned bindings instead.
    #[must_use]
    pub fn with_skills(mut self, skills: Vec<SkillSpec>) -> Self {
        self.tools.skills.add_specs(skills);
        self
    }

    /// A filesystem Skill store for this crate's own unit tests. Other
    /// compositions select their catalog through
    /// [`Self::with_skill_store_ports`].
    #[cfg(test)]
    #[must_use]
    pub fn with_skill_store(self, dir: impl Into<PathBuf>) -> Self {
        let store = Arc::new(
            awaken_skill_store::FsSkillStore::open(dir.into())
                .expect("open durable skill store root"),
        );
        self.with_skill_store_backend(store)
    }

    /// A Skill store backend for this crate's own unit tests, adapted through
    /// the Resources applications the Host reads Skills with.
    #[cfg(test)]
    #[must_use]
    pub fn with_skill_store_backend(
        self,
        store: Arc<dyn awaken_resource_contract::SkillStore>,
    ) -> Self {
        let ports = crate::LocalSkillPorts {
            bundle_source: Arc::new(awaken_resource_application::StoreSkillBundleSource::new(
                store.clone(),
            )),
            application: Arc::new(
                awaken_resource_application::StoreSkillCatalogApplication::new(
                    store.clone(),
                    self.clock(),
                ),
            ),
            store,
        };
        self.with_skill_store_ports(ports)
    }

    /// Test-support replacement of a volatile fixture's Skill catalog: the
    /// store and the Resources applications over it, supplied by the
    /// composition that owns them.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_skill_store_ports(mut self, ports: crate::LocalSkillPorts) -> Self {
        self.tools.skills.set_store(ports.store, ports.application);
        self.skill_bundle_source = Some(ports.bundle_source);
        self
    }

    /// Select only the exact immutable custom-Skill read port used by an
    /// execution Worker. This does not grant authoring or catalog access.
    #[must_use]
    pub fn with_skill_bundle_source(
        mut self,
        source: Arc<
            dyn awaken_session_configuration_contract::SkillBundleSource<
                    crate::ResourceOperationFence,
                >,
        >,
    ) -> Self {
        self.skill_bundle_source = Some(source);
        self
    }

    /// Select the Resources application capability used to freeze custom Skill
    /// selections and publish authored bundles. This grants no Store/purge API.
    #[must_use]
    pub fn with_skill_catalog_application(
        mut self,
        application: Arc<dyn awaken_session_configuration_contract::SkillCatalogApplication>,
    ) -> Self {
        self.tools.skills.set_application(application);
        self
    }
}
