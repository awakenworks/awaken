//! `awaken-runtime-host` — the managed-agents SERVICE layer.
//!
//! It owns the protocol-neutral [`SharedHost`] plus two adapters: [`ManagedHost`]
//! for Managed Agents and [`RunApplicationHost`] for AI SDK / AG-UI / A2A. Both
//! share one host, so a run may resume or be observed through another protocol.
//!
//! The product process (`awaken-coordinator`) exposes these through routers;
//! this crate owns the Runtime services behind its config, files, memory-store,
//! and durable-operation HTTP surfaces.

mod acp_backend;
mod acp_capability_probe;
mod acp_provision;
mod acp_serve;
mod acp_tool_export;
mod agent_catalog;
mod agent_runner;
mod application;
mod authority;
mod background;
mod cache_volume;
mod capabilities;
mod commit_ingest;
mod compact;
mod config;
mod container_environment;
pub use container_environment::{
    ContainerEnvironmentComponents, build_container_environment, package_image_provisioner,
};
mod delegate;
mod deployment_config;
mod durable_operations;
mod environment_continuation;
mod host;
mod hub;
mod inference_routing;
mod judge;
mod lazy_sandbox;
mod live_inbox;
mod managed_adapter_error;
mod managed_model_capability;
mod managed_resource_projection;
mod mcp;
mod mcp_relay;
mod memory;
mod memory_stores;
mod no_model;
mod outcome_controller;
mod provisioning;
mod redact;
mod run_application_host;
mod run_exec;
mod sandbox_source;
mod session_environment;
mod session_slot;
pub use session_environment::HandExecutorFactory;
mod skill_catalog;
mod skills;
mod step_projection;
mod store;
#[cfg(test)]
mod test_mcp;
mod tool_output_spill;
mod unavailable_worker;
mod web_search;
mod worker_services;

use crate::session_environment::AgentSandbox as _;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_contract::live_inbox::{LiveInboxMessageId, MessageOrigin, Offer};
use awaken_session_contract::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, OutcomeIteration, OutcomeReport, Pending, RunError, SessionRuntime,
    StepOutcome, ToolPermissionDecision,
};

#[cfg(any(test, feature = "test-support"))]
pub use crate::authority::EphemeralRuntimeAuthority;
pub use crate::authority::{
    LocalCommit, LocalCommitAdapter, LocalCommitQueries, RuntimeAuthority, RuntimeAuthorityError,
};
pub use crate::host::{CommittedStepReceipt, HostError, HostErrorKind, PendingTool};
// The neutral session substrate and its resume vocabulary.
pub use crate::acp_capability_probe::SessionAcpCapabilityNegotiator;
pub use crate::acp_tool_export::{AcpToolExport, AcpToolExporter};
pub use crate::cache_volume::{CacheVolumeInitializer, CacheVolumeWarmup};
pub use crate::host::{
    AttemptExecutorDecorator, HostResume, RemoteAttemptInstallation, SharedHost,
    remote_worker_placement, self_hosted_inference_holder,
};
pub use crate::no_model::{NoModelConfiguredExecutor, UNCONFIGURED_MODEL_REF};
pub use crate::run_application_host::RunApplicationHost;
use awaken_credential_materializer::{CredentialRefreshFactory, PinnedCredentialMaterializer};
use awaken_resource_contract::{FileContentSource, RepositoryBindingVerifier};
// ACP launch projection consumes the Session environment selected by the host.
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::redact::PiiRedactor;
pub use crate::sandbox_source::{AcpLaunchRegistry, LaunchSource, resolve_sandbox_tier};
use crate::skill_catalog::skill_store_run_error;
pub use crate::skills::SkillForkPlacement;
use managed_adapter_error::{to_live_inbox_error, to_run_error};
// The config data plane (ADR-0036/slice A): the service + its router + the
// advertised-tools helper the process startup builds a config host from.
pub use crate::acp_provision::PublishedAcpLaunchResolver;
pub use crate::acp_serve::{AcpServeHost, AcpStop, AcpTurn};
pub use crate::commit_ingest::claimed_commit_service;
pub use crate::config::{
    advertised_tools, authorable_config_sections, authorable_config_sections_with_web_search,
    authorable_tools, block_text, platform_plugin_capabilities,
    platform_plugin_capabilities_with_web_search,
};
pub use crate::deployment_config::{
    AcpWorkerProfile, ContentCaptureSettings, ContentRedaction, DeploymentConfig, DispatchBackend,
    K8sNetworkPolicyEnforcement, PackageImageBuilder, SandboxSettings, SandboxTier, StoreKind,
    Wake, default_postgres_max_connections,
};
// The model-route seam (R1/R2/R5): a process startup supplies its own
// `InferenceExecutorMaterializer` to map a session's model ref to a labeled executor.
// The managed-vault OAuth seams (ADR-0043): the transport-level refresher, its
// prepared configuration, and the live MCP credential probe.
pub use crate::mcp::ExtMcpProbe;
// ── Managed Agents adapter over the shared host ─────────────────────────────

/// Mint a fresh user message from plain text (Managed `user.message` content is
/// concatenated to text before it enters the host).
fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::new(
        MessageId(awaken_runtime::fresh_process_id("usr")),
        Role::User,
        content,
    )
}

/// Map a neutral terminal state to the Managed idle `stop_reason`. `RequiresAction`
/// carries no event ids here; the projection refills them from the pending tool.
/// The Managed Agents `SessionRuntime` port implemented over the shared host.
/// Holds only an `Arc<SharedHost>` plus runtime-side materialization SPIs, so it
/// configures with any other adapter bound to the same host.
#[derive(Clone)]
pub struct ManagedHost {
    host: Arc<SharedHost>,
    credentials: Option<PinnedCredentialMaterializer>,
    credential_refresh_factory: Option<Arc<dyn CredentialRefreshFactory>>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::ResourceBindingValidator>>,
    repository_binding_verifier:
        Option<Arc<dyn RepositoryBindingVerifier<awaken_run_ingress::RunClaim>>>,
    mcp_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
}

/// One compiled projection from the frozen Session manifest. Standard mounts
/// and optional automatic-memory candidates travel together so installation
/// cannot publish one generation with bindings from another.
struct CompiledEffectiveInputs {
    staged: crate::provisioning::StagedResources,
    memory_bindings: std::collections::HashMap<String, Arc<crate::memory::BoundMemory>>,
}

/// Weak, cloneable Worker-side adapter over the same configured Managed
/// `SessionRuntime`. Durable dispatch carries only secret-free projections; this
/// object reuses the installed Resource validator and credential SPIs for
/// Resource and MCP realization without constructing a parallel vault path.
#[derive(Clone)]
pub(crate) struct DispatchSessionRuntime {
    host: std::sync::Weak<SharedHost>,
    credentials: Option<PinnedCredentialMaterializer>,
    credential_refresh_factory: Option<Arc<dyn CredentialRefreshFactory>>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::ResourceBindingValidator>>,
    repository_binding_verifier:
        Option<Arc<dyn RepositoryBindingVerifier<awaken_run_ingress::RunClaim>>>,
    mcp_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
}

impl DispatchSessionRuntime {
    fn managed(&self) -> Result<ManagedHost, RunError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RunError::internal("dispatch Session Runtime host was dropped"))?;
        Ok(ManagedHost {
            host,
            credentials: self.credentials.clone(),
            credential_refresh_factory: self.credential_refresh_factory.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            mcp_realizer: None,
        })
    }

    async fn install(
        &self,
        thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let managed = self.managed()?;
        let previous = managed.host.thread_resource_manifest(thread);
        let is_replacement = previous
            .as_ref()
            .is_some_and(|previous| previous != manifest);
        if let Some(previous) = previous.as_ref().filter(|previous| *previous != manifest) {
            // A claimed Run may advance a live Session only to a strictly newer
            // durable Resource generation. Exact replay is handled above; an
            // older generation or same-generation/different-value envelope is
            // stale or corrupt and must never replace the active projection.
            if previous.workspace_id != manifest.workspace_id
                || manifest.revision <= previous.revision
            {
                return Err(RunError::bad_request(
                    "a claimed Worker cannot replace the active Session Resource generation",
                ));
            }
        }
        if is_replacement {
            // Claimed-generation decision table: newer/different => reuse the
            // canonical live Session transition with claim-fenced remote reads
            // and without mutating the authority-side reference graph. Older,
            // same-generation/different, and cross-Workspace were fenced above.
            return managed
                .apply_session_inputs_with_context(
                    thread,
                    &manifest.workspace_id,
                    manifest.revision,
                    &manifest.resources,
                    claim,
                )
                .await;
        }
        // Re-stage even when the manifest is unchanged: immutable File bytes,
        // config-version integrity, and credential revocation are live-deny checks
        // at every claimed operation. Worker projection never mutates the
        // authority-side intrinsic Resource reference graph.
        managed
            .stage_resource_manifest(
                thread,
                &manifest.workspace_id,
                manifest.revision,
                &manifest.resources,
                claim,
            )
            .await?;
        Ok(())
    }

    async fn stage_mcp(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.stage_mcp_attachment(request).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::stage_mcp_attachment(
                    &self.managed()?,
                    request,
                )
                .await
            }
        }
    }

    async fn publish_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.publish_mcp_generation(generation).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::publish_mcp_generation(
                    &self.managed()?,
                    generation,
                )
                .await
            }
        }
    }

    async fn drain_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.drain_mcp_generation(generation).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::drain_mcp_generation(
                    &self.managed()?,
                    generation,
                )
                .await
            }
        }
    }
}

impl SharedHost {
    pub(crate) async fn install_dispatched_resources(
        &self,
        thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let preparer = self
            .dispatch_session_runtime
            .read()
            .expect("dispatch Session Runtime lock poisoned")
            .clone()
            .ok_or_else(|| {
                RunError::internal("durable resource dispatch has no Session Runtime")
            })?;
        preparer.install(thread, manifest, claim).await
    }

    fn dispatch_session_runtime(&self) -> Result<DispatchSessionRuntime, RunError> {
        self.dispatch_session_runtime
            .read()
            .expect("dispatch Session Runtime lock poisoned")
            .clone()
            .ok_or_else(|| RunError::internal("dispatch Session Runtime is not configured"))
    }

    pub(crate) async fn stage_dispatched_mcp(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        self.dispatch_session_runtime()?.stage_mcp(request).await
    }

    pub(crate) async fn publish_dispatched_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .publish_mcp(generation)
            .await
    }

    pub(crate) async fn drain_dispatched_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?.drain_mcp(generation).await
    }
}

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self {
            host,
            credentials: None,
            credential_refresh_factory: None,
            resource_validator: None,
            repository_binding_verifier: None,
            mcp_realizer: None,
        }
    }

    /// Install the fully configured Managed adapter used by durable dispatch.
    ///
    /// Call this once at the process startup after all `with_*` configuration
    /// has been applied. Construction and configuration are deliberately free
    /// of shared-host side effects, so a partially configured adapter can never
    /// become visible to a concurrently claimed Run.
    #[must_use]
    pub fn install_dispatch_session_runtime(self) -> Self {
        *self
            .host
            .dispatch_session_runtime
            .write()
            .expect("dispatch Session Runtime lock poisoned") = Some(DispatchSessionRuntime {
            host: Arc::downgrade(&self.host),
            credentials: self.credentials.clone(),
            credential_refresh_factory: self.credential_refresh_factory.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            mcp_realizer: self.mcp_realizer.clone(),
        });
        self
    }

    /// Project the committed attempt result into the Managed Session contract.
    /// Output persistence already happened at the shared attempt executor edge,
    /// before direct or durable delivery returns here.
    async fn finish_step(
        &self,
        _thread: &str,
        result: Result<CommittedStepReceipt, HostError>,
    ) -> Result<StepOutcome, RunError> {
        result
            .map_err(to_run_error)
            .and_then(crate::step_projection::settled_step)
    }

    /// Wire the live resource-invariant port used at activation and Memory use.
    /// Configuration was already selected by the Session control plane; this port
    /// only validates trusted Workspace ownership, lifecycle state, and the frozen
    /// config version. It does not make an authorization decision.
    #[must_use]
    pub fn with_resource_validator(
        mut self,
        validator: Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    ) -> Self {
        self.resource_validator = Some(validator);
        self
    }

    /// Install the Repository-specific live binding guard used by a distributed
    /// Worker without granting it Resource Catalog database access.
    #[must_use]
    pub fn with_repository_binding_verifier(
        mut self,
        verifier: Arc<dyn RepositoryBindingVerifier<awaken_run_ingress::RunClaim>>,
    ) -> Self {
        self.repository_binding_verifier = Some(verifier);
        self
    }

    /// Realize an already-resolved, secret-free manifest. The pinned Memory/
    /// Repository configuration in `inputs` remains authoritative; the per-item
    /// validation in `stage_resolved_input` checks only current ownership/state
    /// and the frozen version's integrity. No Agent binding or current config is
    /// configured here.
    async fn compile_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<CompiledEffectiveInputs, RunError> {
        let mut all = crate::provisioning::StagedResources::default();
        let mut memory_bindings = std::collections::HashMap::new();
        for input in &inputs.inputs {
            let one = self.stage_resolved_input(workspace, input, claim).await?;
            if let awaken_session_contract::ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } = &input.source
            {
                let writable = input.access == awaken_resource_contract::ResourceAccess::ReadWrite;
                // Read this exact projection before merging it. Two bindings may
                // legally reference the same store with different access, and a
                // prior mount must never become the authority for the later one.
                let materialization_reference =
                    one.mounts.iter().find_map(|mount| match &mount.source {
                        awaken_provisioning_contract::MountSource::MemoryStore {
                            store_id,
                            materialization_reference,
                            ..
                        } if store_id == memory_store_id.as_str() => {
                            materialization_reference.clone()
                        }
                        _ => None,
                    });
                if let Some(reference) = &materialization_reference {
                    // Remote Memory claim decision table: active + exact config
                    // => snapshot preflight succeeds; archived/config-changed/
                    // stale claim => fail before a resident Environment or model
                    // can reuse the prior projection. The mounter still owns the
                    // actual copy/write-back lifecycle.
                    self.host
                        .memory_repository()
                        .snapshot_heads(reference)
                        .await
                        .map_err(|error| RunError::bad_request(error.to_string()))?;
                }
                let handle = self.host.platform_memory_handle(
                    materialization_reference
                        .clone()
                        .unwrap_or_else(|| memory_store_id.to_string()),
                    writable,
                );
                let resource_validator = if materialization_reference.is_some() {
                    None
                } else {
                    Some(self.resource_validator.as_ref().ok_or_else(|| {
                        RunError::bad_request(
                            "Memory extraction requires a configured resource binding validator",
                        )
                    })?.clone())
                };
                memory_bindings.insert(
                    input.binding_id.to_string(),
                    Arc::new(self.host.memory.bind(
                        thread,
                        workspace,
                        handle,
                        resource_validator,
                        config,
                        writable,
                    )),
                );
            }
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.binding_checks.extend(one.binding_checks);
            all.repositories.extend(one.repositories);
        }

        Ok(CompiledEffectiveInputs {
            staged: all,
            memory_bindings,
        })
    }

    async fn install_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        compiled: CompiledEffectiveInputs,
    ) -> Result<(), RunError> {
        // The complete manifest replaces the prior projection. Register an empty
        // value too, so deleting the final input cannot leave a stale mount behind.
        self.host.register_thread_resources(thread, compiled.staged);
        self.host.register_thread_resource_manifest(
            thread,
            awaken_session_contract::SessionResourceManifest::at_revision(
                workspace,
                resource_revision,
                inputs.clone(),
            ),
        );
        // Standard mounts and the optional automatic-memory selection are
        // separate facts. Installing a manifest never picks a "first" store.
        self.host
            .register_thread_memory_bindings(thread, compiled.memory_bindings);
        Ok(())
    }

    async fn stage_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let compiled = self
            .compile_effective_inputs(thread, workspace, inputs, claim)
            .await?;
        self.install_effective_inputs(thread, workspace, resource_revision, inputs, compiled)
            .await
    }

    /// Install one already-resolved Session resource manifest. This is shared by
    /// managed Session creation and cold durable workers; neither path reads Agent
    /// defaults or selects a newer mutable-resource configuration.
    async fn stage_resource_manifest(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        resources: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        self.host.register_thread_workspace(thread, workspace);
        let desired = awaken_session_contract::SessionResourceManifest::at_revision(
            workspace,
            resource_revision,
            resources.clone(),
        );
        // The authority-side recovery path may first reconcile a retained or
        // pending generation and then prepare the complete Session projection.
        // Both operations carry the same canonical manifest. Avoid compiling and
        // installing it twice; claimed Workers remain excluded because every
        // claim must revalidate live Resource state even when the pin is equal.
        if claim.is_none() && self.host.thread_resource_manifest(thread).as_ref() == Some(&desired)
        {
            return Ok(());
        }
        match &resources.skills {
            Some(bindings) => {
                let versions = self
                    .host
                    .skills
                    .load_pinned(workspace, bindings, claim)
                    .await
                    .map_err(skill_store_run_error)?;
                self.host
                    .session_slots
                    .update(thread, |slot| slot.skills = Some(versions));
            }
            None => {
                // Legacy records retain the historical global-catalog behavior;
                // modern `Some` manifests always install only their exact pins.
                self.host
                    .skills
                    .reload_cache_in(workspace)
                    .await
                    .map_err(skill_store_run_error)?;
                self.host
                    .session_slots
                    .update(thread, |slot| slot.skills = None);
            }
        }
        self.stage_effective_inputs(thread, workspace, desired.revision, resources, claim)
            .await
    }

    async fn validate_thread_resource_bindings(&self, thread: &str) -> Result<(), RunError> {
        use crate::provisioning::ResourceBindingCheck;

        // This method is entered only through the SessionRuntime application
        // port. Preserve that neutral identity before dispatch so a claiming
        // Worker enters the frozen Session realization path.
        self.host
            .session_slots
            .update(thread, |slot| slot.session_dispatch = true);
        let checks = self
            .host
            .session_slots
            .read(thread, |slot| slot.resources.binding_checks.clone())
            .unwrap_or_default();
        if checks.is_empty() {
            return Ok(());
        }
        let workspace = self.host.thread_workspace(thread);
        for check in checks {
            match check {
                ResourceBindingCheck::MemoryStore {
                    memory_store_id,
                    config_version,
                } => self
                    .resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "memory resources require a configured resource binding validator",
                        )
                    })?
                    .validate_memory_binding(&workspace, &memory_store_id, config_version)
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
                ResourceBindingCheck::Repository {
                    repository_id,
                    config_version,
                    claim,
                } => self
                    .repository_binding_verifier
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "repository resources require a configured binding verifier",
                        )
                    })?
                    .verify(&workspace, &repository_id, config_version, claim.as_ref())
                    .await
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
            }
        }
        Ok(())
    }

    /// Wire runtime credential injection for the already-frozen Session bindings
    /// and Repository realization.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_credentials(
        self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.with_credential_materializer(PinnedCredentialMaterializer::new(credentials, secrets))
    }

    /// Reuse the process startup's canonical exact materializer for MCP and
    /// Repository realization instead of constructing a peer over the same stores.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: PinnedCredentialMaterializer,
    ) -> Self {
        self.credentials = Some(materializer);
        self
    }

    /// Install the credential adapter's exact OAuth refresh port. The Host
    /// retains only this factory and never receives Credential/Secret Store
    /// handles.
    #[must_use]
    pub fn with_credential_refresh_factory(
        mut self,
        factory: Arc<dyn CredentialRefreshFactory>,
    ) -> Self {
        self.credential_refresh_factory = Some(factory);
        self
    }

    /// Replace the local Host MCP realization adapter with one downstream
    /// implementation of the same exact-generation Session port. This is the
    /// sole injection seam used by durable Worker commands; desired state and
    /// credential selection remain outside the implementation.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_realizer = Some(realizer);
        self
    }

    async fn apply_session_inputs_with_context(
        &self,
        thread: &str,
        workspace_id: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        // Reuse the Session slot's canonical realization mutex. Cold active-active
        // requests may concurrently replay the same durable generation; only one
        // may compare, realize, and publish its process-local projection at a time.
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.host.register_thread_workspace(thread, workspace_id);
        let desired_manifest = awaken_session_contract::SessionResourceManifest::at_revision(
            workspace_id,
            resource_revision,
            inputs.clone(),
        );
        // Exact local replays are already converged. Claimed replays are handled
        // by `DispatchSessionRuntime::install`, which re-stages them to revalidate
        // live Resource state before entering this replacement path.
        if self.host.thread_resource_manifest(thread).as_ref() == Some(&desired_manifest) {
            return Ok(());
        }
        let old = self.host.thread_resources_snapshot(thread);
        let old_memory: Vec<_> = old
            .mounts
            .iter()
            .filter_map(|mount| match &mount.source {
                awaken_provisioning_contract::MountSource::MemoryStore { store_id, .. } => Some((
                    mount.mount_id.clone(),
                    store_id.clone(),
                    mount.mount_path.clone(),
                    mount.access,
                )),
                _ => None,
            })
            .collect();
        let desired_memory: Vec<_> = inputs
            .inputs
            .iter()
            .filter_map(|input| match &input.source {
                awaken_session_contract::ResolvedInputSource::MemoryStore {
                    memory_store_id,
                    ..
                } => Some((
                    input.binding_id.to_string(),
                    memory_store_id.to_string(),
                    format!(".mnt/{}", input.mount_path.trim_start_matches('/')),
                    match input.access {
                        awaken_resource_contract::ResourceAccess::ReadOnly => {
                            awaken_provisioning_contract::MountAccess::ReadOnly
                        }
                        awaken_resource_contract::ResourceAccess::ReadWrite => {
                            awaken_provisioning_contract::MountAccess::ReadWrite
                        }
                    },
                )),
                _ => None,
            })
            .collect();
        let live_environment = self.host.session_environment(thread).await;
        // Another cold-rehydration request can install this exact manifest while
        // the environment lookup above yields. Re-read the canonical manifest at
        // the decision boundary: equal means the concurrent replay converged;
        // unequal remains a forbidden live Memory mutation.
        let installed_manifest = self.host.thread_resource_manifest(thread);
        if live_environment.is_some() && installed_manifest.as_ref() == Some(&desired_manifest) {
            return Ok(());
        }
        // A durable sandbox binding can be adopted before this process has any
        // Resource projection. `None` therefore means cold recovery: install the
        // authority's active generation. Only an already-installed, different
        // manifest is evidence of a forbidden live Memory mutation.
        if live_environment.is_some()
            && installed_manifest.is_some()
            && old_memory != desired_memory
        {
            tracing::warn!(
                session_id = thread,
                installed_manifest = ?installed_manifest,
                old_memory = ?old_memory,
                desired_memory = ?desired_memory,
                "rejecting a live Session Memory projection change"
            );
            return Err(RunError::bad_request(
                "memory_store inputs are create-time only for a live Session",
            ));
        }
        let skill_versions = match &inputs.skills {
            Some(bindings) => Some(
                self.host
                    .skills
                    .load_pinned(workspace_id, bindings, claim)
                    .await
                    .map_err(skill_store_run_error)?,
            ),
            None => None,
        };
        let compiled = self
            .compile_effective_inputs(thread, workspace_id, inputs, claim)
            .await?;
        let new = &compiled.staged;
        if let Some(environment) = &live_environment {
            environment
                .validate_live_mount_replacement(&old.mounts, &new.mounts)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        self.host
            .harvest_thread_skills(thread)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        self.host
            .publish_thread_repositories(thread)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        if let Some(environment) = &live_environment {
            // Realize the desired live projection before committing its logical
            // manifest. Every operation is idempotent, so a failed attempt leaves
            // the prior manifest authoritative and the persisted pending generation
            // can safely retry without mistaking an unrealized mount for success.
            environment
                .remove_workspace_path(crate::skills::DELIVERED_SKILLS_SUBDIR)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
            for mount in &old.mounts {
                if !new
                    .mounts
                    .iter()
                    .any(|candidate| candidate.mount_path == mount.mount_path)
                {
                    environment
                        .remove_workspace_path(&mount.mount_path)
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
            for mount in &new.mounts {
                if !old.mounts.iter().any(|candidate| candidate == mount) {
                    environment
                        .attach_mount(mount.clone())
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
            for repository in &old.repositories {
                if !new
                    .repositories
                    .iter()
                    .any(|candidate| candidate.plan == repository.plan)
                {
                    environment
                        .remove_workspace_path(&repository.plan.mount_path)
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
            for repository in &new.repositories {
                if !old
                    .repositories
                    .iter()
                    .any(|candidate| candidate.plan == repository.plan)
                {
                    awaken_provisioning_contract::RepositoryRealizer::realize_repository(
                        environment.as_ref(),
                        &repository.plan,
                        repository.credential.as_ref(),
                    )
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
        }
        self.install_effective_inputs(thread, workspace_id, resource_revision, inputs, compiled)
            .await?;
        self.host
            .session_slots
            .update(thread, |slot| slot.skills = skill_versions);
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ManagedHost {
    fn install_session_request_context(
        &self,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<(), RunError> {
        self.host.install_session_request_context(thread, messages);
        Ok(())
    }

    fn install_environment_binding_sink(
        &self,
        sink: Arc<dyn awaken_session_contract::SessionEnvironmentBindingSink>,
    ) {
        *self
            .host
            .environment_binding_sink
            .write()
            .expect("environment binding sink lock poisoned") = Some(sink);
    }

    fn install_session_realization_lease(
        &self,
        session_id: &str,
        lease: awaken_session_contract::SessionRealizationLease,
    ) {
        self.host
            .install_session_realization_lease(session_id, lease);
    }

    fn install_expected_environment_binding(
        &self,
        session_id: &str,
        binding: Option<String>,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.host
            .install_expected_environment_binding(session_id, binding)
            .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))
    }

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.host.delegated_runs(thread).await.map_err(to_run_error)
    }

    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, RunError> {
        self.host
            .quiesce_terminal_delegations(thread)
            .await
            .map_err(to_run_error)
    }

    async fn owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.host
            .has_durable_thread(thread)
            .await
            .map_err(to_run_error)
    }

    async fn end_session(&self, thread: &str) -> Result<(), RunError> {
        let intent =
            awaken_session_contract::SessionTerminalCleanupIntent::for_thread(thread, thread);
        self.execute_terminal_cleanup(intent).await.map(|_| ())
    }

    async fn execute_terminal_cleanup(
        &self,
        intent: awaken_session_contract::SessionTerminalCleanupIntent,
    ) -> Result<awaken_session_contract::SessionTerminalCleanupReceipt, RunError> {
        // Terminal release owns every reverse operation: publish Agent-authored Repo
        // commits (when the Agent did not own publication through MCP), persist
        // run-authored Skills, then dispose. A GET /files poll is never a write edge.
        self.host
            .publish_thread_repositories(&intent.thread_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        self.host
            .harvest_thread_skills(&intent.thread_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        // Failure is terminal-release blocking: keep the Sandbox available for
        // the durable cleanup retry instead of disposing unharvested outputs.
        let artifact_receipts = self
            .host
            .harvest_thread_artifacts(&intent.thread_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        // Memory is owned by its MemoryMount guard: FUSE writes through live and
        // copy realization performs one CAS harvest during teardown.
        self.host
            .end_session(&intent.thread_id)
            .await
            .map_err(to_run_error)?;
        let receipt = awaken_session_contract::SessionTerminalCleanupReceipt::new(
            &intent,
            artifact_receipts,
            true,
            true,
            true,
        );
        receipt
            .verify(&intent)
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(receipt)
    }

    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .run(Some(agent), thread, vec![user_message(content)])
            .await;
        self.finish_step(thread, result).await
    }

    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .run_attributed(
                Some(agent),
                thread,
                vec![user_message(content)],
                data_subject_id.map(awaken_runtime_contract::DataSubjectId),
            )
            .await;
        self.finish_step(thread, result).await
    }

    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        // Same committed turn as `run`; `sink` mirrors in-flight `stream::Kind` so
        // the Managed adapter can project live `agent.message` previews.
        let result = self
            .host
            .run_streaming(Some(agent), thread, vec![user_message(content)], sink)
            .await;
        self.finish_step(thread, result).await
    }

    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .run_streaming_attributed(
                Some(agent),
                thread,
                vec![user_message(content)],
                sink,
                data_subject_id.map(awaken_runtime_contract::DataSubjectId),
            )
            .await;
        self.finish_step(thread, result).await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::ToolPermission {
                    allow: decision.allow,
                    note: decision.note,
                },
            )
            .await;
        self.finish_step(thread, result).await
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::ClientResult { content, is_error },
            )
            .await;
        self.finish_step(thread, result).await
    }

    async fn live_inbox_snapshot(&self, thread: &str) -> LiveInboxSnapshot {
        match self.host.live_inbox(thread).await {
            Some(inbox) => LiveInboxSnapshot {
                active: true,
                version: inbox.version(),
                messages: inbox
                    .list()
                    .into_iter()
                    .map(|entry| LiveInboxEntry {
                        id: entry.id.0,
                        content: entry.message.content,
                    })
                    .collect(),
            },
            None => LiveInboxSnapshot::inactive(),
        }
    }

    async fn live_inbox_queue(
        &self,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        // Queued through the wire surface — an out-of-band injection into a live
        // run, so it is tagged External (what a product maps operator steering onto).
        match inbox.offer_as(MessageOrigin::External, user_message(content)) {
            Offer::Accepted(id) => Ok(id.0),
            // The attempt closed between lookup and offer: same outcome as no
            // attempt at all.
            Offer::Closed => Err(LiveInboxError::Inactive),
        }
    }

    async fn live_inbox_remove(&self, thread: &str, id: u64) -> Result<(), LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        inbox
            .remove(LiveInboxMessageId(id))
            .map(|_| ())
            .map_err(to_live_inbox_error)
    }

    async fn live_inbox_replace(
        &self,
        thread: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        inbox
            .replace(LiveInboxMessageId(id), user_message(content))
            .map_err(to_live_inbox_error)
    }

    async fn live_inbox_reorder(
        &self,
        thread: &str,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        let order: Vec<LiveInboxMessageId> = order.into_iter().map(LiveInboxMessageId).collect();
        inbox.reorder(&order).map_err(to_live_inbox_error)
    }

    async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError> {
        self.host
            .add_system(thread, text)
            .await
            .map_err(to_run_error)
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        managed_model_capability::supports_mid_conversation_system(
            &self.host.model_for_thread(thread),
        )
    }

    async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        self.host
            .pending_tool(thread)
            .await
            .map(crate::step_projection::pending)
            .map_err(to_run_error)
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.host.interrupt(thread).await.map_err(to_run_error)
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        let report = self
            .host
            .define_outcome(thread, description, rubric, max_iterations)
            .await
            .map_err(to_run_error)?;
        Ok(OutcomeReport {
            iterations: report
                .iterations
                .into_iter()
                .map(|it| OutcomeIteration {
                    messages: it.messages,
                    outcome_id: it.outcome_id,
                    description: description.to_string(),
                    iteration: it.iteration,
                    result: it.result,
                    explanation: it.explanation,
                })
                .collect(),
        })
    }

    /// Replace only the already-selected model projection for one Session and
    /// force its next context construction to consume that exact selection.
    async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError> {
        // R5: re-stage the thread's model and evict its cached context so the next
        // turn rebuilds with the newly resolved executor (native switch is O(1); an
        // ACP thread's cached context relaunches its CLI on rebuild).
        self.host.register_thread_model(thread, model);
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }

    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<awaken_session_contract::ResolvedSkillBinding>, RunError> {
        if skills.is_empty() {
            return Ok(Vec::new());
        }
        self.host
            .skills
            .resolve(workspace_id, skills)
            .await
            .map_err(skill_store_run_error)
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        workspace_id: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.apply_session_inputs_with_context(
            thread,
            workspace_id,
            resource_revision,
            inputs,
            None,
        )
        .await
    }

    async fn prepare_session(
        &self,
        thread: &str,
        init: awaken_session_contract::SessionInit,
    ) -> Result<(), RunError> {
        // Session preparation and durable Resource reconciliation publish one
        // process-local projection. Serialize both through the existing slot
        // lifecycle so a peer request cannot observe Environment installed while
        // the exact frozen Resource manifest is still absent.
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        // A process may have opened this durable thread before its Control-frozen
        // projection arrived (for example a peer/recovery read racing Session
        // rehydration). That context was necessarily built from host defaults.
        // Projection installation is the authority transition: discard only the
        // rebuildable context while retaining any independently-owned Environment.
        // A live run cannot be rebound underneath its already-created activation.
        let active_run = self
            .host
            .session_slots
            .read(thread, |slot| {
                slot.runtime.as_ref().and_then(|context| {
                    context
                        .active_run
                        .lock()
                        .expect("active run mutex poisoned")
                        .clone()
                })
            })
            .flatten();
        if active_run.is_some() {
            return Err(RunError::internal(
                "cannot install a frozen Session projection while its Runtime is active",
            ));
        }
        self.host.session_slots.update(thread, |slot| {
            slot.runtime = None;
            slot.session_dispatch = true;
        });
        // This is the one projection lowering path shared with claimed Worker
        // replay. In particular, workspace/Agent/backend cannot drift between
        // Coordinator dispatch construction and Worker execution.
        self.host
            .project_session_init(thread, &init)
            .map_err(to_run_error)?;
        if init.environment.sandbox_provisioning
            == awaken_session_contract::SandboxProvisioning::OnToolUse
        {
            let executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor> =
                Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
                    Arc::downgrade(&self.host),
                    thread,
                ));
            self.host.session_slots.update(thread, |slot| {
                slot.deferred_executor = Some(executor);
            });
        }
        // Stage only the already-resolved manifest. Runtime never reads the Agent
        // binding repository or configures defaults again.
        self.stage_resource_manifest(
            thread,
            &init.workspace_id,
            init.resource_revision,
            &init.resources,
            None,
        )
        .await?;
        Ok(())
    }

    async fn replace_session_toolsets(
        &self,
        thread: &str,
        toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    ) -> Result<(), RunError> {
        self.host
            .session_slots
            .update(thread, |slot| slot.toolsets = Some(toolsets));
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        // Managed Session restoration is a continuity contract: the durable
        // binding must decode, belong to this Session/provider, and still name a
        // ready physical environment. Run-dispatch recovery has its own explicit
        // RebuildFromCommittedTruth policy; applying that fallback here would
        // turn corrupt or deleted Session authority into a replacement sandbox.
        let (_, _, publication) = self
            .host
            .resolve_session_publication(thread, Some(agent), None)
            .map_err(to_run_error)?;
        let provisioning = publication
            .as_ref()
            .map(|snapshot| &snapshot.resolved_spec.model_binding.provisioning)
            .unwrap_or(&awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor);
        let (adopted, rebuild) = self
            .host
            .adopt_bound_session_environment(thread, Some(binding), provisioning, false)
            .await
            .map_err(to_run_error)?;
        debug_assert!(!rebuild);
        self.host
            .ctx_for_with_sandbox(thread, Some(agent), adopted)
            .await
            .map_err(to_run_error)?;
        Ok(())
    }

    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.quiesce_environment_continuation(thread, operation, generation)
            .await
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_contract::CheckpointReceipt, RunError> {
        self.checkpoint_environment_continuation(thread, request)
            .await
    }

    async fn dispose_checkpoint_source(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        self.dispose_environment_continuation_source(thread, operation, generation, source_binding)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        agent: &str,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        self.restore_environment_continuation(agent, thread, operation, generation, checkpoint)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        _thread: &str,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        self.delete_environment_continuation_checkpoint(checkpoint)
            .await
    }

    /// Committed transcript from durable truth, so the adapter can rehydrate a
    /// session lost to a process restart and resume its awaiting run (ADR-0039).
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::Message>, RunError> {
        self.host
            .committed_messages(thread)
            .await
            .map_err(to_run_error)
    }

    async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_agent_contract::RunLifecyclePage, RunError> {
        let feed = self
            .host
            .run_lifecycle_feed(thread)
            .await
            .map_err(to_run_error)?;
        awaken_agent_contract::RunLifecycleFeed::events_after(feed.as_ref(), cursor, limit)
            .await
            .map_err(|error| RunError::internal(error.to_string()))
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::SessionUsage, RunError> {
        // Map the runtime's per-model tally onto the managed wire's session-level total
        // (the host is the context boundary; the managed crate never sees TokenUsage).
        let attributed = self.host.thread_usage(thread).await;
        let total = attributed.total();
        Ok(awaken_session_contract::SessionUsage {
            input_tokens: total.prompt_tokens,
            output_tokens: total.completion_tokens,
            cache_read_tokens: total.cache_read_tokens,
            cache_creation_tokens: total.cache_creation_tokens,
            by_model: attributed
                .by_model
                .into_iter()
                .map(|(model, usage)| {
                    (
                        model,
                        awaken_session_contract::SessionModelUsage {
                            input_tokens: usage.prompt_tokens,
                            output_tokens: usage.completion_tokens,
                            cache_read_tokens: usage.cache_read_tokens,
                            cache_creation_tokens: usage.cache_creation_tokens,
                        },
                    )
                })
                .collect(),
            active_seconds: 0,
            web_fetch_requests: 0,
            web_search_requests: 0,
        })
    }

    fn model(&self) -> String {
        self.host.model()
    }

    /// Advertise the host's provisioned surface on the created session: its built-in
    /// tools (folded into the agent toolset by the adapter), client tools, offered
    /// skills, and delegate roster. (MCP servers and file resources are not advertised
    /// — the local host wires no MCP capability and has no Files-API resource yet.)
    fn capabilities(&self) -> AgentCapabilities {
        self.capabilities_for_workspace(self.host.local_workspace())
    }

    fn capabilities_for(&self, thread: &str) -> AgentCapabilities {
        let workspace = self.host.thread_workspace(thread);
        let mut capabilities = self.capabilities_for_workspace(&workspace);
        if let Some(delegate_ids) = self.host.thread_delegate_ids(thread) {
            capabilities.delegates = delegate_ids;
        }
        capabilities
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for ManagedHost {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        use awaken_runtime_contract::{
            CredentialRealizationCapabilities, CredentialRealizationKind, PlaintextBoundary,
        };
        use std::collections::BTreeSet;

        if request.workspace_id.trim().is_empty()
            || request.generation.session_id.trim().is_empty()
            || request.realization_id.trim().is_empty()
            || request.stage_idempotency_key.trim().is_empty()
            || request.name.trim().is_empty()
            || request.target.display_target().trim().is_empty()
        {
            return Err(RunError::bad_request(
                "MCP realization request is incomplete",
            ));
        }
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        if !self
            .host
            .mcp_generation_is_authorized_at(&request.generation, now_unix_ms)
        {
            return Err(RunError::classified(
                "mcp_stale_ownership",
                "MCP realization lease has expired",
            ));
        }
        let request_fingerprint = request.fingerprint();
        if let Some(existing) = self.host.mcp_projection(&request.generation) {
            let exact_replay = existing.request.realization_id == request.realization_id
                && existing.request.stage_idempotency_key == request.stage_idempotency_key
                && existing.receipt.receipt_fingerprint == request_fingerprint;
            if exact_replay {
                if existing.state != crate::session_slot::McpProjectionState::Removed {
                    return Ok(existing.receipt);
                }
                if !self.host.forget_exact_removed_mcp_projection(&request) {
                    return Err(RunError::classified(
                        "mcp_stale_generation",
                        "MCP generation changed while its removed projection was being recovered",
                    ));
                }
            } else {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    "MCP generation is already bound to another realization",
                ));
            }
        }
        match self.host.renew_mcp_projection(&request) {
            Ok(Some(receipt)) => return Ok(receipt),
            Ok(None) => {}
            Err(error) => {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    error.to_string(),
                ));
            }
        }

        let is_acp = self
            .host
            .session_slots
            .read(&request.generation.session_id, |slot| {
                slot.backend_ref.clone()
            })
            .flatten()
            .is_some_and(|backend_ref| {
                awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref).is_acp()
            });

        let sandbox_stdio = request.target.sandbox_stdio_target().cloned();
        if sandbox_stdio.is_some()
            && (request.credential.is_some() || request.selected_plaintext_holder.is_some())
        {
            return Err(RunError::classified(
                "mcp_stdio_credential_unsupported",
                "sandbox stdio MCP credentials require an explicit secret-environment binding; HTTP bearer credentials cannot be projected into a process",
            ));
        }

        let (bearer, refresh, actual_realization_kind) = match (
            request.credential.as_ref(),
            request.selected_plaintext_holder.as_ref(),
        ) {
            (None, None) => (None, None, None),
            (Some(access), Some(holder)) => {
                if holder.boundary != PlaintextBoundary::Worker {
                    return Err(RunError::classified(
                        "mcp_holder_unsupported",
                        "MCP Runtime supports only the Worker-held relay boundary",
                    ));
                }
                match &access.usage {
                    awaken_runtime_contract::CredentialUsage::HttpHeader { name, scheme }
                        if name.eq_ignore_ascii_case("authorization")
                            && scheme
                                .as_deref()
                                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer")) => {}
                    _ => {
                        return Err(RunError::classified(
                            "mcp_credential_usage_unsupported",
                            "MCP Runtime supports only canonical Authorization Bearer usage",
                        ));
                    }
                }
                if is_acp
                    && access.policy.model_exposure
                        != awaken_runtime_contract::ModelExposurePolicy::VirtualOnly
                {
                    return Err(RunError::classified(
                        "mcp_model_exposure_forbidden",
                        "authenticated ACP MCP requires explicit VirtualOnly authorization for its generation-scoped relay capability",
                    ));
                }
                if is_acp
                    && !self
                        .host
                        .session_provider
                        .capabilities()
                        .supports_secret_egress_without_bypass()
                {
                    return Err(RunError::classified(
                        "mcp_holder_unsupported",
                        "authenticated ACP MCP requires provider-enforced secret substitution and no-bypass networking before Worker relay materialization",
                    ));
                }
                let injector = self.credentials.as_ref().ok_or_else(|| {
                    RunError::classified(
                        "mcp_material_source_unavailable",
                        "MCP credential requires a configured material resolver",
                    )
                })?;
                let (material_sources, recipient_bound_envelopes) =
                    injector.material_source_capabilities();
                access
                    .admit(
                        holder,
                        CredentialRealizationKind::WorkerRelay,
                        &CredentialRealizationCapabilities {
                            holders: BTreeSet::from([holder.clone()]),
                            material_sources,
                            realization_kinds: BTreeSet::from([
                                CredentialRealizationKind::WorkerRelay,
                            ]),
                            recipient_bound_envelopes,
                            extension_consumers: Default::default(),
                            alternatives: Vec::new(),
                        },
                        now_unix_ms,
                    )
                    .map_err(|error| {
                        RunError::classified("mcp_credential_admission", error.to_string())
                    })?;
                let bearer = injector
                    .resolve_for_workspace(
                        access,
                        holder,
                        CredentialRealizationKind::WorkerRelay,
                        &request.workspace_id,
                        &(&request.name, &request.target),
                    )
                    .await
                    .map_err(|error| {
                        RunError::classified(
                            "mcp_credential_revision_mismatch",
                            format!(
                                "mcp server `{}` credential could not be resolved exactly: {error}",
                                request.name
                            ),
                        )
                    })?
                    .material
                    .into_secret()
                    .map_err(|error| {
                        RunError::classified(
                            "mcp_credential_material_kind_mismatch",
                            error.to_string(),
                        )
                    })?;
                let refresh = match access.refresh.as_ref() {
                    Some(refresh) => Some(Box::new(crate::mcp::McpRefreshMaterial(
                        self.credential_refresh_factory
                            .as_ref()
                            .ok_or_else(|| {
                                RunError::classified(
                                    "mcp_credential_refresh_unavailable",
                                    "MCP credential refresh requires a Coordinator refresh adapter",
                                )
                            })?
                            .refresher(
                                awaken_credential_contract::CredentialSourceId(
                                    access.credential.id.clone(),
                                ),
                                refresh.clone(),
                            ),
                    ))),
                    None => self.credential_refresh_factory.as_ref().map(|factory| {
                        Box::new(crate::mcp::McpRefreshMaterial(factory.bearer_reloader(
                            awaken_credential_contract::CredentialSourceId(
                                access.credential.id.clone(),
                            ),
                            access.credential.revision,
                        )))
                    }),
                };
                (
                    Some(bearer),
                    refresh,
                    Some(CredentialRealizationKind::WorkerRelay),
                )
            }
            _ => {
                return Err(RunError::classified(
                    "mcp_credential_binding_invalid",
                    "MCP credential and selected plaintext holder must be present together",
                ));
            }
        };
        let projection_request = request.clone();
        let server = crate::mcp::McpTransportMaterial {
            name: request.name,
            prompts_as_skills: request.prompts_as_skills,
            transport: match sandbox_stdio.as_ref() {
                Some(target) => crate::mcp::McpTransportMaterialKind::SandboxStdio {
                    command: target.command.clone(),
                    args: target.args.clone(),
                },
                None => crate::mcp::McpTransportMaterialKind::Http {
                    url: request
                        .target
                        .http_url()
                        .expect("non-stdio MCP target must be HTTP")
                        .to_string(),
                    bearer,
                    refresh,
                },
            },
        };
        if is_acp && server.prompts_as_skills {
            return Err(RunError::classified(
                "mcp_prompt_skills_unsupported",
                "MCP prompts-as-skills requires the Native runtime; ACP does not expose a portable prompt-to-Skill projection",
            ));
        }
        let (native_wiring, mcp_process) = if is_acp {
            (None, None)
        } else if let Some(target) = sandbox_stdio.as_ref() {
            let session_id = &request.generation.session_id;
            let environment = match self.host.session_environment(session_id).await {
                Some(environment) => environment,
                None => {
                    let agent_id = self
                        .host
                        .session_slots
                        .read(session_id, |slot| {
                            slot.agent_id.clone().or_else(|| {
                                slot.baseline
                                    .as_ref()
                                    .map(|baseline| baseline.agent_id.clone())
                            })
                        })
                        .flatten()
                        .ok_or_else(|| {
                            RunError::classified(
                                "mcp_sandbox_unavailable",
                                "sandbox stdio MCP requires a frozen Session Agent before Environment realization",
                            )
                        })?;
                    self.host
                        .ctx_for(session_id, Some(&agent_id))
                        .await
                        .map_err(|error| {
                            RunError::classified(
                                "mcp_sandbox_unavailable",
                                format!(
                                    "sandbox stdio MCP could not realize its Session Environment: {error}"
                                ),
                            )
                        })?;
                    let realized = self.host.session_environment(session_id).await;
                    realized.ok_or_else(|| {
                        RunError::classified(
                            "mcp_sandbox_unavailable",
                            "sandbox stdio MCP Session Environment remained deferred after realization",
                        )
                    })?
                }
            };
            let mut argv = Vec::with_capacity(target.args.len() + 1);
            argv.push(target.command.clone());
            argv.extend(target.args.clone());
            // Match the ACP/Hand isolation contract: opaque sandbox processes
            // never inherit the image or operator home. Give this MCP target a
            // writable Session-scoped home inside the workspace so read-only
            // container root filesystems still support CLI/browser caches.
            let mcp_home_logical = format!(".mcp-home/{}", request.target.fingerprint());
            let mcp_home = format!(
                "{}/{}",
                environment.workspace_cwd().trim_end_matches('/'),
                mcp_home_logical
            );
            let home_sentinel = if crate::session_environment::AgentSandbox::supports_host_identity(
                environment.as_ref(),
            ) {
                format!("{mcp_home_logical}/.awaken-mcp-home")
            } else {
                format!("{mcp_home}/.awaken-mcp-home")
            };
            environment
                .materialize_inline(&home_sentinel, b"")
                .await
                .map_err(|error| {
                    RunError::classified(
                        "mcp_sandbox_home_failed",
                        format!("sandbox stdio MCP home could not be materialized: {error}"),
                    )
                })?;
            let (process, channel) = environment
                .spawn_agent(awaken_provisioning_contract::Command {
                    argv,
                    cwd: environment.workspace_cwd(),
                    env: vec![awaken_provisioning_contract::EnvVar {
                        name: "HOME".into(),
                        value: awaken_provisioning_contract::EnvValue::Inline { value: mcp_home },
                        visibility: awaken_provisioning_contract::EnvVisibility::Process,
                    }],
                    stdio: awaken_provisioning_contract::Stdio::Piped,
                })
                .await
                .map_err(|error| {
                    RunError::classified(
                        "mcp_sandbox_spawn_failed",
                        format!("sandbox stdio MCP process could not start: {error}"),
                    )
                })?;
            let process: Arc<dyn awaken_provisioning_contract::ProcessHandle> = Arc::from(process);
            match crate::mcp::connect_sandbox_stdio(&server, channel).await {
                Ok(wiring) => (Some(wiring), Some(process)),
                Err(error) => {
                    let _ = process
                        .signal(awaken_provisioning_contract::Signal::Term)
                        .await;
                    let _ = process.wait().await;
                    return Err(to_run_error(error));
                }
            }
        } else {
            (
                Some(
                    crate::mcp::connect_materialized(std::slice::from_ref(&server))
                        .await
                        .map_err(to_run_error)?,
                ),
                None,
            )
        };
        // Staging is the sole route-creation boundary.  Runtime construction is
        // a projection reader and must never repair or recreate credential-
        // bearing effects behind the durable realization protocol's back.
        let staged_relay = if is_acp && server.bearer().is_some() {
            let relay = self
                .host
                .mcp_relay
                .get_or_try_init(crate::mcp_relay::McpRelay::start)
                .await
                .map_err(|error| {
                    RunError::classified(
                        "mcp_relay_unavailable",
                        format!("could not stage Worker-held MCP route: {error}"),
                    )
                })?;
            if !relay.stage_route(&request.generation, &server) {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    "MCP generation already has a staged relay route",
                ));
            }
            Some(relay)
        } else {
            None
        };
        let receipt = awaken_session_contract::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind,
            receipt_fingerprint: request_fingerprint,
        };
        let cleanup_process = mcp_process.clone();
        if let Err(error) =
            self.host
                .insert_mcp_projection(crate::session_slot::McpGenerationProjection {
                    request: projection_request,
                    receipt: receipt.clone(),
                    server: Some(server),
                    native_wiring,
                    mcp_process,
                    state: crate::session_slot::McpProjectionState::Staged,
                })
        {
            // A route is private and not yet visible, but retaining its bearer
            // after the exact projection failed to stage would still be a leak.
            if let Some(relay) = staged_relay {
                relay.remove_route(&request.generation);
            }
            if let Some(process) = cleanup_process {
                let _ = process
                    .signal(awaken_provisioning_contract::Signal::Term)
                    .await;
                let _ = process.wait().await;
            }
            return Err(to_run_error(error));
        }
        Ok(receipt)
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        if !self
            .host
            .mcp_generation_is_authorized_at(&generation, now_unix_ms)
        {
            return Err(RunError::classified(
                "mcp_stale_ownership",
                "MCP publication lease has expired",
            ));
        }
        self.host
            .publish_mcp_projection(&generation)
            .await
            .map_err(to_run_error)
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.host
            .drain_mcp_projection(&generation)
            .await
            .map_err(to_run_error)
    }
}
