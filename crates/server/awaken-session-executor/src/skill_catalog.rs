// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
//! `SkillCatalog` — the host's skill offering plus its durable-catalog cache coherence.
//!
//! Groups the three skill fields that were flat on [`crate::ExecutorComponents`] (the static
//! configured `specs`, the optional durable `/v1/skills` `store`, and the sync-read
//! `cache` snapshot) behind one type that owns their single invariant: the in-memory
//! `cache` stays coherent with the async `store`, refreshed on every write and at each
//! session setup. Every skill read/write is a closed algebra over just these three
//! fields — no other host state — so the boundary is a real domain seam, not a bucket.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::{AgentSkillBinding, AgentSkillKind};
use awaken_ext_skills::SkillSpec;
#[cfg(test)]
use awaken_resource_contract::SkillDefinition;
use awaken_resource_contract::{SkillStoreError, SkillVersion};
use awaken_session_configuration_contract::resource::ResolvedSkillBinding;

use crate::skill_delivery::anthropic_skill;
use awaken_session_configuration_contract::SkillCatalogApplication;

/// Direct compatibility Skills plus the durable versioned catalog and its
/// synchronous read cache. See the module docs for the coherence invariant.
pub(crate) struct SkillCatalog {
    /// Host-static Skills offered to direct/non-Managed threads (ADR-0036). They
    /// are a compatibility source, not a versioned Managed Skill authority.
    specs: Vec<SkillSpec>,
    /// An optional durable delivered-skill catalog (resources plane). When set, its
    /// immutable versions are offered alongside the static `specs` and survive a restart, so
    /// a catalog configured through `/v1/skills` outlives the process. The host reads
    /// the bytes and feeds them to the extension's `SkillSource`, so the runtime stays
    /// store-unaware.
    application: Option<Arc<dyn SkillCatalogApplication>>,
    #[cfg(any(test, feature = "test-support"))]
    test_store: Option<Arc<dyn awaken_resource_contract::SkillStore>>,
    /// In-memory snapshot of the latest delivered versions, read
    /// *synchronously* by the capability advertisement (`ids`) and the run-loop
    /// `SkillSource` scan — refreshed from the async `store` on a write and at each
    /// session's setup (`reload_cache`). This is how a network-DB (async) catalog
    /// serves the host's sync read paths.
    cache: Mutex<std::collections::BTreeMap<String, Vec<SkillVersion>>>,
}

impl SkillCatalog {
    /// An empty catalog: no static skills, no durable store. The builder wires the
    /// configured set and (optionally) a store.
    pub(crate) fn new() -> Self {
        Self {
            specs: Vec::new(),
            application: None,
            #[cfg(any(test, feature = "test-support"))]
            test_store: None,
            cache: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Builder: append the configured static skills.
    pub(crate) fn add_specs(&mut self, skills: Vec<SkillSpec>) {
        self.specs.extend(skills);
    }

    /// Builder: wire the durable delivered-skill catalog.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set_store(
        &mut self,
        store: Arc<dyn awaken_resource_contract::SkillStore>,
        application: Arc<dyn SkillCatalogApplication>,
    ) {
        self.application = Some(application);
        self.test_store = Some(store);
    }

    pub(crate) fn set_application(&mut self, application: Arc<dyn SkillCatalogApplication>) {
        self.application = Some(application);
    }

    /// The configured static Skills offered to direct/non-Managed threads.
    pub(crate) fn specs(&self) -> &[SkillSpec] {
        &self.specs
    }

    /// Whether this host has a durable skill catalog wired.
    pub(crate) fn has_application(&self) -> bool {
        self.application.is_some()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn store_handle(&self) -> Option<Arc<dyn awaken_resource_contract::SkillStore>> {
        self.test_store.clone()
    }

    #[cfg(test)]
    pub(crate) async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Option<Result<(), SkillStoreError>> {
        let store = self.test_store.as_ref()?;
        let workspace = definition.workspace_id.clone();
        let result = store.create(definition, initial_version).await;
        if result.is_ok()
            && let Err(error) = self.reload_cache_in(workspace.as_str()).await
        {
            return Some(Err(error));
        }
        Some(result)
    }

    #[cfg(test)]
    pub(crate) async fn append_version(
        &self,
        workspace: &str,
        id: &str,
        version: SkillVersion,
    ) -> Option<Result<(), SkillStoreError>> {
        let store = self.test_store.as_ref()?;
        let result = store.append_version(workspace, id, version).await;
        if result.is_ok()
            && let Err(error) = self.reload_cache_in(workspace).await
        {
            return Some(Err(error));
        }
        Some(result)
    }

    /// Test-only explicit control-plane publication helper. Runtime-authored
    /// Skills remain run-scoped and never call this path during cleanup.
    #[cfg(test)]
    pub(crate) async fn persist_authored(
        &self,
        workspace: &str,
        raw_id: &str,
        content: &str,
    ) -> Option<Result<(), SkillStoreError>> {
        let application = self.application.as_ref()?;
        let parsed = awaken_ext_skills::parse_skill_md(raw_id, content);
        let result = application
            .publish_authored(
                workspace,
                raw_id,
                &parsed.name,
                &parsed.description,
                content,
            )
            .await;
        if result.is_ok()
            && let Err(error) = self.reload_cache_in(workspace).await
        {
            return Some(Err(error));
        }
        Some(result)
    }

    #[cfg(test)]
    pub(crate) async fn definitions(
        &self,
        workspace: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        match self.test_store.as_ref() {
            Some(store) => store.list_definitions(workspace).await,
            None => Ok(Vec::new()),
        }
    }

    pub(crate) async fn resolve(
        &self,
        workspace: &str,
        selections: &[AgentSkillBinding],
    ) -> Result<Vec<ResolvedSkillBinding>, SkillStoreError> {
        awaken_agent_contract::validate_agent_skills(selections)
            .map_err(|error| SkillStoreError::InvalidSelection { detail: error })?;
        let mut bindings = Vec::with_capacity(selections.len());
        for selection in selections {
            let version = match selection.kind {
                AgentSkillKind::Anthropic => anthropic_skill(&selection.skill_id)
                    .ok_or_else(|| SkillStoreError::NotFound(selection.skill_id.clone()))?,
                AgentSkillKind::Custom => {
                    let binding = self
                        .application
                        .as_ref()
                        .ok_or_else(|| SkillStoreError::AuthorityUnavailable {
                            detail: "no Skill catalog application".into(),
                        })?
                        .resolve_custom(workspace, &selection.skill_id, &selection.version)
                        .await?;
                    bindings.push(binding);
                    continue;
                }
            };
            bindings.push(ResolvedSkillBinding {
                kind: selection.kind,
                skill_id: selection.skill_id.clone(),
                version: version.version,
                bundle_sha256: version.bundle_sha256,
            });
        }
        Ok(bindings)
    }

    /// Refresh the in-memory delivered-catalog snapshot from the async store. Called
    /// on a write and at each session's setup so the sync read paths (advertisement,
    /// run-loop scan) see the current catalog.
    pub(crate) async fn reload_cache_in(&self, workspace: &str) -> Result<(), SkillStoreError> {
        let Some(application) = self.application.as_ref() else {
            return Ok(());
        };
        let loaded = application.snapshot_latest(workspace).await;
        match loaded {
            Ok(snapshot) => {
                self.cache
                    .lock()
                    .expect("skill cache poisoned")
                    .insert(workspace.to_string(), snapshot);
                Ok(())
            }
            Err(error) => {
                self.cache
                    .lock()
                    .expect("skill cache poisoned")
                    .remove(workspace);
                Err(error)
            }
        }
    }

    /// A clone of the cached latest versions — the synchronous read
    /// the host's `SkillSource` bridge scans (the run-loop scan cannot await).
    pub(crate) fn cache_snapshot_in(&self, workspace: &str) -> Vec<SkillVersion> {
        self.cache
            .lock()
            .expect("skill cache poisoned")
            .get(workspace)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the exact Skill surface visible to a Session needs a physical
    /// Environment. A published Agent supplies `selected`; legacy/direct
    /// Sessions expose the whole configured and cached catalog.
    pub(crate) fn requires_environment_in(
        &self,
        workspace: &str,
        selected: Option<&std::collections::BTreeSet<String>>,
    ) -> bool {
        let is_selected = |id: &str| selected.is_none_or(|ids| ids.contains(id));
        self.specs.iter().any(|skill| {
            is_selected(&skill.id)
                && (skill.environment != awaken_ext_skills::SkillEnvironment::InstructionOnly
                    || skill.context != awaken_ext_skills::SkillContext::Inline)
        }) || self.cache_snapshot_in(workspace).iter().any(|version| {
            is_selected(version.skill_id.as_str())
                && crate::skills::version_requires_environment(version)
        })
    }

    /// Whether the exact Session selection contains any Skill. Filesystem
    /// delivery uses this independently from a Skill's execution substrate:
    /// even an instruction-only Skill needs an Environment when its public
    /// contract is an on-demand `SKILL.md` path.
    pub(crate) fn has_selected_in(
        &self,
        workspace: &str,
        selected: Option<&std::collections::BTreeSet<String>>,
    ) -> bool {
        self.direct_ids_in(workspace)
            .iter()
            .any(|id| selected.is_none_or(|ids| ids.contains(id)))
    }

    /// Direct compatibility ids combine host-static specs and the durable
    /// catalog. This is intentionally private so Managed advertisement cannot
    /// accidentally regain the unversioned source.
    fn direct_ids_in(&self, workspace: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.iter().map(|s| s.id.clone()).collect();
        // The durable catalog is read from the sync cache (refreshed on write and at
        // session setup); a network-DB store cannot be awaited from this sync path.
        // A durable skill is advertised by its tagged catalog id (not its name) so the
        // official worker can download it — `/v1/skills` resolves the same id.
        for id in self.managed_ids_in(workspace) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// Skill ids safe to advertise through the Managed API: every id is backed
    /// by an immutable catalog version that `resolve` can pin and `load_pinned`
    /// can reproduce. Host-static specs are deliberately excluded.
    pub(crate) fn managed_ids_in(&self, workspace: &str) -> Vec<String> {
        self.cache_snapshot_in(workspace)
            .into_iter()
            .map(|version| version.skill_id.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}
