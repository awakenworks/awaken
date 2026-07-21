//! `SkillCatalog` — the host's skill offering plus its durable-catalog cache coherence.
//!
//! Groups the three skill fields that were flat on [`crate::SharedHost`] (the static
//! configured `specs`, the optional durable `/v1/skills` `store`, and the sync-read
//! `cache` snapshot) behind one type that owns their single invariant: the in-memory
//! `cache` stays coherent with the async `store`, refreshed on every write and at each
//! session setup. Every skill read/write is a closed algebra over just these three
//! fields — no other host state — so the boundary is a real domain seam, not a bucket.

use std::sync::{Arc, Mutex};

use awaken_ext_skills::SkillSpec;
use awaken_protocol_managed::ResolvedSkillBinding;
use awaken_skill_store::{SkillDefinition, SkillStore, SkillStoreError, SkillVersion};

/// Skills offered on every thread, plus the durable delivered-catalog and its
/// synchronous read cache. See the module docs for the coherence invariant.
pub(crate) struct SkillCatalog {
    local_workspace: String,
    /// Skills offered on every thread (ADR-0036). The whole set is fronted by the
    /// single `Skill` tool; the model activates one by id to load its instructions.
    specs: Vec<SkillSpec>,
    /// An optional durable delivered-skill catalog (resources plane). When set, its
    /// immutable versions are offered alongside the static `specs` and survive a restart, so
    /// a catalog configured through `/v1/skills` outlives the process. The host reads
    /// the bytes and feeds them to the extension's `SkillSource`, so the runtime stays
    /// store-unaware.
    store: Option<Arc<dyn SkillStore>>,
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
    pub(crate) fn new(local_workspace: String) -> Self {
        Self {
            local_workspace,
            specs: Vec::new(),
            store: None,
            cache: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    pub(crate) fn set_local_workspace(&mut self, workspace: String) {
        self.local_workspace = workspace;
    }

    /// Builder: append the configured static skills.
    pub(crate) fn add_specs(&mut self, skills: Vec<SkillSpec>) {
        self.specs.extend(skills);
    }

    /// Builder: wire the durable delivered-skill catalog.
    pub(crate) fn set_store(&mut self, store: Arc<dyn SkillStore>) {
        self.store = Some(store);
    }

    /// The configured static skills offered on every thread.
    pub(crate) fn specs(&self) -> &[SkillSpec] {
        &self.specs
    }

    /// Whether this host has a durable skill catalog wired.
    pub(crate) fn has_store(&self) -> bool {
        self.store.is_some()
    }

    pub(crate) async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Option<Result<(), SkillStoreError>> {
        let store = self.store.as_ref()?;
        let workspace = definition.workspace_id.clone();
        let result = store.create(definition, initial_version).await;
        if result.is_ok() {
            self.reload_cache_in(&workspace).await;
        }
        Some(result)
    }

    pub(crate) async fn append_version(
        &self,
        workspace: &str,
        id: &str,
        version: SkillVersion,
    ) -> Option<Result<(), SkillStoreError>> {
        let store = self.store.as_ref()?;
        let result = store.append_version(workspace, id, version).await;
        if result.is_ok() {
            self.reload_cache_in(workspace).await;
        }
        Some(result)
    }

    /// Persist one agent-authored `SKILL.md` as an immutable resource version.
    /// Re-harvesting identical bytes is a no-op; changed bytes append exactly one
    /// version. The caller already supplies the trusted Workspace scope.
    pub(crate) async fn persist_authored(
        &self,
        workspace: &str,
        raw_id: &str,
        content: &str,
    ) -> Option<Result<(), SkillStoreError>> {
        let store = self.store.as_ref()?;
        let id = awaken_skill_store::sanitize_stem(raw_id);
        let existing = match store.definition(workspace, &id).await {
            Ok(value) => value,
            Err(error) => return Some(Err(error)),
        };
        let next = existing
            .as_ref()
            .map_or(1, |definition| definition.latest_version + 1);
        if let Some(definition) = &existing {
            match store
                .version(workspace, &id, definition.latest_version)
                .await
            {
                Ok(Some(latest))
                    if latest
                        .skill_md()
                        .is_some_and(|bytes| bytes == content.as_bytes()) =>
                {
                    return Some(Ok(()));
                }
                Ok(_) => {}
                Err(error) => return Some(Err(error)),
            }
        }
        let parsed = awaken_ext_skills::parse_skill_md(&id, content);
        let files = vec![awaken_skill_store::SkillBundleFile {
            path: "SKILL.md".into(),
            content: content.as_bytes().to_vec(),
        }];
        let version = SkillVersion {
            id: format!("skver_{id}_{next}"),
            skill_id: id.clone(),
            version: next,
            name: parsed.name,
            description: parsed.description,
            directory: format!("/skills/{id}"),
            bundle_sha256: awaken_skill_store::bundle_sha256(&files),
            files,
        };
        let result = if existing.is_some() {
            store.append_version(workspace, &id, version).await
        } else {
            store
                .create(
                    SkillDefinition {
                        id,
                        workspace_id: workspace.to_string(),
                        display_title: None,
                        latest_version: 1,
                        last_version: 1,
                    },
                    version,
                )
                .await
        };
        if result.is_ok() {
            self.reload_cache_in(workspace).await;
        }
        Some(result)
    }

    pub(crate) async fn definition(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<Result<Option<SkillDefinition>, SkillStoreError>> {
        Some(self.store.as_ref()?.definition(workspace, id).await)
    }

    pub(crate) async fn definitions(&self, workspace: &str) -> Vec<SkillDefinition> {
        match self.store.as_ref() {
            Some(store) => store.list_definitions(workspace).await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    pub(crate) async fn versions(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<Result<Vec<SkillVersion>, SkillStoreError>> {
        Some(self.store.as_ref()?.list_versions(workspace, id).await)
    }

    pub(crate) async fn resolve_latest(
        &self,
        workspace: &str,
        ids: &[String],
    ) -> Result<Vec<ResolvedSkillBinding>, SkillStoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| SkillStoreError::Storage("no durable Skill repository".into()))?;
        let mut bindings = Vec::with_capacity(ids.len());
        for id in ids {
            let definition = store
                .definition(workspace, id)
                .await?
                .ok_or_else(|| SkillStoreError::NotFound(id.clone()))?;
            let version = store
                .version(workspace, id, definition.latest_version)
                .await?
                .ok_or_else(|| SkillStoreError::NotFound(id.clone()))?;
            bindings.push(ResolvedSkillBinding {
                skill_id: id.clone(),
                version: version.version,
                bundle_sha256: version.bundle_sha256,
            });
        }
        Ok(bindings)
    }

    pub(crate) async fn load_pinned(
        &self,
        workspace: &str,
        bindings: &[ResolvedSkillBinding],
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| SkillStoreError::Storage("no durable Skill repository".into()))?;
        let mut versions = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let version = store
                .version(workspace, &binding.skill_id, binding.version)
                .await?
                .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?;
            if version.bundle_sha256 != binding.bundle_sha256 {
                return Err(SkillStoreError::Invalid(format!(
                    "Skill {} version {} hash changed",
                    binding.skill_id, binding.version
                )));
            }
            versions.push(version);
        }
        Ok(versions)
    }

    pub(crate) async fn delete_version(
        &self,
        workspace: &str,
        id: &str,
        version: u64,
    ) -> Option<Result<bool, SkillStoreError>> {
        let store = self.store.as_ref()?;
        let result = store.delete_version(workspace, id, version).await;
        if result.as_ref().is_ok_and(|removed| *removed) {
            self.reload_cache_in(workspace).await;
        }
        Some(result)
    }

    pub(crate) async fn delete(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<Result<bool, SkillStoreError>> {
        let store = self.store.as_ref()?;
        let result = store.delete_skill(workspace, id).await;
        if result.as_ref().is_ok_and(|removed| *removed) {
            self.reload_cache_in(workspace).await;
        }
        Some(result)
    }

    pub(crate) async fn purge(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<Result<u64, SkillStoreError>> {
        Some(self.store.as_ref()?.purge_skill(workspace, id).await)
    }

    /// Refresh the in-memory delivered-catalog snapshot from the async store. Called
    /// on a write and at each session's setup so the sync read paths (advertisement,
    /// run-loop scan) see the current catalog.
    pub(crate) async fn reload_cache_in(&self, workspace: &str) {
        if let Some(store) = self.store.as_ref() {
            let definitions = store.list_definitions(workspace).await.unwrap_or_default();
            let mut snapshot = Vec::with_capacity(definitions.len());
            for definition in definitions {
                if let Ok(Some(version)) = store
                    .version(workspace, &definition.id, definition.latest_version)
                    .await
                {
                    snapshot.push(version);
                }
            }
            self.cache
                .lock()
                .expect("skill cache poisoned")
                .insert(workspace.to_string(), snapshot);
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

    /// The skill ids offered on every thread (advertised as the agent's `skills`):
    /// the static configured set plus any durable `/v1/skills` catalog, de-duplicated
    /// with the static set winning, so the advertisement matches what `list_skills`
    /// resolves.
    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<String> {
        self.ids_in(&self.local_workspace)
    }

    pub(crate) fn ids_in(&self, workspace: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.iter().map(|s| s.id.clone()).collect();
        // The durable catalog is read from the sync cache (refreshed on write and at
        // session setup); a network-DB store cannot be awaited from this sync path.
        // A durable skill is advertised by its tagged catalog id (not its name) so the
        // official worker can download it — `/v1/skills` resolves the same id.
        for version in self.cache_snapshot_in(workspace) {
            if !ids.contains(&version.skill_id) {
                ids.push(version.skill_id);
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_skill_store::{InMemorySkillStore, SkillBundleFile, bundle_sha256};

    fn version(id: &str, ordinal: u64, body: &str) -> SkillVersion {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: body.as_bytes().to_vec(),
        }];
        SkillVersion {
            id: format!("skver-{id}-{ordinal}"),
            skill_id: id.into(),
            version: ordinal,
            name: id.into(),
            description: String::new(),
            directory: format!("/skills/{id}"),
            bundle_sha256: bundle_sha256(&files),
            files,
        }
    }

    #[tokio::test]
    async fn empty_binding_is_the_identity_without_a_repository() {
        let catalog = SkillCatalog::new("ws-a".into());
        assert!(
            catalog
                .resolve_latest("ws-a", &[])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(catalog.load_pinned("ws-a", &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn frozen_binding_keeps_v1_after_v2_and_is_workspace_scoped() {
        let mut catalog = SkillCatalog::new("ws-a".into());
        catalog.set_store(Arc::new(InMemorySkillStore::new()));
        catalog
            .create(
                SkillDefinition {
                    id: "greet".into(),
                    workspace_id: "ws-a".into(),
                    display_title: None,
                    latest_version: 1,
                    last_version: 1,
                },
                version("greet", 1, "---\ndescription: v1\n---\nONE"),
            )
            .await
            .unwrap()
            .unwrap();

        let frozen = catalog
            .resolve_latest("ws-a", &["greet".into()])
            .await
            .unwrap();
        catalog
            .append_version(
                "ws-a",
                "greet",
                version("greet", 2, "---\ndescription: v2\n---\nTWO"),
            )
            .await
            .unwrap()
            .unwrap();

        let loaded = catalog.load_pinned("ws-a", &frozen).await.unwrap();
        assert_eq!(loaded[0].version, 1);
        assert!(loaded[0].skill_md().unwrap().ends_with(b"ONE"));
        assert!(catalog.load_pinned("ws-b", &frozen).await.is_err());
    }
}
