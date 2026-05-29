//! Published registry graph validation contracts and default validator.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use awaken_runtime_contract::registry_spec::{AgentSpec, ModelPoolSpec, ModelSpec};

use super::versioned_registry::{
    PinnedRegistryEntry, PinnedRegistryManifest, VersionRef, VersionedRecord,
    VersionedRegistryError, VersionedRegistryStore,
};

pub const REGISTRY_KIND_AGENT: &str = "agent";
pub const REGISTRY_KIND_MODEL: &str = "model";
pub const REGISTRY_KIND_MODEL_POOL: &str = "model_pool";
pub const REGISTRY_KIND_PROVIDER: &str = "provider";
pub const REGISTRY_KIND_SKILL: &str = "skill";
pub const REGISTRY_KIND_TOOL: &str = "tool";
pub const REGISTRY_KIND_PLUGIN_CONFIG: &str = "plugin_config";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VersionSelector {
    LatestPublication {
        scope_id: String,
    },
    Publication {
        scope_id: String,
        snapshot_version: u64,
    },
    Exact {
        scope_id: String,
        kind: String,
        id: String,
        version: u64,
    },
    Manifest {
        scope_id: String,
        manifest: PinnedRegistryManifest,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RegistryReferencePolicy {
    #[default]
    SameScopeOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryGraphValidationRequest {
    pub root: VersionSelector,
    #[serde(default)]
    pub reference_policy: RegistryReferencePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryGraphValidationReport {
    pub entries: Vec<PinnedRegistryEntry>,
}

#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum RegistryGraphValidationError {
    #[error("missing registry resource {kind}/{id}")]
    MissingResource { kind: String, id: String },
    #[error("missing registry version {kind}/{id}@{version}")]
    MissingVersion {
        kind: String,
        id: String,
        version: u64,
    },
    #[error("archived registry reference {kind}/{id}{version_suffix}", version_suffix = version.map_or(String::new(), |version| format!("@{version}")))]
    ArchivedReference {
        kind: String,
        id: String,
        version: Option<u64>,
    },
    #[error(
        "content hash mismatch for {kind}/{id}@{version}: expected {expected}, actual {actual}"
    )]
    ContentHashMismatch {
        kind: String,
        id: String,
        version: u64,
        expected: String,
        actual: String,
    },
    #[error("registry graph cycle detected: {path:?}")]
    CycleDetected { path: Vec<VersionRef> },
    #[error("invalid registry reference {kind}/{id}: {reason}")]
    InvalidReference {
        kind: String,
        id: String,
        reason: String,
    },
    #[error("registry validation backend error: {0}")]
    Backend(String),
}

impl From<VersionedRegistryError> for RegistryGraphValidationError {
    fn from(error: VersionedRegistryError) -> Self {
        Self::Backend(error.to_string())
    }
}

#[async_trait]
pub trait RegistryGraphValidator: Send + Sync {
    async fn validate(
        &self,
        request: RegistryGraphValidationRequest,
    ) -> Result<RegistryGraphValidationReport, RegistryGraphValidationError>;
}

pub struct StandardRegistryGraphValidator {
    store: Arc<dyn VersionedRegistryStore>,
}

impl StandardRegistryGraphValidator {
    #[must_use]
    pub fn new(store: Arc<dyn VersionedRegistryStore>) -> Self {
        Self { store }
    }

    async fn root_context(
        &self,
        root: VersionSelector,
    ) -> Result<ValidationContext, RegistryGraphValidationError> {
        match root {
            VersionSelector::LatestPublication { scope_id } => {
                let manifest = self
                    .store
                    .latest_pinned_manifest(&scope_id)
                    .await?
                    .ok_or_else(|| RegistryGraphValidationError::MissingResource {
                        kind: "publication".to_string(),
                        id: "latest".to_string(),
                    })?;
                ValidationContext::from_manifest(scope_id, manifest, false, true)
            }
            VersionSelector::Publication {
                scope_id,
                snapshot_version,
            } => {
                let manifest = self
                    .store
                    .pinned_manifest_for_publication(&scope_id, snapshot_version)
                    .await?
                    .ok_or_else(|| RegistryGraphValidationError::MissingVersion {
                        kind: "publication".to_string(),
                        id: scope_id.clone(),
                        version: snapshot_version,
                    })?;
                ValidationContext::from_manifest(scope_id, manifest, false, false)
            }
            VersionSelector::Manifest { scope_id, manifest } => {
                ValidationContext::from_manifest(scope_id, manifest, false, false)
            }
            VersionSelector::Exact {
                scope_id,
                kind,
                id,
                version,
            } => {
                let record = self.load_record(&scope_id, &kind, &id, version).await?;
                let entry = PinnedRegistryEntry {
                    kind,
                    id,
                    version,
                    content_hash: record.content_hash,
                };
                ValidationContext::from_entries(scope_id, vec![entry], true, false)
            }
        }
    }

    async fn load_record(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        version: u64,
    ) -> Result<VersionedRecord<serde_json::Value>, RegistryGraphValidationError> {
        self.store
            .get(scope_id, kind, id, version)
            .await?
            .ok_or_else(|| RegistryGraphValidationError::MissingVersion {
                kind: kind.to_string(),
                id: id.to_string(),
                version,
            })
    }

    fn validate_entry<'a>(
        &'a self,
        context: &'a mut ValidationContext,
        entry: PinnedRegistryEntry,
    ) -> BoxFuture<'a, Result<(), RegistryGraphValidationError>> {
        Box::pin(async move {
            let key = ResourceKey::from_entry(&entry);
            if let Some(existing) = context.accepted.get(&key) {
                if existing.version == entry.version && existing.content_hash == entry.content_hash
                {
                    return Ok(());
                }
                return Err(RegistryGraphValidationError::InvalidReference {
                    kind: entry.kind,
                    id: entry.id,
                    reason: "conflicting versions for the same resource".to_string(),
                });
            }

            if let Some(position) = context.visiting.iter().position(|visited| {
                visited.kind == entry.kind
                    && visited.id == entry.id
                    && visited.version == entry.version
            }) {
                let mut path = context.visiting[position..].to_vec();
                path.push(VersionRef {
                    kind: entry.kind,
                    id: entry.id,
                    version: entry.version,
                });
                return Err(RegistryGraphValidationError::CycleDetected { path });
            }

            let record = self
                .validate_stored_entry(
                    &context.scope_id,
                    &entry,
                    context.reject_archived_explicit_entries,
                )
                .await?;
            context.visiting.push(VersionRef {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                version: entry.version,
            });
            let dependencies = self
                .dependencies_for_record(context, &entry, &record)
                .await?;
            for dependency in dependencies {
                self.validate_entry(context, dependency).await?;
            }
            context.visiting.pop();
            context.accepted.insert(key, entry);
            Ok(())
        })
    }

    async fn validate_stored_entry(
        &self,
        scope_id: &str,
        entry: &PinnedRegistryEntry,
        reject_archived: bool,
    ) -> Result<VersionedRecord<serde_json::Value>, RegistryGraphValidationError> {
        if reject_archived {
            self.reject_archived(scope_id, &entry.kind, &entry.id, Some(entry.version))
                .await?;
        }
        let record = self
            .load_record(scope_id, &entry.kind, &entry.id, entry.version)
            .await?;
        // ADR-0035 D3/D9: re-derive the SHA-256 from canonical_json_bytes
        // and compare to the stored content_hash before trusting either
        // column. Without this the manifest hash becomes decorative.
        record
            .verify_content_hash()
            .map_err(|error| RegistryGraphValidationError::Backend(error.to_string()))?;
        if record.content_hash != entry.content_hash {
            return Err(RegistryGraphValidationError::ContentHashMismatch {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                version: entry.version,
                expected: entry.content_hash.clone(),
                actual: record.content_hash,
            });
        }
        Ok(record)
    }

    async fn reject_archived(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        version: Option<u64>,
    ) -> Result<(), RegistryGraphValidationError> {
        let state = self.store.resource_state(scope_id, kind, id).await?;
        let state = state.ok_or_else(|| RegistryGraphValidationError::MissingResource {
            kind: kind.to_string(),
            id: id.to_string(),
        })?;
        if state.archived_at_ms.is_some() {
            return Err(RegistryGraphValidationError::ArchivedReference {
                kind: kind.to_string(),
                id: id.to_string(),
                version,
            });
        }
        Ok(())
    }

    async fn dependencies_for_record(
        &self,
        context: &ValidationContext,
        entry: &PinnedRegistryEntry,
        record: &VersionedRecord<serde_json::Value>,
    ) -> Result<Vec<PinnedRegistryEntry>, RegistryGraphValidationError> {
        match entry.kind.as_str() {
            REGISTRY_KIND_AGENT => self.agent_dependencies(context, entry, record).await,
            REGISTRY_KIND_MODEL => self.model_dependencies(context, entry, record).await,
            REGISTRY_KIND_MODEL_POOL => self.model_pool_dependencies(context, entry, record).await,
            REGISTRY_KIND_PROVIDER
            | REGISTRY_KIND_SKILL
            | REGISTRY_KIND_TOOL
            | REGISTRY_KIND_PLUGIN_CONFIG => Ok(Vec::new()),
            _ => Err(RegistryGraphValidationError::InvalidReference {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                reason: "unsupported registry kind".to_string(),
            }),
        }
    }

    async fn agent_dependencies(
        &self,
        context: &ValidationContext,
        entry: &PinnedRegistryEntry,
        record: &VersionedRecord<serde_json::Value>,
    ) -> Result<Vec<PinnedRegistryEntry>, RegistryGraphValidationError> {
        let spec = serde_json::from_value::<AgentSpec>(record.value.clone()).map_err(|error| {
            RegistryGraphValidationError::InvalidReference {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                reason: format!("invalid AgentSpec: {error}"),
            }
        })?;
        if spec.id != entry.id {
            return Err(RegistryGraphValidationError::InvalidReference {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                reason: format!("AgentSpec.id {} does not match registry id", spec.id),
            });
        }

        let mut dependencies = Vec::new();
        if spec.endpoint.is_none() {
            dependencies.push(
                self.resolve_model_or_pool_reference(context, &spec.model_id)
                    .await?,
            );
        }
        for delegate_id in &spec.delegates {
            dependencies.push(
                self.resolve_reference_entry(context, REGISTRY_KIND_AGENT, delegate_id)
                    .await?,
            );
        }
        Ok(dependencies)
    }

    async fn model_dependencies(
        &self,
        context: &ValidationContext,
        entry: &PinnedRegistryEntry,
        record: &VersionedRecord<serde_json::Value>,
    ) -> Result<Vec<PinnedRegistryEntry>, RegistryGraphValidationError> {
        let spec = serde_json::from_value::<ModelSpec>(record.value.clone()).map_err(|error| {
            RegistryGraphValidationError::InvalidReference {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                reason: format!("invalid ModelSpec: {error}"),
            }
        })?;
        if spec.id != entry.id {
            return Err(RegistryGraphValidationError::InvalidReference {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                reason: format!("ModelSpec.id {} does not match registry id", spec.id),
            });
        }
        Ok(vec![
            self.resolve_reference_entry(context, REGISTRY_KIND_PROVIDER, &spec.provider_id)
                .await?,
        ])
    }

    async fn model_pool_dependencies(
        &self,
        context: &ValidationContext,
        entry: &PinnedRegistryEntry,
        record: &VersionedRecord<serde_json::Value>,
    ) -> Result<Vec<PinnedRegistryEntry>, RegistryGraphValidationError> {
        let spec =
            serde_json::from_value::<ModelPoolSpec>(record.value.clone()).map_err(|error| {
                RegistryGraphValidationError::InvalidReference {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                    reason: format!("invalid ModelPoolSpec: {error}"),
                }
            })?;
        if spec.id != entry.id {
            return Err(RegistryGraphValidationError::InvalidReference {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                reason: format!("ModelPoolSpec.id {} does not match registry id", spec.id),
            });
        }
        let mut dependencies = Vec::with_capacity(spec.members.len());
        for member in &spec.members {
            dependencies.push(
                self.resolve_reference_entry(context, REGISTRY_KIND_MODEL, &member.model_id)
                    .await?,
            );
        }
        Ok(dependencies)
    }

    /// Resolve an agent's `model_id`, which may name either a single model or a
    /// pool (pools share the model id namespace). An id that resolves to *both*
    /// a model and a pool is ambiguous and rejected, matching the runtime
    /// resolver's `AmbiguousModelReference`; resolving to exactly one is
    /// returned; resolving to neither reports a missing model.
    async fn resolve_model_or_pool_reference(
        &self,
        context: &ValidationContext,
        id: &str,
    ) -> Result<PinnedRegistryEntry, RegistryGraphValidationError> {
        let model = self
            .try_reference_entry(context, REGISTRY_KIND_MODEL, id)
            .await?;
        let pool = self
            .try_reference_entry(context, REGISTRY_KIND_MODEL_POOL, id)
            .await?;
        match (model, pool) {
            (Some(_), Some(_)) => Err(RegistryGraphValidationError::InvalidReference {
                kind: REGISTRY_KIND_MODEL.to_string(),
                id: id.to_string(),
                reason: "id resolves to both a model and a model pool".to_string(),
            }),
            (Some(entry), None) | (None, Some(entry)) => Ok(entry),
            (None, None) => Err(RegistryGraphValidationError::MissingResource {
                kind: REGISTRY_KIND_MODEL.to_string(),
                id: id.to_string(),
            }),
        }
    }

    /// Like [`resolve_reference_entry`](Self::resolve_reference_entry) but
    /// returns `Ok(None)` when the resource is absent instead of an error, so a
    /// caller can try an alternative kind. Other failures (archived, store)
    /// still propagate.
    async fn try_reference_entry(
        &self,
        context: &ValidationContext,
        kind: &str,
        id: &str,
    ) -> Result<Option<PinnedRegistryEntry>, RegistryGraphValidationError> {
        let key = ResourceKey::new(kind, id);
        if let Some(entry) = context.candidate_entries.get(&key) {
            return Ok(Some(entry.clone()));
        }
        if !context.allow_current_reference_resolution {
            return Ok(None);
        }
        // Honor the soft contract: a resource that simply does not exist for
        // this kind is `Ok(None)` so the caller can try the other kind (an id
        // may name a model OR a pool). `reject_archived` reports absence as
        // `MissingResource`, so probe existence first and only run the archived
        // check when the resource is actually present — an archived resource
        // still propagates as an error.
        if self
            .store
            .resource_state(&context.scope_id, kind, id)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        self.reject_archived(&context.scope_id, kind, id, None)
            .await?;
        let Some(record) = self.store.current(&context.scope_id, kind, id).await? else {
            return Ok(None);
        };
        Ok(Some(PinnedRegistryEntry {
            kind: kind.to_string(),
            id: id.to_string(),
            version: record.version,
            content_hash: record.content_hash,
        }))
    }

    async fn resolve_reference_entry(
        &self,
        context: &ValidationContext,
        kind: &str,
        id: &str,
    ) -> Result<PinnedRegistryEntry, RegistryGraphValidationError> {
        let key = ResourceKey::new(kind, id);
        if let Some(entry) = context.candidate_entries.get(&key) {
            return Ok(entry.clone());
        }
        // ADR-0035 D9: pinned-manifest/publication validation must fail
        // closed when a transitive reference is absent. Only `Exact` opts
        // into expanding through the store's current pointer, and its
        // output is frozen into a manifest before execution begins.
        if !context.allow_current_reference_resolution {
            return Err(RegistryGraphValidationError::MissingResource {
                kind: kind.to_string(),
                id: id.to_string(),
            });
        }
        self.reject_archived(&context.scope_id, kind, id, None)
            .await?;
        let record = self
            .store
            .current(&context.scope_id, kind, id)
            .await?
            .ok_or_else(|| RegistryGraphValidationError::MissingResource {
                kind: kind.to_string(),
                id: id.to_string(),
            })?;
        Ok(PinnedRegistryEntry {
            kind: kind.to_string(),
            id: id.to_string(),
            version: record.version,
            content_hash: record.content_hash,
        })
    }
}

#[async_trait]
impl RegistryGraphValidator for StandardRegistryGraphValidator {
    async fn validate(
        &self,
        request: RegistryGraphValidationRequest,
    ) -> Result<RegistryGraphValidationReport, RegistryGraphValidationError> {
        match request.reference_policy {
            RegistryReferencePolicy::SameScopeOnly => {}
        }
        let mut context = self.root_context(request.root).await?;
        let roots = context.root_entries.clone();
        for entry in roots {
            self.validate_entry(&mut context, entry).await?;
        }
        Ok(context.into_report())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ResourceKey {
    kind: String,
    id: String,
}

impl ResourceKey {
    fn new(kind: &str, id: &str) -> Self {
        Self {
            kind: kind.to_string(),
            id: id.to_string(),
        }
    }

    fn from_entry(entry: &PinnedRegistryEntry) -> Self {
        Self::new(&entry.kind, &entry.id)
    }
}

struct ValidationContext {
    scope_id: String,
    root_entries: Vec<PinnedRegistryEntry>,
    candidate_entries: HashMap<ResourceKey, PinnedRegistryEntry>,
    accepted: BTreeMap<ResourceKey, PinnedRegistryEntry>,
    visiting: Vec<VersionRef>,
    /// Allow the validator to resolve missing references against the
    /// store's current pointer. ADR-0035 D9 forbids this for run-time
    /// resume materialization (Manifest, Publication, LatestPublication
    /// must observe only the frozen entries). It is only enabled for
    /// `Exact`, where the caller asked to expand a single root into its
    /// reachable graph and the resulting `report.entries` will be saved
    /// as the persisted manifest before run execution starts.
    allow_current_reference_resolution: bool,
    reject_archived_explicit_entries: bool,
}

impl ValidationContext {
    fn from_manifest(
        scope_id: String,
        manifest: PinnedRegistryManifest,
        allow_current_reference_resolution: bool,
        reject_archived_explicit_entries: bool,
    ) -> Result<Self, RegistryGraphValidationError> {
        Self::from_entries(
            scope_id,
            manifest.entries,
            allow_current_reference_resolution,
            reject_archived_explicit_entries,
        )
    }

    fn from_entries(
        scope_id: String,
        entries: Vec<PinnedRegistryEntry>,
        allow_current_reference_resolution: bool,
        reject_archived_explicit_entries: bool,
    ) -> Result<Self, RegistryGraphValidationError> {
        let mut candidate_entries = HashMap::new();
        let mut seen = HashSet::new();
        for entry in &entries {
            if entry.version == 0 {
                return Err(RegistryGraphValidationError::InvalidReference {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                    reason: "version cannot be 0".to_string(),
                });
            }
            let key = ResourceKey::from_entry(entry);
            if !seen.insert(key.clone()) {
                return Err(RegistryGraphValidationError::InvalidReference {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                    reason: "duplicate manifest entry".to_string(),
                });
            }
            candidate_entries.insert(key, entry.clone());
        }
        Ok(Self {
            scope_id,
            root_entries: entries,
            candidate_entries,
            accepted: BTreeMap::new(),
            visiting: Vec::new(),
            allow_current_reference_resolution,
            reject_archived_explicit_entries,
        })
    }

    fn into_report(self) -> RegistryGraphValidationReport {
        RegistryGraphValidationReport {
            entries: self.accepted.into_values().collect(),
        }
    }
}
