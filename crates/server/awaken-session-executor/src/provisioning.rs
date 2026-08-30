//! Session provisioning + capability advertisement on [`SharedHost`]: the sandbox
//! spec (with staged resource mounts, ADR-0038), the per-thread resource staging +
//! blob store accessor, and the tool/skill/delegate sets advertised on a managed
//! session. Split out of `host.rs` to keep that file under the length limit; these
//! are the same `SharedHost` (fields are `pub(crate)`).

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

#[derive(Debug)]
pub(crate) enum RepositoryPublicationActivationError {
    Rejected(pc::RepositoryPublicationRejection),
    Failed(crate::host::HostError),
}

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
        control_services: Default::default(),
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
        control_services: Default::default(),
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
        claim: Option<awaken_dispatch::RunClaim>,
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
        let has_repositories = self
            .session_slots
            .read(thread, |slot| !slot.resources.repositories.is_empty())
            .unwrap_or(false);
        let environment = self
            .session_slots
            .read(thread, |slot| slot.environment_projection.clone())
            .flatten();
        let mut spec = sandbox_spec_from_projection(
            thread,
            mounts,
            self.thread_session_env(thread),
            environment.as_ref(),
            self.session_provider.capabilities().network_isolation,
        );
        // A Repository is addressed by one sandbox-absolute `/workspace/...`
        // path through structured tools, Bash, Git, and opaque Agent processes.
        // Workdir can only rewrite cooperative tool arguments, so admitting it
        // here would create two path realities. Namespace is the minimum class;
        // `create_session_environment` additionally checks the explicit
        // tool-transparency/path-fidelity capability bits before any clone.
        if has_repositories {
            spec.isolation = spec.isolation.max(pc::IsolationClass::Namespace);
        }
        spec
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
            .repository_operation_credential(thread, repository, binding_checks, None)
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
        publication_fence: Option<&(
            awaken_session_contract::SessionRepositoryPublicationCommand,
            awaken_session_contract::SessionRealizationLease,
        )>,
    ) -> Result<Option<pc::RepositoryHttpBasicCredential>, crate::host::HostError> {
        let pin = repository.credential_pin.as_ref();
        if pin.is_none() && publication_fence.is_none() {
            return Ok(None);
        }
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
        if let Some(pin) = pin {
            let credential_binding = credential_binding.ok_or_else(|| {
                crate::host::HostError::internal(
                    "protected Repository credential pin has no authored binding",
                )
            })?;
            pin.validate_for_repository(credential_binding, remote_url)
                .map_err(|error| crate::host::HostError::internal(error.to_string()))?;
        }
        if repository.plan.source_remote_url != remote_url {
            return Err(crate::host::HostError::internal(
                "Repository changed its frozen source URL",
            ));
        }
        let publication_transport = match (publication_fence, &self.upstream) {
            (Some(fence), Some(_)) => {
                let verifier = self
                    .dispatch_session_runtime
                    .read()
                    .map_err(|_| {
                        crate::host::HostError::internal("dispatch Session Runtime lock poisoned")
                    })?
                    .as_ref()
                    .and_then(|runtime| runtime.repository_publication_binding_verifier.clone())
                    .ok_or_else(|| {
                        crate::host::HostError::internal(
                            "terminal Repository publication has no binding verifier",
                        )
                    })?;
                Some(
                    verifier
                        .verify(
                            &self.thread_workspace(thread),
                            &repository.plan.repository_id,
                            config_version,
                            Some(fence),
                        )
                        .await
                        .map_err(|error| crate::host::HostError::internal(error.to_string()))?,
                )
            }
            // Local topology already compiled this exact command through the
            // frozen terminal projection. It has no Gateway capability to mint
            // and must not re-read the mutable Repository Registry here.
            (Some(_), None) => Some(awaken_resource_contract::RepositoryTransport::Direct),
            (None, _) => None,
        };
        let Some(pin) = pin else {
            return match publication_transport {
                Some(awaken_resource_contract::RepositoryTransport::Direct)
                    if repository.plan.transport_url == repository.plan.source_remote_url =>
                {
                    Ok(None)
                }
                Some(awaken_resource_contract::RepositoryTransport::Direct) => {
                    Err(crate::host::HostError::internal(
                        "direct Repository changed its frozen transport URL",
                    ))
                }
                Some(awaken_resource_contract::RepositoryTransport::GatewayMediated { .. }) => {
                    Err(crate::host::HostError::internal(
                        "Repository has a mediated transport without a credential pin",
                    ))
                }
                None => Ok(None),
            };
        };
        match pin.selected_plaintext_holder.boundary {
            awaken_runtime_contract::PlaintextBoundary::Worker => {
                if publication_transport.as_ref().is_some_and(|transport| {
                    !matches!(
                        transport,
                        awaken_resource_contract::RepositoryTransport::Direct
                    )
                }) {
                    return Err(crate::host::HostError::internal(
                        "Worker-held Repository credential cannot use a mediated transport",
                    ));
                }
                if repository.plan.transport_url != remote_url {
                    return Err(crate::host::HostError::internal(
                        "direct Repository changed its frozen transport URL",
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
                let transport = match publication_transport {
                    Some(transport) => transport,
                    None => {
                        let verifier = self
                            .dispatch_session_runtime
                            .read()
                            .map_err(|_| {
                                crate::host::HostError::internal(
                                    "dispatch Session Runtime lock poisoned",
                                )
                            })?
                            .as_ref()
                            .and_then(|runtime| runtime.repository_binding_verifier.clone())
                            .ok_or_else(|| {
                                crate::host::HostError::internal(
                                    "Gateway-mediated Repository has no binding verifier",
                                )
                            })?;
                        verifier
                            .verify(
                                &workspace,
                                &repository.plan.repository_id,
                                config_version,
                                claim,
                            )
                            .await
                            .map_err(|error| crate::host::HostError::internal(error.to_string()))?
                    }
                };
                match transport {
                    awaken_resource_contract::RepositoryTransport::GatewayMediated {
                        remote_url,
                        capability,
                        ..
                    } if remote_url == repository.plan.transport_url => {
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

    /// Publish one exact, already-compiled Repository activation. The caller
    /// supplies the activation and binding checks derived from the frozen intent;
    /// this method never selects from the thread's current Resource set and never
    /// loops over unrelated repositories. Credential realization remains the same
    /// operation-scoped edge used by clone and replacement.
    pub(crate) async fn publish_repository_activation(
        &self,
        thread: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        realizer: &dyn pc::RepositoryRealizer,
        expectation: &pc::RepositoryPublicationExpectation,
        publication_fence: Option<&(
            awaken_session_contract::SessionRepositoryPublicationCommand,
            awaken_session_contract::SessionRealizationLease,
        )>,
    ) -> Result<pc::RepositoryPublicationReceipt, RepositoryPublicationActivationError> {
        expectation.validate().map_err(|error| {
            RepositoryPublicationActivationError::Failed(crate::host::HostError::internal(
                error.to_string(),
            ))
        })?;
        if repository.plan.access == pc::MountAccess::ReadOnly {
            return Err(RepositoryPublicationActivationError::Failed(
                crate::host::HostError::internal("read-only repository cannot be published"),
            ));
        }
        let credential = self
            .repository_operation_credential(thread, repository, binding_checks, publication_fence)
            .await
            .map_err(RepositoryPublicationActivationError::Failed)?;
        let receipt = realizer
            .publish_repository(&repository.plan, expectation, credential.as_ref())
            .await
            .map_err(|error| match error {
                pc::RepositoryPublicationError::Rejected(rejection) => {
                    RepositoryPublicationActivationError::Rejected(rejection)
                }
                pc::RepositoryPublicationError::Unavailable(error) => {
                    RepositoryPublicationActivationError::Failed(
                        crate::host::HostError::unavailable_classified(
                            "repository_publication_transport_unavailable",
                            error.to_string(),
                        ),
                    )
                }
            })?;
        receipt
            .verify(&repository.plan, expectation)
            .map_err(|error| {
                RepositoryPublicationActivationError::Failed(crate::host::HostError::internal(
                    error.to_string(),
                ))
            })?;
        Ok(receipt)
    }

    /// Harvest a thread's run-authored skills into the durable catalog (ADR-0036 D6/D8,
    /// scan the workspace skill dir for skills the
    /// agent authored this run — the self-authoring loop a Hermes-style agent runs — and
    /// persist each under its id, so a skill written in this session is delivered to the
    /// next one that opens against the same catalog. A no-op for a thread with no live
    /// environment or a host with no durable skill store (nothing to persist into).
    /// Idempotent: a re-scanned delivered skill puts identical bytes back under the same id.
    pub async fn harvest_thread_skills(&self, thread: &str) -> Result<(), ResourcePurgeError> {
        let env = self.session_environment(thread).await;
        let Some(env) = env else {
            return Ok(());
        };
        self.harvest_thread_skills_from_environment(thread, &env)
            .await
    }

    /// Harvest from an exact owner-held Environment. Terminal cleanup uses this
    /// after its background-work fence because a retryable Bound retirement is
    /// deliberately hidden from ordinary Resident readers.
    pub(crate) async fn harvest_thread_skills_from_environment(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<(), ResourcePurgeError> {
        if !self.skills.has_application() {
            return Ok(());
        }
        let workspace = self.thread_workspace(thread);
        self.persist_authored_skills(&workspace, environment.as_ref())
            .await
    }

    /// Scan a live environment's workspace skill dir and persist each authored skill to the
    /// durable catalog. Split from [`harvest_thread_skills`](Self::harvest_thread_skills) so
    /// the scan→store path is testable with a real sandbox, without a full `SessionCtx`.
    async fn persist_authored_skills(
        &self,
        workspace: &str,
        env: &crate::session_environment::SessionEnvironment,
    ) -> Result<(), ResourcePurgeError> {
        for skill in env.scan_skill_dir(crate::skills::DEFAULT_SKILLS_SUBDIR).map_err(|error| ResourcePurgeError::Storage(error.to_string()))? {
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
    ) -> Result<HarvestedArtifacts, ResourcePurgeError> {
        self.artifact_harvester().harvest(thread).await
    }

    /// Harvest outputs from the exact terminal owner snapshot. This does not
    /// broaden the ordinary Resident lookup to expose Retiring Environments.
    pub(crate) async fn harvest_thread_artifacts_from_environment(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<HarvestedArtifacts, ResourcePurgeError> {
        let harvester = self.artifact_harvester();
        let claim = harvester.current_claim(thread);
        harvester
            .harvest_with_environment(thread, claim, environment)
            .await
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
        dyn awaken_resource_contract::ArtifactPublisher<awaken_dispatch::RunClaim>,
    >,
}

#[derive(Debug)]
pub struct HarvestedArtifacts {
    pub receipts: Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
}

impl ArtifactHarvester {
    pub(crate) fn current_claim(&self, thread: &str) -> Option<awaken_dispatch::RunClaim> {
        self.session_slots
            .read(thread, |slot| slot.dispatch_claim.clone())
            .flatten()
    }

    pub(crate) async fn harvest(
        &self,
        thread: &str,
    ) -> Result<HarvestedArtifacts, ResourcePurgeError> {
        let claim = self.current_claim(thread);
        self.harvest_with_claim(thread, claim).await
    }

    pub(crate) async fn harvest_with_claim(
        &self,
        thread: &str,
        claim: Option<awaken_dispatch::RunClaim>,
    ) -> Result<HarvestedArtifacts, ResourcePurgeError> {
        let env = self
            .session_slots
            .read(thread, |slot| slot.environment_owner.resident())
            .flatten();
        let Some(env) = env else {
            return Ok(HarvestedArtifacts {
                receipts: Vec::new(),
            });
        };
        self.harvest_with_environment(thread, claim, &env).await
    }

    pub(crate) async fn harvest_with_environment(
        &self,
        thread: &str,
        claim: Option<awaken_dispatch::RunClaim>,
        env: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<HarvestedArtifacts, ResourcePurgeError> {
        let artifacts = env
            .artifacts()
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        let workspace = self
            .session_slots
            .read(thread, |slot| slot.workspace.clone())
            .flatten()
            .unwrap_or_else(|| self.local_workspace.clone());
        let mut receipts = Vec::new();
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
                idempotency_scope: None,
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
            receipts.push(receipt);
        }
        Ok(HarvestedArtifacts { receipts })
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
#[path = "provisioning/tests.rs"]
mod provisioning_registry_tests;
