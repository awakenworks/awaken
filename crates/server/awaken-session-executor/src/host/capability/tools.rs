// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Installers of the tool, Skill, plugin, and gate offering registered on every thread;
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! TOOLS capability of [`SharedHost`]: client tools, the Skill offering,
//! runtime plugins and their configuration, WebSearch providers, the gate
//! override, and management tools installed on every thread.

use crate::host::*;

/// Tool/Skill/plugin offering. Consumer: every thread's runtime assembly and
/// the adapters' capability projections.
pub(crate) struct ToolOffering {
    pub(crate) client_tools: HashSet<String>,
    /// The skill offering (ADR-0036): the static configured set, the optional durable
    /// `/v1/skills` catalog, and its sync-read cache — grouped behind one type that
    /// owns the cache↔store coherence invariant. See [`crate::skill_catalog`].
    pub(crate) skills: crate::skill_catalog::SkillCatalog,
    /// Whether a `context: fork` Skill reuses the parent Session environment.
    /// Ordinary delegated child Runs always share that environment.
    pub(crate) skill_fork_placement: crate::skills::SkillForkPlacement,
    /// Runtime plugins this host activates on every thread, and their config
    /// sections (e.g. the tool state machine). Empty by default.
    pub(crate) plugin_ids: Vec<String>,
    pub(crate) plugin_config: std::collections::BTreeMap<String, serde_json::Value>,
    /// One provider registry is used to derive authoring schema and to dispatch
    /// Native/ACP WebSearch calls. External deployments extend this registry;
    /// sessions never construct a provider-specific side registry.
    pub(crate) web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    /// An optional tool gate that replaces the default authorization gate on every
    /// thread's runtime. Used to exercise scheduled actions (ADR-0020, slice E): a
    /// gate that defers tool calls as `ScheduledAction`s so the durable dispatch
    /// worker performs them out of band.
    pub(crate) gate_override: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    /// Globally-registered management tool executables (ADR-0052 D3/D4). Registered
    /// on every thread's runtime (the executor registry stays global); only the
    /// reserved-scope assistant's compiled config *names* them, so no other run can
    /// invoke them. Their ids are also pre-authorized on the gate (read-only tools).
    /// Empty by default.
    pub(crate) admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
}

impl ToolOffering {
    /// The built-in WebSearch providers and `skills`; no client tools,
    /// plugins, gate override, or management tools yet.
    pub(in crate::host) fn new(skills: crate::skill_catalog::SkillCatalog) -> Self {
        Self {
            client_tools: HashSet::new(),
            skills,
            // Subagents share the parent's sandbox by default (`默认共用`).
            skill_fork_placement: crate::skills::SkillForkPlacement::SharedSession,
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
            gate_override: None,
            admin_tools: Vec::new(),
        }
    }
}

impl SharedHost {
    /// Add one externally owned WebSearch provider to the authoritative
    /// registry. Duplicate ids fail at startup time instead of shadowing a
    /// built-in or changing dispatch order at runtime.
    pub fn with_web_search_provider(
        mut self,
        provider: Arc<dyn awaken_ext_builtin_tools::WebSearchProvider>,
    ) -> Result<Self, awaken_ext_builtin_tools::WebSearchRegistryError> {
        self.tools.web_search_providers.register(provider)?;
        Ok(self)
    }

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

    /// Read-only provider discovery for an embedding capability endpoint. The
    /// returned registry is the same value Session dispatch uses.
    #[must_use]
    pub fn web_search_provider_registry(
        &self,
    ) -> &awaken_ext_builtin_tools::WebSearchProviderRegistry {
        &self.tools.web_search_providers
    }

    /// Register the management assistant's tool executables globally (ADR-0052). They
    /// run on every thread's runtime, but only the reserved-scope assistant's config
    /// names them, so only its runs can invoke them; their ids are auto-allowed on the
    /// gate (the tools are read-only).
    #[must_use]
    pub fn with_admin_tools(
        mut self,
        tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Self {
        self.tools.admin_tools = tools;
        self
    }

    /// Install the already-derived effective compaction window used by embedded
    /// test/development Hosts. Production obtains the same value from Config
    /// publication; this seam must not derive a ratio or message-count fallback.
    pub fn with_compaction_tokens(self, max_tokens: u32, keep_last: usize) -> Self {
        self.enable_compaction(CompactConfig {
            max_tokens: Some(max_tokens),
            keep_last,
            ..CompactConfig::default()
        })
    }

    /// Install a resolved `CompactConfig` and wire the `compactor` sub-agent.
    /// The generic plugin id/config pair is the single configuration authority:
    /// it both activates local execution and crosses a durable dispatch snapshot.
    fn enable_compaction(mut self, config: CompactConfig) -> Self {
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
    pub fn with_gate_override(
        mut self,
        gate: Arc<dyn awaken_runtime_contract::permission::ToolGateHook>,
    ) -> Self {
        self.tools.gate_override = Some(gate);
        self
    }

    /// Add client-executed tools: those ids are model-visible but unregistered, so
    /// a call awaits and the client supplies the result.
    pub fn with_client_tools(mut self, client_tools: HashSet<String>) -> Self {
        self.tools.client_tools.extend(client_tools);
        self
    }

    /// Whether `context: fork` Skills reuse the Session environment. Ordinary
    /// delegated child Runs are unaffected and always share it.
    pub fn with_skill_fork_placement(
        mut self,
        placement: crate::skills::SkillForkPlacement,
    ) -> Self {
        self.tools.skill_fork_placement = placement;
        self
    }

    /// Activate the tool state machine on every thread with `config` (its
    /// `{"machines":[…]}` section). The plugin gates and advances tool calls per
    /// the declared transitions (ADR tool-state-machine).
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
    pub fn with_skills(mut self, skills: Vec<SkillSpec>) -> Self {
        self.tools.skills.add_specs(skills);
        self
    }

    /// A filesystem Skill store for this crate's own unit tests. Other
    /// compositions install their catalog through
    /// [`Self::with_skill_store_ports`].
    #[cfg(test)]
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
    pub fn with_skill_store_ports(mut self, ports: crate::LocalSkillPorts) -> Self {
        self.tools
            .skills
            .set_store(ports.store, ports.bundle_source, ports.application);
        self
    }

    /// Install only the exact immutable custom-Skill read port used by an
    /// execution Worker. This does not grant authoring or catalog access.
    pub fn with_skill_bundle_source(
        mut self,
        source: Arc<
            dyn awaken_session_configuration_contract::SkillBundleSource<
                    awaken_dispatch::ResourceOperationFence,
                >,
        >,
    ) -> Self {
        self.tools.skills.set_bundle_source(source);
        self
    }

    /// Install the Resources application capability used to freeze custom Skill
    /// selections and publish authored bundles. This grants no Store/purge API.
    pub fn with_skill_catalog_application(
        mut self,
        application: Arc<dyn awaken_session_configuration_contract::SkillCatalogApplication>,
    ) -> Self {
        self.tools.skills.set_application(application);
        self
    }

    /// The set of client-executed tool ids (model-visible, host-unregistered).
    pub fn client_tools(&self) -> &HashSet<String> {
        &self.tools.client_tools
    }
}
