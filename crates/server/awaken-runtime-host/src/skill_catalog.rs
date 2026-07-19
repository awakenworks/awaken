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
use awaken_skill_store::SkillStore;

use crate::provisioning::HOST_SKILL_WORKSPACE;

/// Skills offered on every thread, plus the durable delivered-catalog and its
/// synchronous read cache. See the module docs for the coherence invariant.
pub(crate) struct SkillCatalog {
    /// Skills offered on every thread (ADR-0036). The whole set is fronted by the
    /// single `Skill` tool; the model activates one by id to load its instructions.
    specs: Vec<SkillSpec>,
    /// An optional durable delivered-skill catalog (resources plane). When set, its
    /// `SKILL.md`s are offered alongside the static `specs` and survive a restart, so
    /// a catalog configured through `/v1/skills` outlives the process. The host reads
    /// the bytes and feeds them to the extension's `SkillSource`, so the runtime stays
    /// store-unaware.
    store: Option<Arc<dyn SkillStore>>,
    /// In-memory snapshot of the delivered catalog `(id, content)`, read
    /// *synchronously* by the capability advertisement (`ids`) and the run-loop
    /// `SkillSource` scan — refreshed from the async `store` on a write and at each
    /// session's setup (`reload_cache`). This is how a network-DB (async) catalog
    /// serves the host's sync read paths.
    cache: Mutex<Vec<(String, String)>>,
}

impl SkillCatalog {
    /// An empty catalog: no static skills, no durable store. The builder wires the
    /// configured set and (optionally) a store.
    pub(crate) fn new() -> Self {
        Self {
            specs: Vec::new(),
            store: None,
            cache: Mutex::new(Vec::new()),
        }
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

    /// Store (or overwrite) a delivered skill's `SKILL.md` `content` under `id` in the
    /// durable catalog, returning the safe id it is addressable by. `None` when this
    /// host has no durable skill store wired (nothing to persist into).
    pub(crate) async fn store_put(&self, id: &str, content: &str) -> Option<String> {
        let store = self.store.as_ref()?;
        let out = store
            .put(HOST_SKILL_WORKSPACE, id, content)
            .await
            .expect("persist durable skill");
        // Keep the sync-read cache current for advertisement + scan.
        self.reload_cache().await;
        Some(out)
    }

    /// The ids currently in the durable skill catalog (read straight from the store,
    /// so the CRUD `list` reflects any peer node's writes). Empty when no store is
    /// wired.
    pub(crate) async fn store_list(&self) -> Vec<String> {
        match self.store.as_ref() {
            Some(store) => store
                .list(HOST_SKILL_WORKSPACE)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Delete one delivered skill and refresh every synchronous projection. Tagged
    /// catalog ids are resolved through the cache to the durable stem; raw legacy
    /// ids are accepted directly. `None` means no durable store is configured.
    pub(crate) async fn store_delete(&self, id: &str) -> Option<bool> {
        let store = self.store.as_ref()?;
        let durable_id = self
            .cache_snapshot()
            .into_iter()
            .find(|(stem, _)| awaken_skill_store::catalog_id(stem) == id || stem == id)
            .map_or_else(|| id.to_string(), |(stem, _)| stem);
        let removed = store
            .delete(HOST_SKILL_WORKSPACE, &durable_id)
            .await
            .expect("delete durable skill");
        self.reload_cache().await;
        Some(removed)
    }

    /// Refresh the in-memory delivered-catalog snapshot from the async store. Called
    /// on a write and at each session's setup so the sync read paths (advertisement,
    /// run-loop scan) see the current catalog.
    pub(crate) async fn reload_cache(&self) {
        if let Some(store) = self.store.as_ref() {
            let snapshot = store.list(HOST_SKILL_WORKSPACE).await.unwrap_or_default();
            *self.cache.lock().expect("skill cache poisoned") = snapshot;
        }
    }

    /// A clone of the cached delivered catalog `(id, content)` — the synchronous read
    /// the host's `SkillSource` bridge scans (the run-loop scan cannot await).
    pub(crate) fn cache_snapshot(&self) -> Vec<(String, String)> {
        self.cache.lock().expect("skill cache poisoned").clone()
    }

    /// The skill ids offered on every thread (advertised as the agent's `skills`):
    /// the static configured set plus any durable `/v1/skills` catalog, de-duplicated
    /// with the static set winning, so the advertisement matches what `list_skills`
    /// resolves.
    pub(crate) fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.iter().map(|s| s.id.clone()).collect();
        // The durable catalog is read from the sync cache (refreshed on write and at
        // session setup); a network-DB store cannot be awaited from this sync path.
        // A durable skill is advertised by its tagged catalog id (not its name) so the
        // official worker can download it — `/v1/skills` resolves the same id.
        for (stem, _) in self.cache_snapshot() {
            let id = awaken_skill_store::catalog_id(&stem);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// Resolve an advertised durable-catalog skill id back to its `(stem, content)`.
    /// Accepts the tagged catalog id (what advertisement + the worker use) and, as a
    /// courtesy, the raw durable stem. Lets the `/v1/skills` read paths serve any
    /// advertised id even for skills that never went through the SDK create route
    /// (harvested / legacy-delivered), where the in-memory registry has no entry.
    pub(crate) fn by_catalog_id(&self, id: &str) -> Option<(String, String)> {
        self.cache_snapshot()
            .into_iter()
            .find(|(stem, _)| awaken_skill_store::catalog_id(stem) == id || stem == id)
    }
}
