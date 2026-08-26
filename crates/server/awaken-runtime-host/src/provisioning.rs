//! Session provisioning + capability advertisement on [`SharedHost`]: the sandbox
//! spec (with staged resource mounts, ADR-0038), the per-thread resource staging +
//! blob store accessor, and the tool/skill/delegate sets advertised on a managed
//! session. Split out of `host.rs` to keep that file under the length limit; these
//! are the same `SharedHost` (fields are `pub(crate)`).

#[cfg(any(test, feature = "test-support"))]
use std::sync::Arc;

use crate::host::SharedHost;
use awaken_provisioning_contract as pc;
use awaken_resource_contract::ResourcePurgeError;
#[cfg(any(test, feature = "test-support"))]
use awaken_resource_contract::{FileCatalog, FileStore};
#[cfg(test)]
use awaken_resource_contract::{FileCatalogError, FileRecord};
use awaken_runtime_contract::resolved::ToolDescriptor;

/// Anthropic Managed Agents' canonical sandbox-absolute deliverables directory.
/// `AWAKEN_OUTPUTS_DIR`, Sandbox creation, durable handles, recovery, and Files
/// harvesting all derive from this one value. Previously persisted handles retain
/// their exact path and remain adoptable without a second live-path convention.
const OUTPUTS_PATH: &str = "/mnt/session/outputs";

/// Canonical projection from the frozen Session Environment into neutral
/// provisioning vocabulary. Admission and realization both consume this value,
/// so a Worker cannot claim requirements different from those it materializes.
pub(crate) fn project_environment(
    environment: &awaken_session_contract::EnvironmentSnapshot,
) -> crate::session_slot::FrozenEnvironmentRuntimeProjection {
    let network = match &environment.network {
        awaken_session_contract::SessionNetworkPolicy::Unrestricted => {
            pc::NetworkPolicy::Unrestricted
        }
        awaken_session_contract::SessionNetworkPolicy::Allowlist { hosts } => {
            pc::NetworkPolicy::Allowlist {
                hosts: hosts.clone(),
            }
        }
        awaken_session_contract::SessionNetworkPolicy::None => pc::NetworkPolicy::None,
    };
    let packages = environment.prepared_image.as_ref().map_or_else(
        || pc::PackageRequirements {
            managers: environment
                .packages
                .manager_packages()
                .into_iter()
                .filter(|(_, packages)| !packages.is_empty())
                .map(|(manager, packages)| (manager.to_owned(), packages.to_vec()))
                .collect(),
            resolution_id: Some(format!(
                "{}:{}",
                environment.environment_id, environment.revision.0
            )),
        },
        |_| pc::PackageRequirements::default(),
    );
    let mut sandbox = environment.sandbox.clone();
    // EnvironmentSnapshot.network is the sole reachability authority. The
    // authoring overlay cannot become a second, late widening decision.
    sandbox.network = None;
    if let Some(image) = &environment.prepared_image {
        sandbox.environment = Some(pc::EnvironmentKind::Image {
            reference: image.clone(),
        });
    }
    crate::session_slot::FrozenEnvironmentRuntimeProjection {
        fingerprint: environment.config_fingerprint.clone(),
        network,
        packages,
        sandbox: (!sandbox.is_empty()).then_some(sandbox),
        provisioning: environment.sandbox_provisioning,
    }
}

/// Exact admission vector derived from the same canonical projection used by
/// Session realization. `opaque_process` distinguishes ACP, whose process must
/// see sandbox paths, from cooperative Native execution.
pub(crate) fn sandbox_requirements(
    environment: &awaken_session_contract::EnvironmentSnapshot,
    opaque_process: bool,
) -> pc::SandboxRequirements {
    let projection = project_environment(environment);
    let spec = sandbox_spec_from_projection(
        &environment.environment_id,
        Vec::new(),
        Vec::new(),
        Some(&projection),
        true,
    );
    pc::SandboxRequirements::from_spec(&spec, opaque_process)
}

/// Scheduling demand projected through the same Environment overlay used for
/// capability admission and eventual sandbox creation.
pub(crate) fn sandbox_resource_requests(
    environment: &awaken_session_contract::EnvironmentSnapshot,
) -> pc::ResourceRequests {
    let projection = project_environment(environment);
    sandbox_spec_from_projection(
        &environment.environment_id,
        Vec::new(),
        Vec::new(),
        Some(&projection),
        true,
    )
    .requests
}

/// Canonical mount/env-independent Environment projection used by both Session
/// creation and proactive capacity. Callers supply per-Session mounts and env;
/// an empty pair therefore yields the exact poolable shape, not a second warmup
/// approximation.
fn sandbox_spec_from_projection(
    scope: &str,
    mounts: Vec<pc::MountRequirement>,
    env: Vec<pc::EnvVar>,
    environment: Option<&crate::session_slot::FrozenEnvironmentRuntimeProjection>,
    provider_enforces_network_isolation: bool,
) -> pc::SandboxSpec {
    let projected_network = environment
        .map(|projection| projection.network.clone())
        .unwrap_or(pc::NetworkPolicy::Unrestricted);
    // Workdir can enforce denial only for spawned tools via its wrapper. Stronger
    // providers retain the exact frozen policy for admission and enforcement.
    let network = if projected_network.is_restricted() && !provider_enforces_network_isolation {
        pc::NetworkPolicy::Unrestricted
    } else {
        projected_network
    };
    let deny_tool_egress = environment.is_some_and(|projection| projection.network.is_restricted());
    let base = pc::SandboxSpec {
        scope: scope.to_owned(),
        isolation: pc::IsolationClass::Workdir,
        environment: None,
        command: Vec::new(),
        deny_tool_egress,
        mounts,
        env,
        packages: environment
            .map(|projection| projection.packages.clone())
            .unwrap_or_default(),
        network,
        outputs_path: OUTPUTS_PATH.to_owned(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
    };
    environment
        .and_then(|projection| projection.sandbox.clone())
        .map_or(base.clone(), |sandbox| sandbox.apply(base))
}

pub(crate) struct EnvironmentCapacityProjection {
    pub(crate) spec: pc::SandboxSpec,
    pub(crate) shape_id: pc::SandboxCapacityShapeId,
}

/// Project one frozen Environment into both the creation request and its
/// canonical capacity identity. Keeping them in one value prevents placement,
/// heartbeat receipts, and pool checkout from recomputing parallel identities.
pub(crate) fn environment_capacity_projection(
    environment: &awaken_session_contract::EnvironmentSnapshot,
    provider_enforces_network_isolation: bool,
) -> EnvironmentCapacityProjection {
    let projection = project_environment(environment);
    let spec = sandbox_spec_from_projection(
        "environment-warmup",
        Vec::new(),
        Vec::new(),
        Some(&projection),
        provider_enforces_network_isolation,
    );
    let shape_id = pc::SandboxCapacityShapeId::from_spec(&spec)
        .expect("Environment capacity projection is mount-less");
    EnvironmentCapacityProjection { spec, shape_id }
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

/// A bare Workdir spec for an ephemeral sub-run sandbox (judge / delegate / compact /
/// skill fork): scoped to the thread, no staged resource mounts, host-shared network.
pub(crate) fn agent_run_sandbox_spec(thread: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: thread.to_string(),
        isolation: pc::IsolationClass::Workdir,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: OUTPUTS_PATH.to_string(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        lease_ttl_secs: None,
    }
}

/// A thread's staged neutral mount requirements plus the prompt fragments derived
/// from the same effective Session inputs.
#[derive(Default, Clone)]
pub(crate) struct StagedResources {
    pub mounts: Vec<pc::MountRequirement>,
    pub prompts: Vec<String>,
    /// One prompt pair per MemoryStore binding. The Session delivery decision
    /// selects exactly one member; retaining both here is projection data, not
    /// a second store or mutable source of truth.
    pub memory_prompts: Vec<MemoryPromptProjection>,
    /// Resource-domain liveness checks repeated at each Session operation. These
    /// carry only Workspace-owned resource identity and frozen, secret-free config
    /// facts; authorization was completed before staging. Repository effect edges
    /// reuse its authored remote/binding to revalidate the Session pin instead of
    /// accepting the pin's own source id as self-authorization.
    pub binding_checks: Vec<ResourceBindingCheck>,
    /// Mutable Repository inputs, realized after the environment is created (not a
    /// byte mount). The plan is secret-free; its transport credential is transient.
    pub repositories: Vec<RepositoryActivation>,
}

#[derive(Clone)]
pub(crate) struct MemoryPromptProjection {
    pub filesystem: String,
    pub semantic_tools: String,
}

#[derive(Clone)]
pub(crate) enum ResourceBindingCheck {
    MemoryStore {
        memory_store_id: String,
        config_version: awaken_resource_contract::ConfigVersion,
    },
    Repository {
        repository_id: String,
        config_version: awaken_resource_contract::ConfigVersion,
        remote_url: String,
        credential_binding: Option<String>,
        claim: Option<awaken_run_ingress::RunClaim>,
    },
}

/// Runtime-only activation material for one already-resolved Repository config.
/// The plan is the neutral environment port and the credential pin is the exact,
/// secret-free Session decision. Plaintext is materialized only at a Git effect
/// edge, used for that one call, and never retained by this activation, the plan,
/// origin URL, Session manifest, or sandbox.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RepositoryActivation {
    pub plan: pc::RepositoryRealizationPlan,
    pub credential_pin: Option<awaken_session_contract::ResolvedRepositoryCredential>,
}

fn repository_http_basic_credential(
    material: awaken_runtime_contract::CredentialMaterial,
) -> Result<pc::RepositoryHttpBasicCredential, &'static str> {
    let awaken_runtime_contract::CredentialMaterial::Structured(mut material) = material else {
        return Err("HTTP Basic requires structured credential material");
    };
    if material.type_id != awaken_runtime_contract::credential::HTTP_BASIC_MATERIAL_TYPE {
        return Err("HTTP Basic credential material has the wrong type");
    }
    let username = material
        .fields
        .remove("username")
        .ok_or("HTTP Basic credential material has no username")?;
    let password = material
        .fields
        .remove("password")
        .ok_or("HTTP Basic credential material has no password")?;
    Ok(pc::RepositoryHttpBasicCredential::new(username, password))
}

impl SharedHost {
    pub(crate) fn artifact_harvester(&self) -> ArtifactHarvester {
        ArtifactHarvester {
            session_slots: self.session_slots.clone(),
            local_workspace: self.local_workspace.clone(),
            publisher: self.artifact_publisher.clone(),
        }
    }

    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools plus the session's
    /// staged resource mounts (ADR-0038), each realized read-only under `.mnt/`.
    pub(crate) fn sandbox_spec(&self, thread: &str) -> pc::SandboxSpec {
        let mounts = self.thread_session_mounts(thread);
        let environment = self
            .session_slots
            .read(thread, |slot| slot.environment_projection.clone())
            .flatten();
        sandbox_spec_from_projection(
            thread,
            mounts,
            self.thread_session_env(thread),
            environment.as_ref(),
            self.session_provider.capabilities().network_isolation,
        )
    }

    /// Stage a thread's resources (mounts + prompt fragments); consumed by
    /// `sandbox_spec` and injected into the run's system prompt. From `prepare_session`.
    /// REPLACES the Thread's set (correct at create time, before its first Run).
    pub(crate) fn register_thread_resources(&self, thread: &str, staged: StagedResources) {
        self.session_slots
            .update(thread, |slot| slot.resources = staged);
    }

    pub(crate) fn register_thread_resource_manifest(
        &self,
        thread: &str,
        manifest: awaken_session_contract::SessionResourceManifest,
    ) {
        self.session_slots
            .update(thread, |slot| slot.manifest = Some(manifest));
    }

    pub(crate) fn thread_resource_manifest(
        &self,
        thread: &str,
    ) -> Option<awaken_session_contract::SessionResourceManifest> {
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn file_store(&self) -> Arc<dyn FileStore> {
        self.file_store.clone()
    }

    /// The sole durable logical-file catalog. Public Files identities resolve
    /// through this catalog before their private content-addressed blob is read.
    #[cfg(any(test, feature = "test-support"))]
    pub fn file_catalog(&self) -> Arc<dyn FileCatalog> {
        self.file_catalog.clone()
    }

    /// The sole Resources-owned logical-File command application. Database-less
    /// Workers deliberately return `None`; they may only use `FileContentSource`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn file_application(
        &self,
    ) -> Option<Arc<dyn awaken_resource_contract::FileApplicationService>> {
        self.file_application.clone()
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
        let resources = self.thread_resources_snapshot(thread);
        for repository in &resources.repositories {
            self.realize_repository_activation(
                thread,
                repository,
                &resources.binding_checks,
                realizer,
            )
            .await?;
        }
        Ok(())
    }

    /// Execute one Repository clone edge through the same operation-scoped
    /// credential path used by initial activation and hot Resource replacement.
    pub(crate) async fn realize_repository_activation(
        &self,
        thread: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        realizer: &dyn pc::RepositoryRealizer,
    ) -> Result<(), crate::host::HostError> {
        let credential = self
            .repository_operation_credential(thread, repository, binding_checks)
            .await?;
        realizer
            .realize_repository(&repository.plan, credential.as_ref())
            .await
            .map_err(|error| crate::host::HostError::internal(error.to_string()))
    }

    /// Materialize only the credential required by this Git effect. A direct
    /// Worker-held pin repeats exact source revision, Workspace, descriptor
    /// expiry/target/material, holder, and Repository id/version binding checks.
    /// A Platform-held pin refreshes its short Gateway capability and cannot
    /// downgrade to direct material. The returned owned value is dropped after
    /// the realizer call and is never written back to `RepositoryActivation`.
    async fn repository_operation_credential(
        &self,
        thread: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
    ) -> Result<Option<pc::RepositoryHttpBasicCredential>, crate::host::HostError> {
        let Some(pin) = repository.credential_pin.as_ref() else {
            return Ok(None);
        };
        let mut matching = binding_checks.iter().filter_map(|check| match check {
            ResourceBindingCheck::Repository {
                repository_id,
                config_version,
                remote_url,
                credential_binding,
                claim,
            } if repository_id == &repository.plan.repository_id => Some((
                *config_version,
                remote_url.as_str(),
                credential_binding.as_deref(),
                claim.as_ref(),
            )),
            _ => None,
        });
        let Some((config_version, remote_url, credential_binding, claim)) = matching.next() else {
            return Err(crate::host::HostError::internal(
                "protected Repository has no exact binding check",
            ));
        };
        if matching.next().is_some() {
            return Err(crate::host::HostError::internal(
                "protected Repository has ambiguous binding checks",
            ));
        }
        let credential_binding = credential_binding.ok_or_else(|| {
            crate::host::HostError::internal(
                "protected Repository credential pin has no authored binding",
            )
        })?;
        pin.validate_for_repository(credential_binding, remote_url)
            .map_err(|error| crate::host::HostError::internal(error.to_string()))?;
        match pin.selected_plaintext_holder.boundary {
            awaken_runtime_contract::PlaintextBoundary::Worker => {
                if repository.plan.remote_url != remote_url {
                    return Err(crate::host::HostError::internal(
                        "direct Repository changed its frozen upstream URL",
                    ));
                }
                let materializer = self.credential_materializer.as_ref().ok_or_else(|| {
                    crate::host::HostError::internal(
                        "repository credential requires a configured credential materializer",
                    )
                })?;
                let workspace = self.thread_workspace(thread);
                let material = materializer
                    .resolve_for_workspace(
                        &pin.access,
                        &pin.selected_plaintext_holder,
                        awaken_runtime_contract::CredentialRealizationKind::WorkerRelay,
                        &workspace,
                        &(&repository.plan.repository_id, config_version),
                    )
                    .await
                    .map_err(|error| crate::host::HostError::internal(error.to_string()))?
                    .material;
                repository_http_basic_credential(material)
                    .map(Some)
                    .map_err(crate::host::HostError::internal)
            }
            awaken_runtime_contract::PlaintextBoundary::Platform => {
                let workspace = self.thread_workspace(thread);
                let verifier = self
                    .dispatch_session_runtime
                    .read()
                    .map_err(|_| {
                        crate::host::HostError::internal("dispatch Session Runtime lock poisoned")
                    })?
                    .as_ref()
                    .and_then(|runtime| runtime.repository_binding_verifier.clone())
                    .ok_or_else(|| {
                        crate::host::HostError::internal(
                            "Gateway-mediated Repository has no binding verifier",
                        )
                    })?;
                let transport = verifier
                    .verify(
                        &workspace,
                        &repository.plan.repository_id,
                        config_version,
                        claim,
                    )
                    .await
                    .map_err(|error| crate::host::HostError::internal(error.to_string()))?;
                match transport {
                    awaken_resource_contract::RepositoryTransport::GatewayMediated {
                        remote_url,
                        capability,
                    } if remote_url == repository.plan.remote_url => {
                        Ok(Some(pc::RepositoryHttpBasicCredential::gateway_capability(
                            capability.expose().to_owned(),
                        )))
                    }
                    awaken_resource_contract::RepositoryTransport::GatewayMediated { .. } => {
                        Err(crate::host::HostError::internal(
                            "Gateway-mediated Repository changed its frozen remote URL",
                        ))
                    }
                    awaken_resource_contract::RepositoryTransport::Direct => {
                        Err(crate::host::HostError::internal(
                            "Gateway-mediated Repository cannot fall back to direct credentials",
                        ))
                    }
                }
            }
            awaken_runtime_contract::PlaintextBoundary::Workload => {
                Err(crate::host::HostError::internal(
                    "Repository credential selected an unsupported plaintext holder",
                ))
            }
        }
    }

    /// Publish a thread's Agent-authored commits through the Repository realizer.
    /// The host never fabricates a commit or resolves another config; it only applies
    /// the already-selected activation and its ephemeral transport credential.
    ///
    /// This is the sole publication path for a runtime-staged Repository. Agents
    /// never receive the Git credential or perform a competing push. A no-op for
    /// a thread with no repositories, no live environment, or no authored commit.
    /// A transport failure keeps terminal cleanup pending so the live checkout
    /// remains available for the same idempotent Git publication retry.
    pub async fn publish_thread_repositories(
        &self,
        thread: &str,
    ) -> Result<(), ResourcePurgeError> {
        let env = self.session_environment(thread).await;
        let resources = self.thread_resources_snapshot(thread);
        let Some(env) = env else {
            return Ok(());
        };
        for repository in &resources.repositories {
            if repository.plan.access == pc::MountAccess::ReadOnly {
                continue;
            }
            let credential = self
                .repository_operation_credential(thread, repository, &resources.binding_checks)
                .await
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            pc::RepositoryRealizer::publish_repository(
                env.as_ref(),
                &repository.plan,
                credential.as_ref(),
            )
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        }
        Ok(())
    }

    /// Harvest a thread's run-authored skills into the durable catalog (ADR-0036 D6/D8,
    /// scan the workspace skill dir for skills the
    /// agent authored this run — the self-authoring loop a Hermes-style agent runs — and
    /// persist each under its id, so a skill written in this session is delivered to the
    /// next one that opens against the same catalog. A no-op for a thread with no live
    /// environment or a host with no durable skill store (nothing to persist into).
    /// Idempotent: a re-scanned delivered skill puts identical bytes back under the same id.
    pub async fn harvest_thread_skills(&self, thread: &str) -> Result<(), ResourcePurgeError> {
        if !self.skills.has_application() {
            return Ok(());
        }
        let env = self.session_environment(thread).await;
        let Some(env) = env else {
            return Ok(());
        };
        let workspace = self.thread_workspace(thread);
        self.persist_authored_skills(&workspace, env.as_ref()).await
    }

    /// Scan a live environment's workspace skill dir and persist each authored skill to the
    /// durable catalog. Split from [`harvest_thread_skills`](Self::harvest_thread_skills) so
    /// the scan→store path is testable with a real sandbox, without a full `SessionCtx`.
    async fn persist_authored_skills(
        &self,
        workspace: &str,
        env: &crate::session_environment::SessionEnvironment,
    ) -> Result<(), ResourcePurgeError> {
        for skill in env.scan_skill_dir(crate::skills::DEFAULT_SKILLS_SUBDIR) {
            if let Some(result) = self
                .skills
                .persist_authored(workspace, &skill.id, &skill.content)
                .await
            {
                result.map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            }
        }
        Ok(())
    }

    /// Persist every Agent-authored output before the environment can be disposed.
    /// The `(Session, logical path, content)` harvest key makes retries idempotent;
    /// Files API reads only this durable catalog and never scan the Sandbox.
    pub async fn harvest_thread_artifacts(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_resource_contract::ArtifactPublicationReceipt>, ResourcePurgeError> {
        self.artifact_harvester().harvest(thread).await
    }
}

/// The single Runtime-to-Resources application edge for Session outputs.
///
/// It is cloneable so the same operation can decorate direct, durable, and
/// recovered attempts without retaining the whole Host (and creating a
/// SessionCtx -> executor -> Host reference cycle). Terminal release calls the
/// same operation as an idempotent final retry before Sandbox disposal.
#[derive(Clone)]
pub(crate) struct ArtifactHarvester {
    session_slots: crate::session_slot::SessionRuntimeSlots,
    local_workspace: String,
    publisher: std::sync::Arc<
        dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>,
    >,
}

impl ArtifactHarvester {
    pub(crate) fn current_claim(&self, thread: &str) -> Option<awaken_run_ingress::RunClaim> {
        self.session_slots
            .read(thread, |slot| slot.dispatch_claim.clone())
            .flatten()
    }

    pub(crate) async fn harvest(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_resource_contract::ArtifactPublicationReceipt>, ResourcePurgeError> {
        let claim = self.current_claim(thread);
        self.harvest_with_claim(thread, claim).await
    }

    pub(crate) async fn harvest_with_claim(
        &self,
        thread: &str,
        claim: Option<awaken_run_ingress::RunClaim>,
    ) -> Result<Vec<awaken_resource_contract::ArtifactPublicationReceipt>, ResourcePurgeError> {
        let env = self
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten();
        let Some(env) = env else {
            return Ok(Vec::new());
        };
        let artifacts = env
            .artifacts()
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        let workspace = self
            .session_slots
            .read(thread, |slot| slot.workspace.clone())
            .flatten()
            .unwrap_or_else(|| self.local_workspace.clone());
        let mut out = Vec::new();
        for artifact in artifacts {
            let bytes = env
                .read_artifact(&artifact.id)
                .await
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            let content_id = awaken_resource_contract::content_id(&bytes);
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
            let effect_id = awaken_resource_contract::harvest_idempotency_key(
                thread,
                &logical_path,
                &content_id,
            );
            let publication = awaken_resource_contract::ArtifactPublication {
                effect_id,
                workspace_id: workspace.clone(),
                session_id: thread.to_string(),
                logical_path,
                mime_type,
                content_id,
                bytes,
                fence: claim.clone(),
            };
            publication
                .verify()
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            let receipt = self
                .publisher
                .publish(publication.clone())
                .await
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            receipt
                .verify(&publication)
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            out.push(receipt);
        }
        Ok(out)
    }
}

impl SharedHost {
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
                    .delegates
                    .into_iter()
                    .map(|binding| binding.agent_id.0)
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

    /// Whether a projected Workdir spec denies tool egress.
    fn denies(spec: &pc::SandboxSpec) -> bool {
        spec.deny_tool_egress
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

    fn root_terminal_cleanup_command(
        session_id: &str,
    ) -> awaken_session_contract::SessionCleanupCommand {
        let mut operation = awaken_session_contract::SessionCleanupOperation::default();
        assert!(operation.request(session_id));
        operation.freeze_targets(session_id, [], 0, 0).unwrap();
        operation.command_for(session_id, session_id).unwrap()
    }

    #[test]
    fn session_and_housekeeping_filesystem_continuity_are_distinct() {
        /* Continuity cause/effect table.
         * Causes: C1 the spec realizes the canonical Session environment; C2
         * the spec realizes a disposable child/probe environment. Effects: E1
         * request retained writable state for DurableRequest recovery; E2
         * request ephemeral state and therefore no configured continuation PVC.
         * Rules: SC1 C1=>E1; SC2 C2=>E2. The typed field participates in the
         * capacity identity, so the two requests cannot share warm capacity.
         */
        assert_eq!(
            host().sandbox_spec("session").filesystem_continuity,
            pc::FilesystemContinuity::Retained,
            "SC1"
        );
        assert_eq!(
            agent_run_sandbox_spec("probe").filesystem_continuity,
            pc::FilesystemContinuity::Ephemeral,
            "SC2"
        );
        assert_ne!(
            pc::SandboxCapacityShapeId::from_spec(&host().sandbox_spec("session")),
            pc::SandboxCapacityShapeId::from_spec(&agent_run_sandbox_spec("probe")),
            "SC1/SC2"
        );
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
            credential_pin: None,
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
                memory_prompts: Vec::new(),
                binding_checks: Vec::new(),
                repositories: vec![repository_activation("repo-a")],
            },
        );
        // A second register REPLACES (correct at create time, before any first Run).
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("c.md")],
                prompts: vec!["second".into()],
                memory_prompts: Vec::new(),
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
            &awaken_session_contract::EnvironmentSnapshot {
                environment_id: "environment".into(),
                revision: awaken_session_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                    "environment-1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_contract::SessionNetworkPolicy::None,
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
                    credential_pin: None,
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
        host.publish_thread_repositories("t").await.unwrap(); // no env → no publish
        host.harvest_thread_skills("t").await.unwrap(); // no env / no store → no persist
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
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        use awaken_session_contract::SessionRuntime;
        // Cause/effect decision table:
        // R1 output present + live Sandbox => harvest creates one scoped File.
        // R2 identical retry => same File id, no duplicate manifest row/reference.
        // R3 terminal release => Sandbox gone while File metadata/bytes remain.
        let storage = tempfile::tempdir().unwrap();
        let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
        host.register_thread_workspace("session-artifacts", "workspace-a");
        let spec = agent_run_sandbox_spec("session-artifacts");
        let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(storage.path())
                .create_sandbox(&spec)
                .await
                .unwrap(),
        ));
        host.session_slots.update("session-artifacts", |slot| {
            slot.environment = Some(environment.clone())
        });
        let output = storage
            .path()
            .join("session-artifacts")
            .join(spec.outputs_path.trim_start_matches('/'))
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
        assert_eq!(retry[0].record.id, first[0].record.id);
        assert!(first[0].record.id.starts_with("file_"));
        assert!(first[0].record.downloadable);

        crate::ManagedHost::new(host.clone())
            .execute_terminal_cleanup(root_terminal_cleanup_command("session-artifacts"))
            .await
            .unwrap();
        assert!(
            host.session_environment("session-artifacts")
                .await
                .is_none()
        );
        let records = host
            .file_application()
            .expect("test startup installs File application")
            .list("workspace-a", Some("session-artifacts"))
            .await
            .unwrap();
        assert_eq!(records.len(), 1, "terminal retry remains idempotent");
        assert_eq!(
            host.file_application()
                .expect("test startup installs File application")
                .bytes("workspace-a", &records[0].id)
                .await
                .unwrap()
                .unwrap()
                .1,
            b"durable report"
        );
    }

    struct FailingFileCatalog;

    use awaken_resource_contract::CreateFileRecordOutcome;

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
        use awaken_session_contract::SessionRuntime;

        // Test design. Causes: R4 has terminal output present while its durable
        // catalog write fails. Effects: end_session fails and preserves both
        // Environment and output for retry. Constraint/Invariant: sandbox disposal
        // follows successful durable harvest, never precedes it. Decision rule:
        // execute R4 and require failure with zero disposal.
        let storage = tempfile::tempdir().unwrap();
        let mut raw_host = SharedHost::new(Arc::new(NoLlm), "test");
        let catalog = Arc::new(FailingFileCatalog);
        raw_host.file_catalog = catalog.clone();
        let application = Arc::new(awaken_resource_application::FileApplication::new(
            raw_host.file_store(),
            catalog,
            raw_host
                .resource_reclamation()
                .expect("test lifecycle repository"),
        ));
        raw_host = raw_host.with_file_application(
            application.clone(),
            Arc::new(
                awaken_resource_application::ApplicationFileContentSource::new(application.clone()),
            ),
            Arc::new(awaken_resource_application::ApplicationArtifactPublisher::new(application)),
        );
        let host = Arc::new(raw_host);
        let spec = agent_run_sandbox_spec("session-harvest-failure");
        let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(storage.path())
                .create_sandbox(&spec)
                .await
                .unwrap(),
        ));
        host.session_slots
            .update("session-harvest-failure", |slot| {
                slot.environment = Some(environment)
            });
        let output = storage
            .path()
            .join("session-harvest-failure")
            .join(spec.outputs_path.trim_start_matches('/'))
            .join("report.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, b"retry me").unwrap();

        let error = crate::ManagedHost::new(host.clone())
            .execute_terminal_cleanup(root_terminal_cleanup_command("session-harvest-failure"))
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
            .await
            .unwrap();
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
