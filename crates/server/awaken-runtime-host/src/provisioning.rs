//! Session provisioning + capability advertisement on [`SharedHost`]: the sandbox
//! spec (with staged resource mounts, ADR-0038), the per-thread resource staging +
//! blob store accessor, and the tool/skill/delegate sets advertised on a managed
//! session. Split out of `host.rs` to keep that file under the length limit; these
//! are the same `SharedHost` (fields are `pub(crate)`).

use std::sync::Arc;

use crate::host::SharedHost;
use awaken_file_store::FileStore;
use awaken_protocol_managed::resource_plane::{
    CreateFileRecordOutcome, FileCatalog, FileCatalogError, FileRecord, PutResourcePurgeOutcome,
    ResourceKind, ResourcePurgeError, ResourcePurgeIntent, ResourcePurgeScheduler,
    ResourceReference, ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::resolved::ToolDescriptor;

/// The sandbox-absolute outputs dir (must be absolute for `prepare_environment`);
/// resolved under the root to `<root>/outputs`, which `list_files("outputs")` reads.
const OUTPUTS_PATH: &str = "/outputs";
pub const MAX_MANAGED_FILE_SIZE_BYTES: u64 = 500 * 1024 * 1024;
pub const MAX_WORKSPACE_FILE_BYTES: u64 = 500 * 1024 * 1024 * 1024;

/// Serialize the Session-domain resource manifest into the dispatch context's
/// opaque envelope. This one ACL keeps durable ingress independent of Session
/// vocabulary while preserving a lossless, secret-free payload.
pub(crate) fn encode_session_resource_envelope(
    manifest: &awaken_protocol_managed::SessionResourceManifest,
) -> Result<awaken_run_ingress::SessionResourceEnvelope, serde_json::Error> {
    Ok(awaken_run_ingress::SessionResourceEnvelope::new(
        manifest.workspace_id.clone(),
        serde_json::to_string(&manifest.resources)?,
    ))
}

/// Decode the dispatch-neutral envelope back into the Session contract before
/// resource validation or sandbox creation.
pub(crate) fn decode_session_resource_envelope(
    envelope: &awaken_run_ingress::SessionResourceEnvelope,
) -> Result<awaken_protocol_managed::SessionResourceManifest, serde_json::Error> {
    Ok(awaken_protocol_managed::SessionResourceManifest::new(
        envelope.workspace_id.clone(),
        serde_json::from_str(&envelope.resolved_resources_json)?,
    ))
}

fn logical_file_reference(record: &FileRecord) -> ResourceReferenceRecord {
    ResourceReferenceRecord {
        target: ResourceTarget::new(&record.workspace_id, ResourceKind::File, &record.blob_id),
        reference: ResourceReference {
            kind: if record.scope_id.is_some() {
                ResourceReferenceKind::Artifact
            } else {
                ResourceReferenceKind::WorkspaceOwnership
            },
            reference_id: record.id.clone(),
        },
    }
}

fn file_catalog_error(error: FileCatalogError) -> ResourcePurgeError {
    match error {
        FileCatalogError::Invalid(message) => ResourcePurgeError::Invalid(message),
        FileCatalogError::Storage(message) => ResourcePurgeError::Storage(message),
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn validate_file_capacity(file_size: u64, active_size: u64) -> Result<(), ResourcePurgeError> {
    if file_size > MAX_MANAGED_FILE_SIZE_BYTES {
        return Err(ResourcePurgeError::Invalid(format!(
            "file exceeds the {} byte limit",
            MAX_MANAGED_FILE_SIZE_BYTES
        )));
    }
    if active_size.saturating_add(file_size) > MAX_WORKSPACE_FILE_BYTES {
        return Err(ResourcePurgeError::Invalid(format!(
            "Workspace files exceed the {} byte limit",
            MAX_WORKSPACE_FILE_BYTES
        )));
    }
    Ok(())
}

fn mime_type_for_path(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "md" | "log") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

#[async_trait::async_trait]
impl ResourcePurgeScheduler for SharedHost {
    async fn schedule_purge(
        &self,
        target: ResourceTarget,
        config_version: Option<u64>,
        requested_at_unix_ms: u64,
        not_before_unix_ms: u64,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        self.request_resource_purge(
            target,
            config_version,
            requested_at_unix_ms,
            not_before_unix_ms,
        )
        .await
    }
}

/// A bare Workdir spec for an ephemeral sub-run sandbox (judge / delegate / compact /
/// skill fork): scoped to the thread, no staged resource mounts, host-shared network.
pub(crate) fn agent_run_sandbox_spec(thread: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: thread.to_string(),
        isolation: pc::IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: OUTPUTS_PATH.to_string(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

/// A thread's staged neutral mount requirements plus the prompt fragments derived
/// from the same effective Session inputs.
#[derive(Default, Clone)]
pub(crate) struct StagedResources {
    pub mounts: Vec<pc::MountRequirement>,
    pub prompts: Vec<String>,
    /// Resource-domain liveness checks repeated at each Session operation. These
    /// carry only Workspace-owned resource identity and frozen config versions;
    /// authorization was completed before staging.
    pub binding_checks: Vec<ResourceBindingCheck>,
    /// Mutable Repository inputs, realized after the environment is created (not a
    /// byte mount). The plan is secret-free; its transport credential is transient.
    pub repositories: Vec<RepositoryActivation>,
}

#[derive(Clone)]
pub(crate) enum ResourceBindingCheck {
    File {
        file_id: String,
    },
    MemoryStore {
        memory_store_id: String,
        config_version: awaken_protocol_managed::resource_plane::ConfigVersion,
    },
    Repository {
        repository_id: String,
        config_version: awaken_protocol_managed::resource_plane::ConfigVersion,
    },
}

/// Runtime-only activation material for one already-resolved Repository config.
/// The plan is the neutral environment port; the credential is materialized at the
/// injection seam, used only for a transport operation, and never persisted in the
/// plan, origin URL, Session manifest, or sandbox.
#[derive(Clone)]
pub(crate) struct RepositoryActivation {
    pub plan: pc::RepositoryRealizationPlan,
    pub credential: Option<pc::RepositoryHttpBasicCredential>,
}

impl SharedHost {
    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools plus the session's
    /// staged resource mounts (ADR-0038), each realized read-only under `.mnt/`.
    pub(crate) fn sandbox_spec(&self, thread: &str) -> pc::SandboxSpec {
        let mounts = self.thread_session_mounts(thread);
        // Egress denial is a Workdir-tier bwrap convenience (not admission-gated
        // network isolation, which this tier cannot enforce), so it rides `extra`.
        let extra = self
            .session_slots
            .read(thread, |slot| {
                slot.environment_projection
                    .as_ref()
                    .is_some_and(|environment| environment.network.is_restricted())
            })
            .unwrap_or(false)
            .then(|| serde_json::json!({ "deny_egress": true }));
        let projected_network = self
            .session_slots
            .read(thread, |slot| {
                slot.environment_projection
                    .as_ref()
                    .map(|environment| environment.network.clone())
            })
            .flatten()
            .unwrap_or(pc::NetworkPolicy::Unrestricted);
        let packages = self
            .session_slots
            .read(thread, |slot| {
                slot.environment_projection
                    .as_ref()
                    .map(|environment| environment.packages.clone())
            })
            .flatten()
            .unwrap_or_default();
        // Workdir can enforce the same deny-all intent only for spawned tools via
        // its `deny_egress` wrapper; it must not advertise an OS network-isolation
        // requirement that its provider deliberately does not claim. Stronger
        // providers retain the exact frozen policy for admission and enforcement.
        let network = if projected_network.is_restricted()
            && !self.session_provider.capabilities().network_isolation
        {
            pc::NetworkPolicy::Unrestricted
        } else {
            projected_network
        };
        let base = pc::SandboxSpec {
            scope: thread.to_string(),
            isolation: pc::IsolationClass::Workdir,
            mounts,
            env: self.thread_session_env(thread),
            packages,
            network,
            outputs_path: OUTPUTS_PATH.to_string(),
            limits: pc::ResourceLimits::default(),
            lease_ttl_secs: None,
            extra,
        };
        // Apply only the network-free Sandbox requirement from the same frozen
        // Environment projection ACP consumes. Reachability remains the distinct
        // `network` fact above; retained sandbox.network fields were discarded.
        self.session_slots
            .read(thread, |slot| {
                slot.environment_projection
                    .as_ref()
                    .and_then(|environment| environment.sandbox.clone())
            })
            .flatten()
            .map_or(base.clone(), |sandbox| sandbox.apply(base))
    }

    /// Stage a thread's resources (mounts + prompt fragments); consumed by
    /// `sandbox_spec` and injected into the run's system prompt. From `prepare_session`.
    /// REPLACES the thread's set (correct at create time, before any first turn).
    pub(crate) fn register_thread_resources(&self, thread: &str, staged: StagedResources) {
        self.session_slots
            .update(thread, |slot| slot.resources = staged);
    }

    pub(crate) fn register_thread_resource_manifest(
        &self,
        thread: &str,
        manifest: awaken_protocol_managed::SessionResourceManifest,
    ) {
        self.session_slots
            .update(thread, |slot| slot.manifest = Some(manifest));
    }

    pub(crate) fn thread_resource_manifest(
        &self,
        thread: &str,
    ) -> Option<awaken_protocol_managed::SessionResourceManifest> {
        self.session_slots
            .read(thread, |slot| slot.manifest.clone())
            .flatten()
    }

    pub(crate) fn thread_resources_snapshot(&self, thread: &str) -> StagedResources {
        self.session_slots
            .read(thread, |slot| slot.resources.clone())
            .unwrap_or_default()
    }

    /// The Repository activations queued for `thread` (test-only observability: a
    /// working tree is realized through its port, not as a byte mount, so it is absent
    /// from `sandbox_spec`).
    #[cfg(test)]
    pub(crate) fn thread_repository_activations(&self, thread: &str) -> Vec<RepositoryActivation> {
        self.session_slots
            .read(thread, |slot| slot.resources.repositories.clone())
            .unwrap_or_default()
    }

    /// The content-addressed blob store (Files API, file-resource mounts, artifacts).
    pub fn file_store(&self) -> Arc<dyn FileStore> {
        self.file_store.clone()
    }

    /// The sole durable logical-file catalog. Public Files identities resolve
    /// through this catalog before their private content-addressed blob is read.
    pub fn file_catalog(&self) -> Arc<dyn FileCatalog> {
        self.file_catalog.clone()
    }

    pub async fn file_record(
        &self,
        workspace: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        self.file_catalog
            .get_file(workspace, file_id, false)
            .await
            .map_err(file_catalog_error)
    }

    pub async fn list_file_records(
        &self,
        workspace: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        self.file_catalog
            .list_files(workspace, scope_id)
            .await
            .map_err(file_catalog_error)
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_file_record(
        &self,
        workspace: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        downloadable: bool,
        scope_id: Option<String>,
        logical_path: Option<String>,
        harvest_key: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        if let Some(key) = harvest_key.as_deref()
            && let Some(existing) = self
                .file_catalog
                .list_files(workspace, scope_id.as_deref())
                .await
                .map_err(file_catalog_error)?
                .into_iter()
                .find(|record| record.harvest_key.as_deref() == Some(key))
        {
            return Ok(existing);
        }
        let active = self
            .file_catalog
            .active_size_bytes(workspace)
            .await
            .map_err(file_catalog_error)?;
        validate_file_capacity(bytes.len() as u64, active)?;
        let blob_id = self
            .file_store
            .put(bytes)
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        let candidate = FileRecord {
            id: format!("file_{}", uuid::Uuid::new_v4().simple()),
            workspace_id: workspace.to_string(),
            blob_id,
            filename,
            mime_type,
            size_bytes: bytes.len() as u64,
            created_at: awaken_protocol_managed::cron::to_rfc3339(now_unix_ms()),
            downloadable,
            scope_id,
            logical_path,
            harvest_key,
            deleted: false,
        };
        let candidate_reference = logical_file_reference(&candidate);
        self.required_resource_lifecycle()?
            .add_reference(candidate_reference.clone())
            .await?;
        let outcome = match self.file_catalog.create_file(candidate.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = self
                    .required_resource_lifecycle()?
                    .remove_reference(&candidate_reference)
                    .await;
                return Err(file_catalog_error(error));
            }
        };
        match outcome {
            CreateFileRecordOutcome::Inserted(record) => Ok(record),
            CreateFileRecordOutcome::Existing(record) => {
                self.required_resource_lifecycle()?
                    .remove_reference(&candidate_reference)
                    .await?;
                self.required_resource_lifecycle()?
                    .add_reference(logical_file_reference(&record))
                    .await?;
                Ok(record)
            }
        }
    }

    pub async fn create_uploaded_file(
        &self,
        workspace: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create_file_record(
            workspace, filename, mime_type, bytes, false, None, None, None,
        )
        .await
    }

    pub async fn file_bytes(
        &self,
        workspace: &str,
        file_id: &str,
    ) -> Result<Option<(FileRecord, Vec<u8>)>, ResourcePurgeError> {
        let Some(record) = self.file_record(workspace, file_id).await? else {
            return Ok(None);
        };
        let bytes = self
            .file_store
            .get(&record.blob_id)
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?
            .ok_or_else(|| {
                ResourcePurgeError::Storage(format!(
                    "file `{file_id}` references missing blob `{}`",
                    record.blob_id
                ))
            })?;
        Ok(Some((record, bytes)))
    }

    pub async fn delete_file_record(
        &self,
        workspace: &str,
        file_id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        let Some(record) = self.file_record(workspace, file_id).await? else {
            return Ok(None);
        };
        let target = ResourceTarget::new(workspace, ResourceKind::File, &record.blob_id);
        let intent = ResourcePurgeIntent::new(
            format!("purge:File:{workspace}:{file_id}"),
            format!("file-delete:{workspace}:{file_id}"),
            target,
            None,
            requested_at_unix_ms,
            requested_at_unix_ms,
        )?;
        self.required_resource_lifecycle()?.put(intent).await?;
        let deleted = self
            .file_catalog
            .mark_file_deleted(workspace, file_id)
            .await
            .map_err(file_catalog_error)?;
        if deleted.is_some() {
            self.required_resource_lifecycle()?
                .remove_reference(&logical_file_reference(&record))
                .await?;
        }
        Ok(deleted)
    }

    pub(crate) fn required_resource_lifecycle(
        &self,
    ) -> Result<
        &Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
        ResourcePurgeError,
    > {
        self.resource_lifecycle.as_ref().ok_or_else(|| {
            ResourcePurgeError::Storage(
                "resource lifecycle repository is not configured by the composition root".into(),
            )
        })
    }

    pub async fn file_has_any_reference(&self, id: &str) -> Result<bool, ResourcePurgeError> {
        Ok(!self
            .required_resource_lifecycle()?
            .references_for_resource(ResourceKind::File, id)
            .await?
            .is_empty())
    }

    /// Persist physical cleanup work after the caller has committed logical deny.
    pub async fn request_resource_purge(
        &self,
        target: ResourceTarget,
        config_version: Option<u64>,
        requested_at_unix_ms: u64,
        not_before_unix_ms: u64,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        let key = format!(
            "{}:{:?}:{}:{:?}",
            target.workspace_id, target.kind, target.resource_id, config_version
        );
        let intent = ResourcePurgeIntent::new(
            format!("purge:{:?}:{key}", target.kind),
            key,
            target,
            config_version,
            requested_at_unix_ms,
            not_before_unix_ms,
        )?;
        self.required_resource_lifecycle()?.put(intent).await
    }

    pub(crate) async fn replace_session_references(
        &self,
        workspace: &str,
        thread: &str,
        resources: &awaken_protocol_managed::ResolvedSessionResources,
    ) -> Result<(), ResourcePurgeError> {
        use awaken_protocol_managed::ResolvedInputSource;

        let reference = |target| ResourceReferenceRecord {
            target,
            reference: ResourceReference {
                kind: ResourceReferenceKind::SessionBinding,
                reference_id: thread.to_string(),
            },
        };
        let mut records = Vec::with_capacity(
            resources.inputs.len() + resources.skills.as_ref().map_or(0, Vec::len),
        );
        for input in &resources.inputs {
            let target = match &input.source {
                ResolvedInputSource::File { file_id } => ResourceTarget::new(
                    workspace,
                    ResourceKind::File,
                    self.file_catalog
                        .get_file(workspace, file_id.as_str(), false)
                        .await
                        .map_err(file_catalog_error)?
                        .ok_or_else(|| {
                            ResourcePurgeError::Invalid(format!(
                                "file `{file_id}` was not found in this Workspace"
                            ))
                        })?
                        .blob_id,
                ),
                ResolvedInputSource::MemoryStore {
                    memory_store_id, ..
                } => ResourceTarget::new(
                    workspace,
                    ResourceKind::MemoryStore,
                    memory_store_id.as_str(),
                ),
                ResolvedInputSource::Repository { repository_id, .. } => {
                    ResourceTarget::new(workspace, ResourceKind::Repository, repository_id.as_str())
                }
            };
            records.push(reference(target));
        }
        if let Some(skills) = &resources.skills {
            records.extend(
                skills
                    .iter()
                    .filter(|skill| skill.kind == awaken_agent_contract::AgentSkillKind::Custom)
                    .map(|skill| {
                        reference(ResourceTarget::new(
                            workspace,
                            ResourceKind::Skill,
                            &skill.skill_id,
                        ))
                    }),
            );
        }
        if records.is_empty() && self.resource_lifecycle.is_none() {
            return Ok(());
        }
        self.required_resource_lifecycle()?
            .replace_references(ResourceReferenceKind::SessionBinding, thread, records)
            .await
    }

    pub(crate) async fn clear_session_references(
        &self,
        thread: &str,
    ) -> Result<(), ResourcePurgeError> {
        match &self.resource_lifecycle {
            Some(repository) => {
                repository
                    .replace_references(ResourceReferenceKind::SessionBinding, thread, Vec::new())
                    .await
            }
            None => Ok(()),
        }
    }

    /// Realize a thread's resolved Repository inputs into its freshly-created
    /// environment. The narrow port receives only a secret-free plan and an ephemeral
    /// transport credential after authorization/config resolution. A failure aborts
    /// activation so the Session cannot run believing a working tree exists.
    pub(crate) async fn realize_thread_repositories(
        &self,
        thread: &str,
        realizer: &dyn pc::RepositoryRealizer,
    ) -> Result<(), crate::host::HostError> {
        let repositories = self
            .session_slots
            .read(thread, |slot| slot.resources.repositories.clone())
            .unwrap_or_default();
        for repository in repositories {
            realizer
                .realize_repository(&repository.plan, repository.credential.as_ref())
                .await
                .map_err(|e| crate::host::HostError::internal(e.to_string()))?;
        }
        Ok(())
    }

    /// Publish a thread's Agent-authored commits through the Repository realizer.
    /// The host never fabricates a commit or resolves another config; it only applies
    /// the already-selected activation and its ephemeral transport credential.
    ///
    /// A repo whose remote ops the agent owns through an injected GitHub MCP server (the
    /// Managed Agents model — branch/commit/push/PR via MCP tools) is SKIPPED here: pushing
    /// host-side too would double-write or conflict with the agent's own pushes. Host-push
    /// remains only the fallback for a repo with no GitHub MCP (e.g. a non-MCP CLI). A no-op
    /// for a thread with no repos, no live env, or nothing the agent committed. Best-effort.
    pub async fn publish_thread_repositories(&self, thread: &str) {
        let env = self.session_environment(thread).await;
        let repositories = self
            .session_slots
            .read(thread, |slot| slot.resources.repositories.clone())
            .unwrap_or_default();
        let Some(env) = env else {
            return;
        };
        for repository in repositories {
            if repository.plan.access == pc::MountAccess::ReadOnly {
                continue;
            }
            let _ = pc::RepositoryRealizer::publish_repository(
                env.as_ref(),
                &repository.plan,
                repository.credential.as_ref(),
            )
            .await;
        }
    }

    /// Harvest a thread's run-authored skills into the durable catalog (ADR-0036 D6/D8,
    /// scan the workspace skill dir for skills the
    /// agent authored this run — the self-authoring loop a Hermes-style agent runs — and
    /// persist each under its id, so a skill written in this session is delivered to the
    /// next one that opens against the same catalog. A no-op for a thread with no live
    /// environment or a host with no durable skill store (nothing to persist into).
    /// Idempotent: a re-scanned delivered skill puts identical bytes back under the same id.
    pub async fn harvest_thread_skills(&self, thread: &str) {
        if !self.skills.has_store() {
            return;
        }
        let env = self.session_environment(thread).await;
        let Some(env) = env else {
            return;
        };
        let workspace = self.thread_workspace(thread);
        self.persist_authored_skills(&workspace, env.as_ref()).await;
    }

    /// Scan a live environment's workspace skill dir and persist each authored skill to the
    /// durable catalog. Split from [`harvest_thread_skills`](Self::harvest_thread_skills) so
    /// the scan→store path is testable with a real sandbox, without a full `SessionCtx`.
    async fn persist_authored_skills(
        &self,
        workspace: &str,
        env: &crate::session_environment::SessionEnvironment,
    ) {
        for skill in env.scan_skill_dir(crate::skills::DEFAULT_SKILLS_SUBDIR) {
            self.skills
                .persist_authored(workspace, &skill.id, &skill.content)
                .await;
        }
    }

    /// Persist every Agent-authored output before the environment can be disposed.
    /// The `(Session, logical path, content)` harvest key makes retries idempotent;
    /// Files API reads only this durable catalog and never scan the Sandbox.
    pub async fn harvest_thread_artifacts(
        &self,
        thread: &str,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        let env = self.session_environment(thread).await;
        let Some(env) = env else {
            return Ok(Vec::new());
        };
        let artifacts = env
            .artifacts()
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        let workspace = self.thread_workspace(thread);
        let mut out = Vec::new();
        for artifact in artifacts {
            let bytes = env
                .read_artifact(&artifact.id)
                .await
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            let content_id = awaken_file_store::content_id(&bytes);
            if content_id != artifact.content_hash || content_id != artifact.id {
                return Err(ResourcePurgeError::Storage(format!(
                    "artifact `{}` changed during harvest",
                    artifact.path
                )));
            }
            let logical_path = artifact
                .path
                .strip_prefix("/mnt/session/outputs/")
                .or_else(|| artifact.path.strip_prefix("/outputs/"))
                .unwrap_or(artifact.path.trim_start_matches('/'))
                .to_string();
            let mime_type = mime_type_for_path(&logical_path).to_string();
            let record = self
                .create_file_record(
                    &workspace,
                    logical_path.clone(),
                    mime_type,
                    &bytes,
                    true,
                    Some(thread.to_string()),
                    Some(logical_path.clone()),
                    Some(format!("{thread}\0{logical_path}\0{content_id}")),
                )
                .await?;
            out.push(record);
        }
        Ok(out)
    }

    /// The registered built-in tools advertised on a managed session's agent object:
    /// each hand-tool id and whether its calls require confirmation. Folded into the
    /// public `agent_toolset` by the adapter. Deterministic from host config.
    pub fn builtin_tools(&self) -> Vec<(String, bool)> {
        crate::config::builtin_hand_tools()
    }

    /// The client-executed (custom) tools advertised on a managed session: their
    /// descriptors, so the adapter can shape each as a `custom` tool definition.
    pub fn custom_tools(&self) -> Vec<ToolDescriptor> {
        self.client_tools
            .iter()
            .map(|id| crate::config::client_tool_descriptor(id))
            .collect()
    }

    /// The default Agent's published delegation targets in one Workspace.
    pub fn delegate_ids_in(&self, workspace: &str) -> Vec<String> {
        self.agent_publications
            .as_ref()
            .and_then(|source| {
                source.current(
                    workspace,
                    &awaken_runtime_contract::snapshot::AgentId("assistant".into()),
                )
            })
            .map(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .delegate_ids
                    .into_iter()
                    .map(|id| id.0)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Gap-1 coverage: the resource-staging registry (`register`/`merge`/`remove`),
/// `sandbox_spec` projection, the `realize_thread_repositories` fail-closed contract,
/// and the reverse-channel no-ops when a thread has no live environment. These
/// exercise the host-plane provisioning bookkeeping directly; the wired
/// memory/repo write-back happy paths run in `host::tests` through a real session.
#[cfg(test)]
mod provisioning_registry_tests {
    use super::*;
    use crate::host::SharedHost;
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
    use awaken_sandbox_local::LocalProvider;

    /// The logical path a staged resource realizes under, recovered from a projected
    /// pc mount (`.mnt/<logical>`) so the registry assertions stay resource-oriented.
    fn logical_of(m: &pc::MountRequirement) -> &str {
        m.mount_path.strip_prefix(".mnt/").unwrap_or(&m.mount_path)
    }

    /// Whether a projected Workdir spec denies egress (carried on the opaque `extra`).
    fn denies(spec: &pc::SandboxSpec) -> bool {
        spec.extra
            .as_ref()
            .and_then(|v| v.get("deny_egress"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    struct NoLlm;
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            unreachable!("provisioning bookkeeping never calls the model")
        }
    }

    fn host() -> SharedHost {
        SharedHost::new(Arc::new(NoLlm), "test")
    }

    #[test]
    fn managed_file_capacity_decision_boundaries_are_exact() {
        // Causes C1=file bytes at/over 500 MiB and C2=Workspace active + file at/over
        // 500 GiB. Effects: R1/R2 exact boundaries pass; R3/R4 one byte over fails.
        assert!(validate_file_capacity(MAX_MANAGED_FILE_SIZE_BYTES, 0).is_ok());
        assert!(validate_file_capacity(MAX_MANAGED_FILE_SIZE_BYTES + 1, 0).is_err());
        assert!(validate_file_capacity(1, MAX_WORKSPACE_FILE_BYTES - 1).is_ok());
        assert!(validate_file_capacity(1, MAX_WORKSPACE_FILE_BYTES).is_err());
    }

    /// A resource mount realized read-only under `.mnt/<logical>`.
    fn resource_mount(logical: &str) -> pc::MountRequirement {
        pc::MountRequirement {
            mount_id: format!("id-{logical}"),
            source: pc::MountSource::InlineBytes {
                contents: format!("content of {logical}").into_bytes(),
                content_hash: None,
            },
            mount_path: format!(".mnt/{logical}"),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }
    }

    fn repository_activation(logical: &str) -> RepositoryActivation {
        RepositoryActivation {
            plan: pc::RepositoryRealizationPlan {
                repository_id: format!("id-{logical}"),
                mount_path: logical.to_string(),
                remote_url: "https://example.invalid/x.git".to_string(),
                initial_branch: None,
                initial_commit: None,
                access: pc::MountAccess::ReadWrite,
            },
            credential: None,
        }
    }

    #[test]
    fn register_replaces_the_threads_staged_set() {
        let host = host();
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("a.md"), resource_mount("b.md")],
                prompts: vec!["first".into()],
                binding_checks: Vec::new(),
                repositories: vec![repository_activation("repo-a")],
            },
        );
        // A second register REPLACES (correct at create time, before any first turn).
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("c.md")],
                prompts: vec!["second".into()],
                binding_checks: Vec::new(),
                repositories: Vec::new(),
            },
        );

        assert_eq!(host.sandbox_spec("t").mounts.len(), 1, "old mounts dropped");
        assert_eq!(host.thread_session_prompts("t"), vec!["second".to_string()]);
        assert!(
            host.thread_repository_activations("t").is_empty(),
            "old repository activation dropped"
        );
    }

    #[test]
    fn sandbox_spec_carries_deny_egress_and_the_staged_mounts() {
        // Cause graph: a frozen restriction plus a provider without strict network
        // isolation selects the existing Workdir wrapper, not an unsupported
        // admission requirement.
        // | Rule | restriction | strict provider | network | deny wrapper |
        // | W1 | absent | no | unrestricted | no |
        // | W2 | none | no | unrestricted | yes |
        let host = host();
        // No registration and no egress: shared network, no mounts.
        let bare = host.sandbox_spec("t");
        assert!(!denies(&bare));
        assert!(bare.mounts.is_empty());

        host.install_environment_projection(
            "t",
            &awaken_protocol_managed::EnvironmentSnapshot {
                environment_id: "environment".into(),
                revision: awaken_protocol_managed::EnvironmentRevision(1),
                config_fingerprint: awaken_protocol_managed::EnvironmentFingerprint(
                    "environment-1".into(),
                ),
                sandbox: serde_json::json!({}),
                packages: Default::default(),
                network: awaken_protocol_managed::SessionNetworkPolicy::None,
                credential_realization:
                    awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
            },
        )
        .expect("freeze test Environment");
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("notes.md")],
                ..Default::default()
            },
        );
        let spec = host.sandbox_spec("t");
        assert!(denies(&spec), "the thread's deny-egress policy is carried");
        assert_eq!(
            spec.network,
            pc::NetworkPolicy::Unrestricted,
            "Workdir does not claim strict network isolation in admission"
        );
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(logical_of(&spec.mounts[0]), "notes.md");
    }

    #[tokio::test]
    async fn realize_thread_repositories_fails_closed_on_an_unsafe_path() {
        // A jail-escaping logical path is rejected by `LocalSandbox::provision_repo`
        // BEFORE any git runs (deterministic, no git binary needed). The fail-closed
        // contract: that SandboxError surfaces as a HostError so a session never
        // starts believing a repo mounted when it did not.
        let tmp = tempfile::tempdir().unwrap();
        let env = crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(tmp.path())
                .create_sandbox(&agent_run_sandbox_spec("s"))
                .await
                .unwrap(),
        );
        let host = host();
        host.register_thread_resources(
            "t",
            StagedResources {
                repositories: vec![RepositoryActivation {
                    plan: pc::RepositoryRealizationPlan {
                        repository_id: "repo-escape".into(),
                        mount_path: "../escape".into(),
                        remote_url: "https://example.invalid/x.git".into(),
                        initial_branch: None,
                        initial_commit: None,
                        access: pc::MountAccess::ReadWrite,
                    },
                    credential: None,
                }],
                ..Default::default()
            },
        );
        let err = host.realize_thread_repositories("t", &env).await;
        assert!(
            err.is_err(),
            "an unsafe repo mount must abort session start"
        );
    }

    #[tokio::test]
    async fn reverse_channels_are_safe_noops_without_a_live_session() {
        let host = host();
        // Stage a repo, but never create a session for the thread: reverse channels
        // must early-return (no live env), not panic. Memory write-through is owned
        // by the MemoryMount guard and therefore has no Host-side reverse channel.
        host.register_thread_resources(
            "t",
            StagedResources {
                repositories: vec![repository_activation("r")],
                ..Default::default()
            },
        );
        host.publish_thread_repositories("t").await; // no env → no publish
        host.harvest_thread_skills("t").await; // no env / no store → no persist
        assert!(host.harvest_thread_artifacts("t").await.unwrap().is_empty());
        assert!(
            host.harvest_thread_artifacts("never-seen")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn terminal_release_harvests_outputs_idempotently_before_sandbox_disposal() {
        use awaken_protocol_managed::SessionRuntime;
        // Cause/effect decision table:
        // R1 output present + live Sandbox => harvest creates one scoped File.
        // R2 identical retry => same File id, no duplicate manifest row/reference.
        // R3 terminal release => Sandbox gone while File metadata/bytes remain.
        let storage = tempfile::tempdir().unwrap();
        let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
        host.register_thread_workspace("session-artifacts", "workspace-a");
        let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(storage.path())
                .create_sandbox(&agent_run_sandbox_spec("session-artifacts"))
                .await
                .unwrap(),
        ));
        host.session_slots.update("session-artifacts", |slot| {
            slot.environment = Some(environment.clone())
        });
        let output = storage
            .path()
            .join("session-artifacts")
            .join("outputs")
            .join("report.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, b"durable report").unwrap();

        let first = host
            .harvest_thread_artifacts("session-artifacts")
            .await
            .unwrap();
        let retry = host
            .harvest_thread_artifacts("session-artifacts")
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(retry[0].id, first[0].id);
        assert!(first[0].id.starts_with("file_"));
        assert!(first[0].downloadable);

        crate::ManagedHost::new(host.clone())
            .end_session("session-artifacts")
            .await
            .unwrap();
        assert!(
            host.session_environment("session-artifacts")
                .await
                .is_none()
        );
        let records = host
            .list_file_records("workspace-a", Some("session-artifacts"))
            .await
            .unwrap();
        assert_eq!(records.len(), 1, "terminal retry remains idempotent");
        assert_eq!(
            host.file_bytes("workspace-a", &records[0].id)
                .await
                .unwrap()
                .unwrap()
                .1,
            b"durable report"
        );
    }

    struct FailingFileCatalog;

    #[async_trait::async_trait]
    impl FileCatalog for FailingFileCatalog {
        async fn create_file(
            &self,
            _record: FileRecord,
        ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
            Err(FileCatalogError::Storage("injected catalog failure".into()))
        }

        async fn get_file(
            &self,
            _workspace_id: &str,
            _file_id: &str,
            _include_deleted: bool,
        ) -> Result<Option<FileRecord>, FileCatalogError> {
            Ok(None)
        }

        async fn list_files(
            &self,
            _workspace_id: &str,
            _scope_id: Option<&str>,
        ) -> Result<Vec<FileRecord>, FileCatalogError> {
            Ok(Vec::new())
        }

        async fn mark_file_deleted(
            &self,
            _workspace_id: &str,
            _file_id: &str,
        ) -> Result<Option<FileRecord>, FileCatalogError> {
            Ok(None)
        }

        async fn active_size_bytes(&self, _workspace_id: &str) -> Result<u64, FileCatalogError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn terminal_harvest_failure_preserves_the_sandbox_for_retry() {
        use awaken_protocol_managed::SessionRuntime;

        // Rule R4: output present + durable catalog write fails → end_session
        // fails and the environment/output remain available; disposal is forbidden.
        let storage = tempfile::tempdir().unwrap();
        let mut raw_host = SharedHost::new(Arc::new(NoLlm), "test");
        raw_host.file_catalog = Arc::new(FailingFileCatalog);
        let host = Arc::new(raw_host);
        let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(storage.path())
                .create_sandbox(&agent_run_sandbox_spec("session-harvest-failure"))
                .await
                .unwrap(),
        ));
        host.session_slots
            .update("session-harvest-failure", |slot| {
                slot.environment = Some(environment)
            });
        let output = storage
            .path()
            .join("session-harvest-failure/outputs/report.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, b"retry me").unwrap();

        let error = crate::ManagedHost::new(host.clone())
            .end_session("session-harvest-failure")
            .await
            .unwrap_err();
        assert!(error.message.contains("injected catalog failure"));
        assert!(
            host.session_environment("session-harvest-failure")
                .await
                .is_some()
        );
        assert_eq!(std::fs::read(output).unwrap(), b"retry me");
    }

    #[tokio::test]
    async fn harvest_persists_an_agent_authored_skill_to_the_durable_catalog() {
        // Hermes-style self-authoring (ADR-0036 D6/D8): a skill the agent writes under the
        // workspace this run must be harvested into the durable catalog so the next session
        // delivers it — the skill analogue of memory write-back.
        let dir = std::env::temp_dir().join(format!("awaken-skillharvest-{}", std::process::id()));
        let host = SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store"));

        // A real sandbox env with a skill authored under the workspace `skills/` dir.
        let base = dir.join("sbx");
        let env = crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(&base)
                .create_sandbox(&agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        );
        let skill_dir = base.join("t").join("skills").join("notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: authored this run\n---\nremember to hydrate",
        )
        .unwrap();

        // The catalog is empty until the run-authored skill is harvested; after harvest it
        // holds the skill, addressable for delivery to the next session.
        assert!(
            host.skills
                .definitions(host.local_workspace())
                .await
                .unwrap()
                .is_empty()
        );
        host.persist_authored_skills(host.local_workspace(), &env)
            .await;
        let ids = host
            .skills
            .definitions(host.local_workspace())
            .await
            .unwrap()
            .into_iter()
            .map(|definition| definition.id)
            .collect::<Vec<_>>();
        assert!(
            ids.iter().any(|id| id.as_str().contains("notes")),
            "the authored skill must be persisted to the durable catalog: {ids:?}"
        );

        env.dispose().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_durable_skill_is_advertised_by_a_resolvable_catalog_id() {
        // The official worker reads `agent.skills[].skill_id` then downloads it — so the
        // advertised id must be a tagged catalog id the `/v1/skills` read paths resolve,
        // never the skill's name (which used to 404). This pins that round-trip.
        let dir = std::env::temp_dir().join(format!("awaken-skillid-{}", std::process::id()));
        let host = SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store"));
        host.skills
            .persist_authored(
                host.local_workspace(),
                "Greeter",
                "---\nname: Greeter\ndescription: hi\n---\nsay hi",
            )
            .await;

        let cid = "Greeter".to_string();
        let advertised = host.skills.ids_in(host.local_workspace());
        assert!(
            advertised.contains(&cid),
            "advertisement {advertised:?} must offer the stable resource id {cid}"
        );

        let version = host
            .skills
            .cache_snapshot_in(host.local_workspace())
            .into_iter()
            .find(|version| version.skill_id.as_str() == cid)
            .expect("the advertised resource id must resolve to the Skill version");
        assert!(version.skill_md().unwrap().ends_with(b"say hi"));
        assert!(
            host.skills
                .cache_snapshot_in(host.local_workspace())
                .into_iter()
                .find(|version| version.skill_id.as_str() == "skill_deadbeefdeadbeef")
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
