// @formal entrypoint resource_catalog.artifact_publication_receipt_requires_exact_publication_identity
// @formal entrypoint sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
//! Session provisioning and capability advertisement. ResourceStaging owns the
//! neutral sandbox projection and Repository effects; the realization executor owns
//! artifact harvesting through the selected Resource authority. The staged
//! resource mounts retain ADR-0038's one physical projection.

use crate::realization_error::{PendingState, RealizationError};
use crate::staged_resources::{RepositoryActivation, ResourceBindingCheck, StagedResources};
use awaken_session_configuration_contract::environment_projection;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::ResourceOperationFence;
use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_resource_contract::ArtifactPublicationError;

#[derive(Debug)]
pub enum RepositoryPublicationActivationError {
    Rejected(pc::RepositoryPublicationRejection),
    Failed(RealizationError),
}

/// A denied installed-Session read leaves the Artifact authority unknown; the
/// publication retries once a healthy owner can read it.
fn artifact_registry_denied(error: RealizationError) -> ArtifactPublicationError {
    ArtifactPublicationError::AuthorityUnavailable {
        detail: error.to_string(),
    }
}

/// One physical-mount classifier shared by fresh-spec construction and live
/// transition validation. MemoryStore mounts are provider create-time state;
/// File mounts use the canonical late-attach port.
pub fn resource_mount_is_create_time(mount: &pc::MountRequirement) -> bool {
    matches!(mount.source, pc::MountSource::MemoryStore { .. })
}

/// An inference-only execution can omit a physical Environment only when its complete immutable
/// execution surface is environment-free. Inference-only Native/A2A routes do
/// not need a Sandbox; ACP owns a CLI process and every executable integration
/// keeps the ordinary Session Environment path.
pub fn environment_free_inference(
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
) -> bool {
    let agent = &snapshot.resolved_spec.plugin_config.agent;
    snapshot.resolved_spec.tool_descriptors.is_empty()
        && snapshot.resolved_spec.plugin_ids.is_empty()
        && agent.mcp_servers.is_empty()
        && agent.skills.is_empty()
        && agent.delegates.is_empty()
        && agent.advisor.is_none()
        && agent.toolsets.is_empty()
        && snapshot
            .resolved_spec
            .attempt_candidates(None)
            .into_iter()
            .all(|candidate| {
                matches!(
                    awaken_runtime_contract::resolved::Backend::from_ref(
                        candidate.binding().identity().2,
                    ),
                    awaken_runtime_contract::resolved::Backend::Native
                        | awaken_runtime_contract::resolved::Backend::Remote(_)
                )
            })
}

/// One ephemeral, complete input snapshot for projecting a Session's physical
/// Sandbox layout. It is assembled at the caller's single projection point and
/// consumed immediately; the frozen Session and process-local slot remain the
/// authorities for every field.
pub struct ProjectedSandboxLayout<'a> {
    pub resource_mounts: Vec<pc::MountRequirement>,
    pub has_repositories: bool,
    pub baseline_mounts: Option<&'a [pc::MountRequirement]>,
    pub baseline_env: Option<&'a [pc::EnvVar]>,
    pub content_delivery: Option<crate::frozen_inputs::ManagedContentDelivery>,
    pub environment: Option<&'a environment_projection::FrozenEnvironmentInputs>,
    pub network_isolation: bool,
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
pub fn agent_run_sandbox_spec(thread: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: thread.to_string(),
        isolation: pc::IsolationClass::Workdir,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.to_string(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        control_services: Default::default(),
        lease_ttl_secs: None,
    }
}

fn repository_http_basic_credential(
    material: awaken_credential_contract::CredentialMaterial,
) -> Result<pc::RepositoryHttpBasicCredential, &'static str> {
    let awaken_credential_contract::CredentialMaterial::Structured(mut material) = material else {
        return Err("HTTP Basic requires structured credential material");
    };
    if material.type_id != awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE {
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

/// The single Runtime-to-Resources application edge for Session outputs.
///
/// It is cloneable so the same operation can decorate direct, durable, and
/// recovered attempts without retaining the whole Host (and creating a
/// SessionCtx -> executor -> Host reference cycle). Terminal release calls the
/// same operation as an idempotent final retry before Sandbox disposal.
#[derive(Clone)]
pub struct ArtifactHarvester {
    sessions: crate::session_realizations::SessionRealizations,
    local_workspace: String,
    publisher:
        std::sync::Arc<dyn awaken_resource_contract::ArtifactPublisher<ResourceOperationFence>>,
}

/// Closed, process-local choice of Artifact source for one harvest attempt.
/// Receipt-only recovery is authorized exclusively by a terminal fence; it is
/// never inferred for an ordinary Run whose live Environment is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactCaptureMode {
    Live,
    ReceiptOnly,
}

#[derive(Debug)]
pub struct HarvestedArtifacts {
    pub receipts: Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
}

impl ArtifactHarvester {
    pub fn new(
        sessions: crate::session_realizations::SessionRealizations,
        local_workspace: String,
        publisher: Arc<dyn awaken_resource_contract::ArtifactPublisher<ResourceOperationFence>>,
    ) -> Self {
        Self {
            sessions,
            local_workspace,
            publisher,
        }
    }

    pub fn publisher(
        &self,
    ) -> Arc<dyn awaken_resource_contract::ArtifactPublisher<ResourceOperationFence>> {
        self.publisher.clone()
    }

    pub fn current_claim(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_dispatch_contract::RunClaim>, RealizationError> {
        self.sessions.dispatch_claim(thread)
    }

    pub async fn harvest(
        &self,
        thread: &str,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        let fence = self
            .current_claim(thread)
            .map_err(artifact_registry_denied)?
            .map(ResourceOperationFence::Run);
        self.harvest_with_fence(thread, fence).await
    }

    pub async fn harvest_with_claim(
        &self,
        thread: &str,
        claim: Option<awaken_dispatch_contract::RunClaim>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence(thread, claim.map(ResourceOperationFence::Run))
            .await
    }

    pub async fn harvest_with_fence(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence_mode(thread, fence, ArtifactCaptureMode::Live)
            .await
    }

    /// Execute one harvest through the sole publish/recovery owner. Callers may
    /// select `ReceiptOnly` only when existing physical cleanup evidence proves
    /// that live output capture must not be attempted.
    pub async fn harvest_with_fence_mode(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
        capture_mode: ArtifactCaptureMode,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence_mode_from_environment(thread, fence, capture_mode, None)
            .await
    }

    /// Use one exact owner-held Environment while retaining the same publisher,
    /// fence validation, idempotency key, and receipt recovery path.
    pub async fn harvest_with_environment_and_fence(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence_mode_from_environment(
            thread,
            fence,
            ArtifactCaptureMode::Live,
            Some(environment),
        )
        .await
    }

    async fn harvest_with_fence_mode_from_environment(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
        capture_mode: ArtifactCaptureMode,
        exact_environment: Option<&Arc<crate::session_environment::SessionEnvironment>>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        let terminal_effect = fence.as_ref().and_then(|fence| match fence {
            ResourceOperationFence::Terminal(effect) => Some(effect),
            ResourceOperationFence::Run(_)
            | ResourceOperationFence::SessionMaterialization(_)
            | ResourceOperationFence::RepositoryPublication { .. }
            | ResourceOperationFence::CheckpointRelease(_) => None,
        });
        if capture_mode == ArtifactCaptureMode::ReceiptOnly && terminal_effect.is_none() {
            return Err(ArtifactPublicationError::InvalidRequest {
                detail: "receipt-only Artifact recovery requires a terminal fence".into(),
            });
        }
        let workspace_owner = terminal_effect
            .map(|effect| effect.command.session_id.as_str())
            .unwrap_or(thread);
        let workspace = self
            .sessions
            .frozen(workspace_owner)
            .map_err(artifact_registry_denied)?
            .workspace
            .clone();
        let workspace = match (workspace, terminal_effect) {
            (Some(workspace), _) => workspace,
            (None, Some(_)) => {
                return Err(ArtifactPublicationError::AuthorityUnavailable {
                    detail: "terminal Artifact recovery has no root Session Workspace projection"
                        .into(),
                });
            }
            (None, None) => self.local_workspace.clone(),
        };
        if capture_mode == ArtifactCaptureMode::ReceiptOnly {
            return self
                .recover_terminal_receipts(
                    thread,
                    workspace,
                    terminal_effect.expect("receipt-only mode requires a terminal effect"),
                )
                .await;
        }
        let projected_environment = match exact_environment {
            Some(environment) => Some(environment.clone()),
            None => self
                .sessions
                .read_realization(thread, |r| r.environment.owner.resident())
                .map_err(artifact_registry_denied)?
                .flatten(),
        };
        let Some(env) = projected_environment else {
            if let Some(effect) = terminal_effect {
                return self
                    .recover_terminal_receipts(thread, workspace, effect)
                    .await;
            }
            return Ok(HarvestedArtifacts {
                receipts: Vec::new(),
            });
        };
        // Read the revision before observing bytes. On CAS conflict a caller
        // must start a fresh harvest, never rebase an old scan onto a new head.
        let current_capture = self
            .publisher
            .capture(&workspace, thread, fence.as_ref())
            .await?;
        if let Some(capture) = &current_capture {
            capture
                .verify_scope(&workspace, thread)
                .map_err(ArtifactPublicationError::from)?;
        }
        let expected_revision = current_capture.map_or(0, |capture| capture.revision);
        let artifacts = env.harvest().capture_artifacts().await.map_err(|error| {
            ArtifactPublicationError::Indeterminate {
                detail: error.to_string(),
            }
        })?;
        let idempotency_scope = terminal_effect.map(|effect| effect.operation_id().to_string());
        let mut receipts = Vec::new();
        for captured in artifacts {
            let artifact = captured.metadata;
            let bytes = captured.bytes;
            let content_id = awaken_resource_contract::content_id(&bytes);
            if content_id != artifact.content_hash || content_id != artifact.id {
                return Err(ArtifactPublicationError::CorruptData {
                    detail: format!("artifact `{}` changed during harvest", artifact.path),
                });
            }
            let logical_path = pc::WorkspaceLayout::outputs_relative(&artifact.path)
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
                idempotency_scope: idempotency_scope.clone(),
                fence: fence.clone(),
            };
            publication.verify()?;
            let receipt = self.publisher.publish(publication.clone()).await?;
            receipt.verify(&publication)?;
            receipts.push(receipt);
        }
        let replacement = awaken_resource_contract::ReplaceArtifactCapture::new(
            workspace,
            thread.into(),
            expected_revision,
            receipts
                .iter()
                .map(|receipt| receipt.record.id.clone())
                .collect(),
        );
        let outcome = self
            .publisher
            .replace_capture(replacement.clone(), fence.as_ref())
            .await?;
        replacement
            .verify_outcome(&outcome)
            .map_err(ArtifactPublicationError::from)?;
        if outcome == awaken_resource_contract::ReplaceArtifactCaptureOutcome::Conflict {
            return Err(ArtifactPublicationError::Conflict {
                detail: "output capture changed; a fresh harvest is required".into(),
            });
        }
        Ok(HarvestedArtifacts { receipts })
    }

    async fn recover_terminal_receipts(
        &self,
        thread: &str,
        workspace: String,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        let recovery = awaken_resource_contract::ArtifactRecovery {
            workspace_id: workspace,
            session_id: thread.to_string(),
            idempotency_scope: effect.operation_id().to_string(),
            fence: ResourceOperationFence::Terminal(effect.clone()),
        };
        let receipts = self.publisher.recover(recovery).await?;
        Ok(HarvestedArtifacts { receipts })
    }
}

/// Gap-1 coverage: the resource-staging registry (`register`/`merge`/`remove`),
/// `sandbox_spec` projection, the `realize_thread_repositories` fail-closed contract,
/// and the reverse-channel no-ops when a thread has no live environment. These
/// exercise the host-plane provisioning bookkeeping directly; the wired
/// memory/repo write-back happy paths run in `host::tests` through a real session.
pub fn project_session_mounts(
    mut resources: Vec<awaken_provisioning_contract::MountRequirement>,
    baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
    content_delivery: Option<crate::frozen_inputs::ManagedContentDelivery>,
) -> Vec<awaken_provisioning_contract::MountRequirement> {
    if let Some(baseline_mounts) = baseline_mounts {
        resources.extend_from_slice(baseline_mounts);
    }
    if content_delivery == Some(crate::frozen_inputs::ManagedContentDelivery::SemanticTools) {
        resources.retain(|mount| {
            !matches!(
                mount.source,
                awaken_provisioning_contract::MountSource::MemoryStore { .. }
            )
        });
    }
    resources
}

/// Test-only spec helpers over the default Session provider. A fixture with a
/// poisoned registry is a broken test, so these helpers panic instead of
/// reporting the realization denial production paths return.
#[cfg(any(test, feature = "test-support"))]
impl crate::environment_holding::EnvironmentHolding {
    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools plus the session's
    /// staged resource mounts (ADR-0038), each realized read-only under `.mnt/`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sandbox_spec(&self, thread: &str) -> pc::SandboxSpec {
        let resources = self
            .staging
            .thread_resources_snapshot(thread)
            .expect("test fixture registry is healthy");
        self.sandbox_spec_for_resources(thread, &resources)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn sandbox_spec_for_resources(
        &self,
        thread: &str,
        resources: &StagedResources,
    ) -> pc::SandboxSpec {
        self.sandbox_spec_for_mount_layout(
            thread,
            resources.mounts.clone(),
            !resources.repositories.is_empty(),
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn sandbox_spec_for_mount_layout(
        &self,
        thread: &str,
        resource_mounts: Vec<pc::MountRequirement>,
        has_repositories: bool,
    ) -> pc::SandboxSpec {
        self.staging
            .sandbox_spec_for_mount_layout_with_network(
                thread,
                resource_mounts,
                has_repositories,
                self.env
                    .session_provider
                    .realizer()
                    .capabilities()
                    .network_isolation,
            )
            .expect("test fixture registry is healthy")
    }
}

impl crate::resource_staging::ResourceStaging {
    /// Realize a thread's resolved Repository inputs into its freshly-created
    /// environment. The narrow port receives only a secret-free plan and an ephemeral
    /// transport credential after authorization/config resolution. A failure aborts
    /// activation so the Session cannot run believing a working tree exists.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn realize_thread_repositories(
        &self,
        thread: &str,
        fallback_workspace: &str,
        realizer: &dyn pc::RepositoryRealizer,
    ) -> Result<(), RealizationError> {
        let resources = self.thread_resources_snapshot(thread)?;
        for repository in &resources.repositories {
            self.realize_repository_activation(
                thread,
                fallback_workspace,
                repository,
                &resources.binding_checks,
                realizer,
                pc::RepositoryRealizationIntent::Initialize,
            )
            .await?;
        }
        Ok(())
    }

    /// Execute one Repository clone edge through the same operation-scoped
    /// credential path used by initial activation and hot Resource replacement.
    pub async fn realize_repository_activation(
        &self,
        thread: &str,
        fallback_workspace: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        realizer: &dyn pc::RepositoryRealizer,
        intent: pc::RepositoryRealizationIntent,
    ) -> Result<(), RealizationError> {
        repository
            .plan
            .validate_mount_path()
            .map_err(RealizationError::from)?;
        let credential = self
            .repository_operation_credential(
                thread,
                fallback_workspace,
                repository,
                binding_checks,
                None,
            )
            .await?;
        realizer
            .realize_repository(&repository.plan, credential.as_ref(), intent)
            .await
            .map_err(RealizationError::from)
    }

    /// Materialize only the credential required by this Git effect. A direct
    /// Sandbox-held pin repeats exact source revision, Workspace, descriptor
    /// expiry/target/material, holder, and Repository id/version binding checks.
    /// A Platform-held pin refreshes its short Gateway capability and cannot
    /// downgrade to direct material. The returned owned value is dropped after
    /// the realizer call and is never written back to `RepositoryActivation`.
    async fn repository_operation_credential(
        &self,
        thread: &str,
        fallback_workspace: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        publication_fence: Option<&(
            awaken_session_state_contract::SessionRepositoryPublicationCommand,
            session_resource::SessionRealizationLease,
        )>,
    ) -> Result<Option<pc::RepositoryHttpBasicCredential>, RealizationError> {
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
                authority,
            } if repository_id == &repository.plan.repository_id => Some((
                *config_version,
                remote_url.as_str(),
                credential_binding.as_deref(),
                authority.as_deref(),
            )),
            _ => None,
        });
        let Some((config_version, remote_url, credential_binding, authority)) = matching.next()
        else {
            return Err(RealizationError::invariant(
                "protected Repository has no exact binding check",
            ));
        };
        if matching.next().is_some() {
            return Err(RealizationError::invariant(
                "protected Repository has ambiguous binding checks",
            ));
        }
        if let Some(pin) = pin {
            let credential_binding = credential_binding.ok_or_else(|| {
                RealizationError::invariant(
                    "protected Repository credential pin has no authored binding",
                )
            })?;
            pin.validate_for_repository(credential_binding, remote_url)
                .map_err(RealizationError::from)?;
        }
        if repository.plan.source_remote_url != remote_url {
            return Err(RealizationError::invariant(
                "Repository changed its frozen source URL",
            ));
        }
        let publication_authority = publication_fence.map(|(command, lease)| {
            ResourceOperationFence::RepositoryPublication {
                command: Box::new(command.clone()),
                lease: lease.clone(),
            }
        });
        let publication_transport = match (
            publication_authority.as_ref(),
            self.fetches_resources_upstream,
        ) {
            (Some(fence), true) => {
                let verifier = self.repository_binding_verifier.clone().ok_or_else(|| {
                    RealizationError::invariant(
                        "terminal Repository publication has no binding verifier",
                    )
                })?;
                Some(
                    verifier
                        .verify(
                            &self.sessions.workspace(thread, fallback_workspace)?,
                            &repository.plan.repository_id,
                            config_version,
                            Some(fence),
                        )
                        .await
                        .map_err(RealizationError::RepositoryBinding)?,
                )
            }
            // Local topology already compiled this exact command through the
            // frozen terminal projection. It has no Gateway capability to mint
            // and must not re-read the mutable Repository Registry here.
            (Some(_), false) => Some(awaken_resource_contract::RepositoryTransport::Direct),
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
                    Err(RealizationError::invariant(
                        "direct Repository changed its frozen transport URL",
                    ))
                }
                Some(awaken_resource_contract::RepositoryTransport::GatewayMediated { .. }) => {
                    Err(RealizationError::invariant(
                        "Repository has a mediated transport without a credential pin",
                    ))
                }
                None => Ok(None),
            };
        };
        match pin.selected_plaintext_holder.boundary {
            awaken_runtime_contract::PlaintextBoundary::Worker => Err(RealizationError::invariant(
                "Runtime Host or Worker cannot hold a Repository transport credential",
            )),
            awaken_runtime_contract::PlaintextBoundary::Workload => {
                if publication_transport.as_ref().is_some_and(|transport| {
                    !matches!(
                        transport,
                        awaken_resource_contract::RepositoryTransport::Direct
                    )
                }) {
                    return Err(RealizationError::invariant(
                        "Sandbox-held Repository credential cannot use a mediated transport",
                    ));
                }
                if repository.plan.transport_url != remote_url {
                    return Err(RealizationError::invariant(
                        "direct Repository changed its frozen transport URL",
                    ));
                }
                let materializer = self.credential_materializer.as_ref().ok_or_else(|| {
                    RealizationError::invariant(
                        "repository credential requires a configured credential materializer",
                    )
                })?;
                let workspace = self.sessions.workspace(thread, fallback_workspace)?;
                let material = materializer
                    .resolve_for_workspace(
                        &pin.access,
                        &pin.selected_plaintext_holder,
                        awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField,
                        &workspace,
                        &(&repository.plan.repository_id, config_version),
                    )
                    .await
                    .map_err(RealizationError::from)?
                    .material;
                repository_http_basic_credential(material)
                    .map(Some)
                    .map_err(RealizationError::invariant)
            }
            awaken_runtime_contract::PlaintextBoundary::Platform => {
                let workspace = self.sessions.workspace(thread, fallback_workspace)?;
                let transport = match publication_transport {
                    Some(transport) => transport,
                    None => {
                        let verifier =
                            self.repository_binding_verifier.clone().ok_or_else(|| {
                                RealizationError::invariant(
                                    "Gateway-mediated Repository has no binding verifier",
                                )
                            })?;
                        verifier
                            .verify(
                                &workspace,
                                &repository.plan.repository_id,
                                config_version,
                                authority,
                            )
                            .await
                            .map_err(RealizationError::RepositoryBinding)?
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
                        Err(RealizationError::invariant(
                            "Gateway-mediated Repository changed its frozen remote URL",
                        ))
                    }
                    awaken_resource_contract::RepositoryTransport::Direct => {
                        Err(RealizationError::invariant(
                            "Gateway-mediated Repository cannot fall back to direct credentials",
                        ))
                    }
                }
            }
        }
    }

    /// Publish one exact, already-compiled Repository activation. The caller
    /// supplies the activation and binding checks derived from the frozen intent;
    /// this method never selects from the thread's current Resource set and never
    /// loops over unrelated repositories. Credential realization remains the same
    /// operation-scoped edge used by clone and replacement.
    // Keep exact effect coordinates and the deferred Workspace fallback explicit.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_repository_activation(
        &self,
        thread: &str,
        fallback_workspace: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        realizer: &dyn pc::RepositoryRealizer,
        expectation: &pc::RepositoryPublicationExpectation,
        publication_fence: Option<&(
            awaken_session_state_contract::SessionRepositoryPublicationCommand,
            session_resource::SessionRealizationLease,
        )>,
    ) -> Result<pc::RepositoryPublicationReceipt, RepositoryPublicationActivationError> {
        repository
            .plan
            .validate_mount_path()
            .map_err(|error| RepositoryPublicationActivationError::Failed(error.into()))?;
        expectation
            .validate()
            .map_err(|error| RepositoryPublicationActivationError::Failed(error.into()))?;
        if repository.plan.access == pc::MountAccess::ReadOnly {
            return Err(RepositoryPublicationActivationError::Failed(
                RealizationError::invariant("read-only repository cannot be published"),
            ));
        }
        let credential = self
            .repository_operation_credential(
                thread,
                fallback_workspace,
                repository,
                binding_checks,
                publication_fence,
            )
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
                    RepositoryPublicationActivationError::Failed(RealizationError::pending(
                        PendingState::RepositoryPublicationTransportUnavailable,
                        error.to_string(),
                    ))
                }
            })?;
        receipt
            .verify(&repository.plan, expectation)
            .map_err(|error| {
                RepositoryPublicationActivationError::Failed(RealizationError::from(error))
            })?;
        Ok(receipt)
    }

    /// Build the exact neutral Session spec for the provider selected by the
    /// immutable Agent publication. Network projection is provider capability
    /// dependent, so create/adopt/restore must not build with the process-wide
    /// default and then execute on a BackendOwned provider.
    pub fn sandbox_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let resources = self.thread_resources_snapshot(thread)?;
        self.sandbox_spec_for_resources_and_provider(thread, &resources, provider)
    }

    /// Build the provider-selected physical substrate with only mounts that the
    /// provider contract cannot attach later. The Session pending transition is
    /// already durable before this create-time Memory effect; after create, the
    /// V2 handle is persisted before any mutable workspace or Git effect.
    pub fn sandbox_substrate_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let (create_time_mounts, has_repositories) = self
            .sessions
            .read_realization(thread, |r| {
                let create_time_mounts = r
                    .staging
                    .resources
                    .mounts
                    .iter()
                    .filter(|mount| resource_mount_is_create_time(mount))
                    .cloned()
                    .collect();
                let has_repositories = r.staging.transition.as_ref().is_some_and(|transition| {
                    transition.desired().resources.inputs().iter().any(|input| {
                        matches!(
                            input.source,
                            session_resource::ResolvedInputSource::Repository { .. }
                        )
                    })
                });
                (create_time_mounts, has_repositories)
            })?
            .unwrap_or_default();
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            create_time_mounts,
            has_repositories,
            provider.realizer().capabilities().network_isolation,
        )
    }

    pub fn sandbox_spec_for_resources_and_provider(
        &self,
        thread: &str,
        resources: &StagedResources,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            resources.mounts.clone(),
            !resources.repositories.is_empty(),
            provider.realizer().capabilities().network_isolation,
        )
    }

    /// Project a frozen aggregate manifest into the exact create-time provider
    /// substrate without loading any File, Memory, Skill, Vault, or Repository
    /// material. File mounts remain on the canonical late-attach path; cold
    /// adoption retains only typed Memory identity for handle validation.
    pub fn sandbox_spec_for_resolved_resources_and_provider(
        &self,
        thread: &str,
        resources: &session_resource::ResolvedSessionResources,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let environment = self.sessions.frozen(thread)?.environment_projection.clone();
        let mounts = resources
            .inputs()
            .iter()
            .filter_map(|input| {
                crate::managed_input_layout::resolved_input_validation_mount(
                    input,
                    environment.as_ref(),
                )
            })
            .filter(resource_mount_is_create_time)
            .collect();
        let has_repositories = resources.inputs().iter().any(|input| {
            matches!(
                input.source,
                session_resource::ResolvedInputSource::Repository { .. }
            )
        });
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            mounts,
            has_repositories,
            provider.realizer().capabilities().network_isolation,
        )
    }

    fn sandbox_spec_for_mount_layout_with_network(
        &self,
        thread: &str,
        resource_mounts: Vec<pc::MountRequirement>,
        has_repositories: bool,
        network_isolation: bool,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let (baseline, content_delivery, environment) = self
            .sessions
            .modify_session(thread, |slot| {
                (
                    slot.inputs.baseline.clone(),
                    slot.realization.staging.content_delivery,
                    slot.inputs.environment_projection.clone(),
                )
            })?
            .unwrap_or_default();
        self.sandbox_spec_for_projected_layout(
            thread,
            ProjectedSandboxLayout {
                resource_mounts,
                has_repositories,
                baseline_mounts: baseline.as_ref().map(|baseline| baseline.mounts.as_slice()),
                baseline_env: baseline.as_ref().map(|baseline| baseline.env.as_slice()),
                content_delivery,
                environment: environment.as_ref(),
                network_isolation,
            },
        )
    }

    pub fn sandbox_spec_for_projected_layout(
        &self,
        thread: &str,
        projection: ProjectedSandboxLayout<'_>,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let mounts = project_session_mounts(
            projection.resource_mounts,
            projection.baseline_mounts,
            projection.content_delivery,
        );
        let env = projection
            .baseline_env
            .map(<[pc::EnvVar]>::to_vec)
            .map_or_else(|| self.thread_session_env(thread), Ok)?;
        let spec = environment_projection::sandbox_spec_from_projection(
            thread,
            mounts,
            env,
            projection.environment,
            projection.network_isolation,
        );
        Ok(
            if projection.environment.is_some_and(|environment| {
                environment
                    .sandbox
                    .as_ref()
                    .is_some_and(|sandbox| sandbox.host_access == pc::HostAccessPolicy::TrustedHost)
            }) {
                // Preserve the frozen exact trust decision, just as placement does.
                // TrustedHost clients consume AgentFilesystemView, not an asserted
                // sandbox-absolute process namespace. Provider admission still checks
                // every requested capability against the exact Workdir provider.
                spec
            } else {
                pc::session_sandbox_projection(&spec, projection.has_repositories, false).0
            },
        )
    }

    pub fn validate_repository_environment_adoption_paths(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &pc::SandboxSpec,
        repository_paths: &[&str],
        historical_owned_paths: &[&str],
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let effective = provider
            .realizer()
            .effective_spec(spec)
            .map_err(RealizationError::from)?;
        pc::validate_repository_sandbox_adoption_layout(
            repository_paths,
            &effective,
            historical_owned_paths,
        )
        .map_err(RealizationError::from)?;
        Ok(effective)
    }

    /// Stage a thread's resources (mounts + prompt fragments); consumed by
    /// `sandbox_spec` and injected into the run's system prompt. From `prepare_session`.
    /// REPLACES the Thread's set (correct at create time, before its first Run).
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_thread_resources(&self, thread: &str, staged: StagedResources) {
        // A create-time fixture installs the Session it registers.
        self.sessions
            .install_session(thread)
            .and_then(|()| {
                self.sessions
                    .update_realization(thread, |r| r.staging.resources = staged)
            })
            .expect("test fixture registry is healthy");
    }

    pub fn register_thread_resource_manifest(
        &self,
        thread: &str,
        manifest: session_resource::SessionResourceManifest,
    ) -> Result<(), RealizationError> {
        self.sessions
            .update_realization(thread, |r| r.staging.manifest = Some(manifest))?;
        Ok(())
    }

    pub fn thread_resource_manifest(
        &self,
        thread: &str,
    ) -> Result<Option<session_resource::SessionResourceManifest>, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| r.staging.manifest.clone())?
            .flatten())
    }

    pub fn thread_resources_snapshot(
        &self,
        thread: &str,
    ) -> Result<StagedResources, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| r.staging.resources.clone())?
            .unwrap_or_default())
    }

    /// The Repository activations queued for `thread` (test-only observability: a
    /// working tree is realized through its port, not as a byte mount, so it is absent
    /// from `sandbox_spec`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn thread_repository_activations(&self, thread: &str) -> Vec<RepositoryActivation> {
        self.sessions
            .read_realization(thread, |r| r.staging.resources.repositories.clone())
            .expect("test fixture registry is healthy")
            .unwrap_or_default()
    }

    pub fn thread_session_env(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_provisioning_contract::EnvVar>, RealizationError> {
        Ok(self
            .sessions
            .modify_session(thread, |slot| {
                slot.inputs
                    .baseline
                    .as_ref()
                    .map(|baseline| baseline.env.clone())
                    .unwrap_or_default()
            })?
            .unwrap_or_default())
    }
}
