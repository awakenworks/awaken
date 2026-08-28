//! `SkillCatalog` — the host's skill offering plus its durable-catalog cache coherence.
//!
//! Groups the three skill fields that were flat on [`crate::SharedHost`] (the static
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
use awaken_session_contract::{ResolvedSkillBinding, RunError};

use awaken_session_contract::{SkillBundleSource, SkillCatalogApplication};

/// Keep repository failure classification beside the one durable Skill owner so
/// every create, restore, and live-apply path shares the same closed algebra.
pub(crate) fn skill_store_run_error(error: SkillStoreError) -> RunError {
    let message = error.to_string();
    match error {
        SkillStoreError::NotFound(_) | SkillStoreError::Invalid(_) => {
            RunError::bad_request(message)
        }
        SkillStoreError::Io(_) => RunError::unavailable(message),
        SkillStoreError::AlreadyExists(_)
        | SkillStoreError::VersionConflict(_)
        | SkillStoreError::Storage(_) => RunError::internal(message),
    }
}

fn anthropic_skill(id: &str) -> Option<SkillVersion> {
    let description = match id {
        "pptx" => "Create, inspect, and modify PowerPoint presentations",
        "xlsx" => "Create, inspect, and modify Excel workbooks",
        "docx" => "Create, inspect, and modify Word documents",
        "pdf" => "Create, inspect, and modify PDF documents",
        _ => return None,
    };
    let body = format!(
        "---\nname: {id}\ndescription: {description}\n---\nUse the available sandbox tools and libraries to {description}. Validate the generated artifact before returning it."
    );
    let files = vec![awaken_resource_contract::SkillBundleFile {
        path: "SKILL.md".into(),
        content: body.into_bytes(),
        executable: false,
    }];
    Some(SkillVersion {
        id: format!("anthropic-{id}-1").into(),
        skill_id: id.into(),
        version: 1,
        name: id.into(),
        description: description.into(),
        directory: format!("/skills/{id}"),
        bundle_sha256: awaken_resource_contract::skill_bundle_sha256(&files),
        files,
        created_unix_nanos: 0,
    })
}

/// Skills offered on every thread, plus the durable delivered-catalog and its
/// synchronous read cache. See the module docs for the coherence invariant.
pub(crate) struct SkillCatalog {
    /// Skills offered on every thread (ADR-0036). The whole set is fronted by the
    /// single `Skill` tool; the model activates one by id to load its instructions.
    specs: Vec<SkillSpec>,
    /// An optional durable delivered-skill catalog (resources plane). When set, its
    /// immutable versions are offered alongside the static `specs` and survive a restart, so
    /// a catalog configured through `/v1/skills` outlives the process. The host reads
    /// the bytes and feeds them to the extension's `SkillSource`, so the runtime stays
    /// store-unaware.
    application: Option<Arc<dyn SkillCatalogApplication>>,
    #[cfg(any(test, feature = "test-support"))]
    test_store: Option<Arc<dyn awaken_resource_contract::SkillStore>>,
    /// Exact immutable custom-Skill bytes used during Session realization. A
    /// Coordinator installs the local store adapter; an execution Worker installs
    /// the claim-fenced HTTP adapter and never opens the authoring store.
    bundle_source: Option<Arc<dyn SkillBundleSource<awaken_run_ingress::RunClaim>>>,
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
            bundle_source: None,
            cache: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Builder: append the configured static skills.
    pub(crate) fn add_specs(&mut self, skills: Vec<SkillSpec>) {
        self.specs.extend(skills);
    }

    /// Builder: wire the durable delivered-skill catalog.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set_store(&mut self, store: Arc<dyn awaken_resource_contract::SkillStore>) {
        self.bundle_source = Some(Arc::new(
            awaken_resource_application::StoreSkillBundleSource::new(store.clone()),
        ));
        self.application = Some(Arc::new(
            awaken_resource_application::StoreSkillCatalogApplication::new(store.clone()),
        ));
        self.test_store = Some(store);
    }

    pub(crate) fn set_application(&mut self, application: Arc<dyn SkillCatalogApplication>) {
        self.application = Some(application);
    }

    /// Builder: wire only exact custom-Skill bundle reads for an execution Worker.
    pub(crate) fn set_bundle_source(
        &mut self,
        source: Arc<dyn SkillBundleSource<awaken_run_ingress::RunClaim>>,
    ) {
        self.bundle_source = Some(source);
    }

    /// The configured static skills offered on every thread.
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

    /// Persist one agent-authored `SKILL.md` as an immutable resource version.
    /// Re-harvesting identical bytes is a no-op; changed bytes append exactly one
    /// version. The caller already supplies the trusted Workspace scope.
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
            .map_err(SkillStoreError::Invalid)?;
        let mut bindings = Vec::with_capacity(selections.len());
        for selection in selections {
            let version = match selection.kind {
                AgentSkillKind::Anthropic => anthropic_skill(&selection.skill_id)
                    .ok_or_else(|| SkillStoreError::NotFound(selection.skill_id.clone()))?,
                AgentSkillKind::Custom => {
                    let binding = self
                        .application
                        .as_ref()
                        .ok_or_else(|| {
                            SkillStoreError::Storage("no Skill catalog application".into())
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

    pub(crate) async fn load_pinned(
        &self,
        workspace: &str,
        bindings: &[ResolvedSkillBinding],
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        let mut versions = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let version = match binding.kind {
                AgentSkillKind::Anthropic => anthropic_skill(&binding.skill_id)
                    .filter(|version| version.version == binding.version)
                    .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?,
                AgentSkillKind::Custom => self
                    .bundle_source
                    .as_ref()
                    .ok_or_else(|| SkillStoreError::Storage("no Skill bundle source".into()))?
                    .load(workspace, binding, claim)
                    .await
                    .map_err(|error| SkillStoreError::Storage(error.to_string()))?
                    .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?,
            };
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
        self.ids_in(workspace)
            .iter()
            .any(|id| selected.is_none_or(|ids| ids.contains(id)))
    }

    /// The skill ids offered on every thread (advertised as the agent's `skills`):
    /// the static configured set plus any durable `/v1/skills` catalog, de-duplicated
    /// with the static set winning, so the advertisement matches what `list_skills`
    /// resolves.
    pub(crate) fn ids_in(&self, workspace: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.iter().map(|s| s.id.clone()).collect();
        // The durable catalog is read from the sync cache (refreshed on write and at
        // session setup); a network-DB store cannot be awaited from this sync path.
        // A durable skill is advertised by its tagged catalog id (not its name) so the
        // official worker can download it — `/v1/skills` resolves the same id.
        for version in self.cache_snapshot_in(workspace) {
            if !ids.iter().any(|id| id == version.skill_id.as_str()) {
                ids.push(version.skill_id.to_string());
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_contract::RunErrorKind;
    use awaken_skill_store::{InMemorySkillStore, SkillBundleFile, bundle_sha256};

    fn version(id: &str, ordinal: u64, body: &str) -> SkillVersion {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: body.as_bytes().to_vec(),
            executable: false,
        }];
        SkillVersion {
            id: format!("skver-{id}-{ordinal}").into(),
            skill_id: id.into(),
            version: ordinal,
            name: id.into(),
            description: String::new(),
            directory: format!("/skills/{id}"),
            bundle_sha256: bundle_sha256(&files),
            files,
            created_unix_nanos: ordinal,
        }
    }

    #[test]
    fn session_skill_repository_failures_follow_the_closed_error_table() {
        // Cause/effect decision table: caller-controlled missing/invalid
        // selectors => 400; temporary filesystem I/O => 503; impossible write
        // conflicts or durable storage/corruption => 500. Every runtime Skill
        // entry point uses this one adapter so create, restore, and live apply
        // cannot classify the same repository cause differently.
        let rules = [
            (
                SkillStoreError::NotFound("x".into()),
                RunErrorKind::BadRequest,
            ),
            (
                SkillStoreError::Invalid("x".into()),
                RunErrorKind::BadRequest,
            ),
            (SkillStoreError::Io("x".into()), RunErrorKind::Unavailable),
            (
                SkillStoreError::AlreadyExists("x".into()),
                RunErrorKind::Internal,
            ),
            (
                SkillStoreError::VersionConflict("x".into()),
                RunErrorKind::Internal,
            ),
            (SkillStoreError::Storage("x".into()), RunErrorKind::Internal),
        ];
        for (cause, effect) in rules {
            assert_eq!(skill_store_run_error(cause).kind, effect);
        }
    }

    #[tokio::test]
    async fn empty_pinned_skill_set_needs_no_repository_but_real_references_fail_closed() {
        let catalog = SkillCatalog::new();
        assert!(
            catalog
                .load_pinned("ws-a", &[], None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            catalog
                .resolve("ws-a", &[AgentSkillBinding::custom("missing")])
                .await
                .is_err()
        );
        assert!(
            catalog
                .load_pinned(
                    "ws-a",
                    &[ResolvedSkillBinding {
                        kind: AgentSkillKind::Custom,
                        skill_id: "missing".into(),
                        version: 1,
                        bundle_sha256: "sha256".into(),
                    }],
                    None,
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn frozen_binding_keeps_v1_after_v2_and_is_workspace_scoped() {
        let mut catalog = SkillCatalog::new();
        catalog.set_store(Arc::new(InMemorySkillStore::new()));
        catalog
            .create(
                SkillDefinition {
                    id: "greet".into(),
                    workspace_id: "ws-a".into(),
                    display_title: None,
                    latest_version: 1,
                    last_version: 1,
                    timestamps: Default::default(),
                },
                version("greet", 1, "---\ndescription: v1\n---\nONE"),
            )
            .await
            .unwrap()
            .unwrap();

        let frozen = catalog
            .resolve("ws-a", &[AgentSkillBinding::custom("greet")])
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

        let loaded = catalog.load_pinned("ws-a", &frozen, None).await.unwrap();
        assert_eq!(loaded[0].version, 1);
        assert!(loaded[0].skill_md().unwrap().ends_with(b"ONE"));
        assert!(catalog.load_pinned("ws-b", &frozen, None).await.is_err());
    }

    #[tokio::test]
    async fn prebuilt_and_custom_selectors_follow_one_resolution_table() {
        // Causes: prebuilt/custom source, latest/exact selector, repository
        // presence, and Workspace. Constraints: prebuilt v1 is runtime-owned;
        // custom bytes are Workspace-owned. Effects: both freeze exact hashes,
        // prebuilt needs no store, and custom exact selection never drifts.
        // Decision rules: S8 prebuilt latest/1 -> bundled v1; S9 prebuilt v2 ->
        // reject; S10 custom latest/exact -> selected store version.
        let mut catalog = SkillCatalog::new();
        for selector in ["latest", "1"] {
            let selection = AgentSkillBinding {
                kind: AgentSkillKind::Anthropic,
                skill_id: "xlsx".into(),
                version: selector.into(),
            };
            let frozen = catalog.resolve("ws-a", &[selection]).await.unwrap();
            assert_eq!(frozen[0].kind, AgentSkillKind::Anthropic, "S8");
            assert_eq!(
                catalog.load_pinned("ws-a", &frozen, None).await.unwrap()[0].version,
                1
            );
        }
        assert!(
            catalog
                .resolve(
                    "ws-a",
                    &[AgentSkillBinding {
                        kind: AgentSkillKind::Anthropic,
                        skill_id: "xlsx".into(),
                        version: "2".into(),
                    }],
                )
                .await
                .is_err(),
            "S9"
        );

        catalog.set_store(Arc::new(InMemorySkillStore::new()));
        catalog
            .create(
                SkillDefinition {
                    id: "greet".into(),
                    workspace_id: "ws-a".into(),
                    display_title: None,
                    latest_version: 1,
                    last_version: 1,
                    timestamps: Default::default(),
                },
                version("greet", 1, "---\ndescription: v1\n---\nONE"),
            )
            .await
            .unwrap()
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
        for (selector, expected) in [("latest", 2), ("1", 1)] {
            let frozen = catalog
                .resolve(
                    "ws-a",
                    &[AgentSkillBinding {
                        kind: AgentSkillKind::Custom,
                        skill_id: "greet".into(),
                        version: selector.into(),
                    }],
                )
                .await
                .unwrap();
            assert_eq!(frozen[0].version, expected, "S10");
        }
    }
}
