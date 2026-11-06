// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Static owner: managed MCP attachment realization at the runtime host. Dynamic path: the
// @formal reason: claimed Session attachment is resolved into the single MCP transport used by the
// @formal reason: executing Run and released with that scope. Evidence: protocol.mcp.mcp_protocol and
// @formal reason: managed MCP Session tests; this is a product-requirement classification, not a direct
// @formal reason: proof claim.
//! Staging, publication and drain of one Session-owned MCP attachment
//! generation: credential admission and materialization, the sandbox-stdio
//! process owner, and the Worker relay route. The Managed adapter implements
//! the realizer port by delegating here.

use awaken_agent_contract::credential::{CredentialUsage, ModelExposurePolicy};
use awaken_credential_contract::CredentialAccessAdmission;
use std::sync::{Arc, Weak};

use awaken_credential_contract::CredentialRefreshFactory;
use awaken_credential_materializer::{PinnedCredentialMaterializer, PlatformMcpEgressProvisioner};
use awaken_runtime_contract::runs::RunError;

use crate::mcp::credential_target::exact_credential_realization_target;
use crate::mcp::error::run_error;
use crate::mcp::{McpGenerationAuthority, McpSessions, SessionMcpStates};

async fn validate_platform_reference(
    injector: &PinnedCredentialMaterializer,
    access: &awaken_agent_contract::credential::CredentialAccess,
    holder: &awaken_agent_contract::credential::PlaintextHolder,
    workspace: &str,
    target: &awaken_agent_contract::McpTarget,
) -> Result<(), RunError> {
    injector
        .validate_reference_for_workspace(
            access,
            holder,
            awaken_credential_contract::CredentialRealizationKind::PlatformRelay,
            workspace,
            exact_credential_realization_target(target),
        )
        .await
        .map_err(|error| {
            if error == awaken_credential_contract::CredentialMaterialError::Unavailable {
                RunError::unavailable_classified(
                    "mcp_material_source_unavailable",
                    "MCP credential source cannot be validated",
                )
            } else {
                RunError::classified(
                    credential_rejection_code(&error),
                    format!("MCP credential reference was refused: {error}"),
                )
            }
        })
}

fn validate_stdio_credential_binding(
    target: &awaken_agent_contract::McpTarget,
    has_credential: bool,
    has_plaintext_holder: bool,
) -> Result<(), RunError> {
    if target.sandbox_stdio_target().is_some() && (has_credential || has_plaintext_holder) {
        return Err(RunError::classified(
            "mcp_stdio_credential_unsupported",
            "sandbox stdio MCP credentials require an explicit secret-environment binding; HTTP bearer credentials cannot be projected into a process",
        ));
    }
    Ok(())
}

/// What staging reads from the Session it stages into. The Runtime Host owns
/// these facts (backend, Environment, frozen network, root authority); MCP
/// staging only consumes them.
#[async_trait::async_trait]
pub trait McpStagingSession: Send + Sync {
    /// Whether the exact generation is authorized at `now_unix_ms`; an
    /// unreadable root authority is refused as an error.
    fn mcp_generation_is_authorized_at(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        now_unix_ms: u64,
    ) -> Result<bool, RunError>;

    /// The authority a newly started Worker relay admits routes by.
    fn relay_authority(&self) -> Weak<dyn McpGenerationAuthority>;

    /// The execution backend frozen for the Session, when projected; an
    /// unreadable installed-Session registry is refused, never absent.
    fn execution_backend(
        &self,
        session: &str,
    ) -> Result<Option<awaken_runtime_contract::resolved::Backend>, RunError>;

    /// Whether the Session's Environment provider enforces secret substitution
    /// with no-bypass networking.
    fn supports_secret_egress_without_bypass(&self, session: &str) -> Result<bool, RunError>;

    /// The Session's frozen Environment network policy, when projected; an
    /// unreadable installed-Session registry is refused, never absent.
    fn frozen_network(
        &self,
        session: &str,
    ) -> Result<Option<awaken_provisioning_contract::NetworkPolicy>, RunError>;

    /// The installed process-private client-injection capabilities of one
    /// ACP adapter, or `None` when the adapter cannot consume them.
    fn acp_client_injection_capabilities(
        &self,
        materializer: &PinnedCredentialMaterializer,
        cli: &str,
        http_transport: bool,
    ) -> Result<Option<awaken_credential_contract::CredentialRealizationCapabilities>, RunError>;

    /// The Session's resident Environment, if one is realized. An unreadable
    /// installed-Session registry is refused, never reported as absent.
    async fn resident_environment(
        &self,
        session: &str,
    ) -> Result<Option<Arc<awaken_session_environment_contract::SessionEnvironment>>, RunError>;

    /// The Agent frozen for the Session; an unreadable registry is refused.
    fn frozen_agent(&self, session: &str) -> Result<Option<String>, RunError>;

    /// Realize the Session's Environment for `agent`.
    async fn realize_environment(&self, session: &str, agent: &str) -> Result<(), RunError>;
}

/// The credential materialization a staging call may use. The Managed
/// adapter owns these; staging borrows them for one call.
#[derive(Clone, Copy, Default)]
pub struct McpCredentialMaterialization<'a> {
    pub credentials: Option<&'a PinnedCredentialMaterializer>,
    pub refresh_factory: Option<&'a dyn CredentialRefreshFactory>,
    pub platform_egress: Option<&'a dyn PlatformMcpEgressProvisioner>,
}

mod staging;
use staging::{McpStagingOwner, StagingAdmission};

#[cfg(test)]
mod preparation_tests;

// Runtime realization of one Session-owned MCP attachment generation.
impl McpSessions {
    /// Stage through the SDK direct-assertion entrance. An installed root still
    /// overrides that assertion; service assignments use stage_assigned_attachment.
    pub async fn stage_attachment<S: SessionMcpStates, C: McpStagingSession + 'static>(
        &self,
        slots: &Arc<S>,
        host: Arc<C>,
        credentials: for<'a> fn(&'a C) -> McpCredentialMaterialization<'a>,
        request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError> {
        self.stage_owned(slots, host, credentials, request, StagingAdmission::Direct)
            .await
    }

    /// Reject unknown service-assigned roots and recheck root admission under
    /// the canonical lock. Every branch reserves its original preparation
    /// allocation before external I/O and rechecks it before the next effect.
    /// Platform route enforcement remains the mediator's scoped responsibility.
    /// Assigned calls never acquire the SDK direct-assertion fallback.
    pub async fn stage_assigned_attachment<S: SessionMcpStates, C: McpStagingSession + 'static>(
        &self,
        slots: &Arc<S>,
        host: Arc<C>,
        credentials: for<'a> fn(&'a C) -> McpCredentialMaterialization<'a>,
        request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError> {
        self.stage_owned(
            slots,
            host,
            credentials,
            request,
            StagingAdmission::Assigned,
        )
        .await
    }

    /// The operation owns its original inputs across caller cancellation. A
    /// dropped waiter never aborts prepare I/O or releases its drain barrier.
    async fn stage_owned<S: SessionMcpStates, C: McpStagingSession + 'static>(
        &self,
        slots: &Arc<S>,
        host: Arc<C>,
        credentials: for<'a> fn(&'a C) -> McpCredentialMaterialization<'a>,
        request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
        admission: StagingAdmission,
    ) -> Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError> {
        let sessions = self.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let mut platform_preparation = None;
            let result = sessions
                .stage_with_admission(
                    &slots,
                    host.as_ref(),
                    credentials(host.as_ref()),
                    request,
                    admission,
                    &mut platform_preparation,
                )
                .await;
            drop(host);
            if let Some(preparation) = platform_preparation {
                let completion = preparation.finish(slots).await;
                return result.and_then(|receipt| completion.map(|()| receipt));
            }
            result
        })
        .await
        .map_err(|error| {
            RunError::unavailable_classified(
                "mcp_staging_task_failed",
                format!("owned MCP preparation did not complete: {error}"),
            )
        })?
    }

    async fn stage_with_admission<S: SessionMcpStates>(
        &self,
        slots: &Arc<S>,
        host: &dyn McpStagingSession,
        credentials: McpCredentialMaterialization<'_>,
        request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
        admission: StagingAdmission,
        platform_preparation: &mut Option<staging::PlatformMcpPreparation>,
    ) -> Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError> {
        use awaken_agent_contract::credential::PlaintextBoundary;
        use awaken_credential_contract::CredentialRealizationKind;

        if request.workspace_id.trim().is_empty()
            || request.generation.session_id().as_str().trim().is_empty()
            || request.realization_id.trim().is_empty()
            || request.stage_idempotency_key.trim().is_empty()
            || request.name.trim().is_empty()
            || request.target.display_target().trim().is_empty()
        {
            return Err(RunError::bad_request(
                "MCP realization request is incomplete",
            ));
        }
        if !slots
            .read_mcp(request.generation.session_id().as_str(), |mcp| {
                mcp.admits_realization()
            })
            .map_err(run_error)?
            .unwrap_or(true)
        {
            return Err(RunError::unavailable_classified(
                "session_environment_quiescing",
                "MCP staging is closed while the Session Environment quiesces",
            ));
        }
        let now_unix_ms = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        let root = slots
            .modify_mcp_with_root(&request.generation, |_mcp, root| root)
            .map_err(run_error)?
            .flatten();
        if !host.mcp_generation_is_authorized_at(&request.generation, now_unix_ms)?
            || !admission.admits(root, &request.generation, now_unix_ms)
        {
            return Err(RunError::classified(
                "mcp_stale_ownership",
                "MCP realization lease has expired",
            ));
        }
        // `prompts_as_skills` is a lazy semantic registry/executor, not either
        // admitted Managed filesystem source: exact attached version bytes or a
        // scan-once Skill under an already-realized Repository mount. Reject the
        // parallel authority before creating or replaying any MCP realization.
        // Direct MCP callers retain their compatibility adapter through the
        // ordinary MCP wiring path.
        if request.prompts_as_skills {
            return Err(RunError::classified(
                "mcp_prompt_skills_unsupported",
                "Managed Sessions accept only attached versioned Skills or fixed repository filesystem Skills; MCP prompts-as-skills are direct-session compatibility only",
            ));
        }
        let request_fingerprint = request.fingerprint();
        if let Some(existing) = self
            .projection(slots.as_ref(), &request.generation)
            .map_err(run_error)?
        {
            let exact_replay = existing.request.realization_id == request.realization_id
                && existing.request.stage_idempotency_key == request.stage_idempotency_key
                && existing.receipt.receipt_fingerprint == request_fingerprint;
            if exact_replay {
                match existing.state {
                    crate::mcp_material::McpProjectionState::Staged
                    | crate::mcp_material::McpProjectionState::Active => {
                        return Ok(existing.receipt);
                    }
                    crate::mcp_material::McpProjectionState::Staging => {
                        return Err(RunError::unavailable_classified(
                            "mcp_generation_staging",
                            "MCP generation staging is still owned by an earlier attempt",
                        ));
                    }
                    crate::mcp_material::McpProjectionState::Draining => {
                        return Err(RunError::unavailable_classified(
                            "mcp_generation_draining",
                            "MCP generation cleanup has not completed",
                        ));
                    }
                    crate::mcp_material::McpProjectionState::Removed => {}
                }
            } else {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    "MCP generation is already bound to another realization",
                ));
            }
        }
        // A completed drain has no effect left to renew. Both exact retry and
        // same-binding lease catch-up re-enter ordinary staging through this
        // single tombstone owner; live/Staging/Draining projections stay fenced.
        self.forget_replayable_removed_projection(slots.as_ref(), &request)
            .map_err(run_error)?;
        match self.renew_projection(slots.as_ref(), &request) {
            Ok(Some(receipt)) => return Ok(receipt),
            Ok(None) => {}
            Err(error) => {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    error.to_string(),
                ));
            }
        }

        let execution_backend = host.execution_backend(request.generation.session_id().as_str())?;
        let is_acp = execution_backend
            .as_ref()
            .is_some_and(|backend| backend.is_acp());

        let sandbox_stdio = request.target.sandbox_stdio_target().cloned();
        validate_stdio_credential_binding(
            &request.target,
            request.credential.is_some(),
            request.selected_plaintext_holder.is_some(),
        )?;

        let process_reaper = if sandbox_stdio.is_some() && !is_acp {
            Some(self.process_reaper().map_err(run_error)?)
        } else {
            None
        };
        let mut staging = None;
        let (bearer, refresh, actual_realization_kind, mediated_route) = match (
            request.credential.as_ref(),
            request.selected_plaintext_holder.as_ref(),
        ) {
            (None, None) => (None, None, None, None),
            (Some(access), Some(holder)) => {
                let realization_kind = match holder.boundary {
                    PlaintextBoundary::Worker => CredentialRealizationKind::WorkerRelay,
                    PlaintextBoundary::Workload if is_acp => {
                        CredentialRealizationKind::ProcessProtocolField
                    }
                    PlaintextBoundary::Workload => {
                        return Err(RunError::classified(
                            "mcp_holder_unsupported",
                            "Workload-held MCP credentials require an ACP backend",
                        ));
                    }
                    // The deployment's egress mediator performs this call.
                    // The Host learns a route and an expiry, never material.
                    PlaintextBoundary::Platform => CredentialRealizationKind::PlatformRelay,
                };
                // Both relay kinds hand the workload a generation-scoped
                // synthetic route rather than upstream material, so they share
                // one exposure and one no-bypass requirement.
                let mediated = matches!(
                    realization_kind,
                    CredentialRealizationKind::WorkerRelay
                        | CredentialRealizationKind::PlatformRelay
                );
                if realization_kind == CredentialRealizationKind::ProcessProtocolField
                    && access.refresh.is_some()
                {
                    return Err(RunError::classified(
                        "mcp_client_refresh_unsupported",
                        "ACP process-protocol MCP credentials require a new Session generation; dynamic refresh cannot be silently discarded",
                    ));
                }
                match &access.usage {
                    CredentialUsage::HttpHeader { name, scheme }
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
                    && mediated
                    && access.policy.model_exposure != ModelExposurePolicy::VirtualOnly
                {
                    return Err(RunError::classified(
                        "mcp_model_exposure_forbidden",
                        "authenticated ACP MCP requires explicit VirtualOnly authorization for its generation-scoped relay capability",
                    ));
                }
                if is_acp && mediated {
                    if !host.supports_secret_egress_without_bypass(
                        request.generation.session_id().as_str(),
                    )? {
                        return Err(RunError::classified(
                            "mcp_holder_unsupported",
                            "authenticated ACP MCP requires provider-enforced secret substitution and no-bypass networking before mediated relay materialization",
                        ));
                    }
                    // The advertised-origin allowlist below describes the
                    // Host's own relay. A platform mediator is reached through
                    // the deployment's egress boundary instead.
                    if let Some(exposure) = self
                        .relay_exposure()
                        .filter(|_| realization_kind == CredentialRealizationKind::WorkerRelay)
                    {
                        let upstream_host = request
                            .target
                            .http_url()
                            .and_then(|url| awaken_agent_contract::McpTarget::identity(url).ok())
                            .map(|identity| identity.host)
                            .ok_or_else(|| {
                                RunError::classified(
                                    "mcp_relay_target_invalid",
                                    "Worker relay requires one canonical HTTP MCP target",
                                )
                            })?;
                        let network =
                            host.frozen_network(request.generation.session_id().as_str())?;
                        if !network
                            .as_ref()
                            .is_some_and(|network| exposure.admitted_by(network, &upstream_host))
                        {
                            return Err(RunError::classified(
                                "mcp_relay_network_not_frozen",
                                "an external authenticated ACP MCP relay requires the frozen Environment allowlist to contain its advertised origin and exclude the exact upstream target",
                            ));
                        }
                        if !self.relay.get().is_some_and(|relay| {
                            relay.matches_exposure(exposure) && relay.is_accepting()
                        }) {
                            return Err(RunError::unavailable_classified(
                                "mcp_relay_unavailable",
                                "an external authenticated ACP MCP relay must be bound before Worker Ready",
                            ));
                        }
                    }
                }
                let injector = credentials.credentials.ok_or_else(|| {
                    RunError::unavailable_classified(
                        "mcp_material_source_unavailable",
                        "MCP credential requires a configured material resolver",
                    )
                })?;
                let installed = match realization_kind {
                    CredentialRealizationKind::WorkerRelay => injector.worker_relay_capabilities(),
                    CredentialRealizationKind::PlatformRelay => {
                        injector.platform_relay_capabilities(holder.trust_domain.0.clone())
                    }
                    CredentialRealizationKind::ProcessProtocolField => {
                        let Some(awaken_runtime_contract::resolved::Backend::Acp(backend)) =
                            execution_backend.as_ref()
                        else {
                            return Err(RunError::classified(
                                "mcp_holder_unsupported",
                                "Workload-held MCP credentials require an ACP backend",
                            ));
                        };
                        let cli = backend.cli();
                        host.acp_client_injection_capabilities(
                            injector,
                            cli,
                            request.target.http_url().is_some(),
                        )?
                        .ok_or_else(|| {
                            RunError::classified(
                                "mcp_client_injection_unsupported",
                                format!(
                                    "ACP adapter `{cli}` cannot consume this MCP credential through its process-private channel"
                                ),
                            )
                        })?
                    }
                    _ => {
                        return Err(RunError::classified(
                            "mcp_holder_unsupported",
                            "MCP credential has no installed realization mechanism",
                        ));
                    }
                };
                access
                    .admit(holder, realization_kind, &installed, now_unix_ms)
                    .map_err(|error| {
                        RunError::classified("mcp_credential_admission", error.to_string())
                    })?;
                if realization_kind == CredentialRealizationKind::PlatformRelay {
                    let provisioner = credentials.platform_egress.ok_or_else(|| {
                        RunError::unavailable_classified(
                            "mcp_gateway_provisioner_required",
                            "Platform-held MCP credentials require an installed egress mediator",
                        )
                    })?;
                    if request.target.http_url().is_none() {
                        return Err(RunError::classified(
                            "mcp_relay_target_invalid",
                            "a mediated MCP credential requires one canonical HTTP MCP target",
                        ));
                    }
                    // Reference-only admission must not resolve Platform-held
                    // plaintext into the Host just to check current revocation.
                    validate_platform_reference(
                        injector,
                        access,
                        holder,
                        &request.workspace_id,
                        &request.target,
                    )
                    .await?;
                    staging = Some(McpStagingOwner::begin(
                        slots,
                        &request,
                        admission,
                        self.clock.as_ref(),
                    )?);
                    let route = staging
                        .as_mut()
                        .expect("Platform preparation has its original owner")
                        .provision_platform(
                            provisioner,
                            admission,
                            self.clock.as_ref(),
                            platform_preparation,
                        )
                        .await?;
                    // Route creation can outlive a credential change. On refusal
                    // the existing staging owner retains/drains this allocation;
                    // no server projection can be published from the old read.
                    validate_platform_reference(
                        injector,
                        access,
                        holder,
                        &request.workspace_id,
                        &request.target,
                    )
                    .await?;
                    // Nothing is resolved here. The Host is a caller of the
                    // mediator, never a holder of this credential.
                    (None, None, Some(realization_kind), Some(route))
                } else {
                    staging = Some(McpStagingOwner::begin(
                        slots,
                        &request,
                        admission,
                        self.clock.as_ref(),
                    )?);
                    let bearer = injector
                        .resolve_for_workspace(
                            access,
                            holder,
                            realization_kind,
                            &request.workspace_id,
                            exact_credential_realization_target(&request.target),
                        )
                        .await
                        .map_err(|error| match error {
                            awaken_credential_contract::CredentialMaterialError::Unavailable => {
                                RunError::unavailable_classified(
                                    "mcp_material_source_unavailable",
                                    format!(
                                        "mcp server `{}` credential material is temporarily unavailable",
                                        request.name
                                    ),
                                )
                            }
                            error => RunError::classified(
                                credential_rejection_code(&error),
                                format!(
                                    "mcp server `{}` credential could not be resolved exactly: {error}",
                                    request.name
                                ),
                            ),
                        })?
                        .material
                        .into_secret()
                        .map_err(|error| {
                            RunError::classified(
                                "mcp_credential_material_kind_mismatch",
                                error.to_string(),
                            )
                        })?;
                    let refresh = if realization_kind == CredentialRealizationKind::WorkerRelay {
                        match access.refresh.as_ref() {
                            Some(refresh) => Some(Box::new(crate::mcp_material::McpRefreshMaterial(
                                credentials
                                    .refresh_factory
                                    .ok_or_else(|| {
                                        RunError::unavailable_classified(
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
                            None => credentials.refresh_factory.map(|factory| {
                                Box::new(crate::mcp_material::McpRefreshMaterial(factory.bearer_reloader(
                                    awaken_credential_contract::CredentialSourceId(
                                        access.credential.id.clone(),
                                    ),
                                    access.credential.revision,
                                )))
                            }),
                        }
                    } else {
                        None
                    };
                    (Some(bearer), refresh, Some(realization_kind), None)
                }
            }
            _ => {
                return Err(RunError::classified(
                    "mcp_credential_binding_invalid",
                    "MCP credential and selected plaintext holder must be present together",
                ));
            }
        };
        let mut staging = match staging {
            Some(owner) => owner,
            None => McpStagingOwner::begin(slots, &request, admission, self.clock.as_ref())?,
        };
        let server = crate::mcp_material::McpTransportMaterial {
            name: request.name,
            prompts_as_skills: request.prompts_as_skills,
            transport: match sandbox_stdio.as_ref() {
                Some(target) => crate::mcp_material::McpTransportMaterialKind::SandboxStdio {
                    command: target.command.clone(),
                    args: target.args.clone(),
                },
                None => crate::mcp_material::McpTransportMaterialKind::Http {
                    // A mediated credential points the client at the mediator.
                    // Reaching upstream is the mediator's business, and the
                    // client holds nothing it could replay there.
                    url: match &mediated_route {
                        Some(route) => route.url.clone(),
                        None => request
                            .target
                            .http_url()
                            .expect("non-stdio MCP target must be HTTP")
                            .to_string(),
                    },
                    bearer,
                    refresh,
                },
            },
        };
        let receipt = awaken_session_configuration_contract::mcp::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder.clone(),
            actual_realization_kind,
            receipt_fingerprint: request_fingerprint,
        };
        staging.authorize(admission, self.clock.as_ref())?;
        if let Some(target) = sandbox_stdio.as_ref().filter(|_| !is_acp) {
            let process_reaper =
                process_reaper.expect("stdio preflight selected its process owner");
            let session_id = &request.generation.session_id();
            let environment = match host.resident_environment(session_id.as_str()).await? {
                Some(environment) => environment,
                None => {
                    let agent_id = host.frozen_agent(session_id.as_str())?.ok_or_else(|| {
                            RunError::classified(
                                "mcp_sandbox_unavailable",
                                "sandbox stdio MCP requires a frozen Session Agent before Environment realization",
                            )
                        })?;
                    staging.authorize(admission, self.clock.as_ref())?;
                    host.realize_environment(session_id.as_str(), &agent_id)
                        .await?;
                    let realized = host.resident_environment(session_id.as_str()).await?;
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
                environment
                    .agent_process()
                    .workspace_cwd()
                    .trim_end_matches('/'),
                mcp_home_logical
            );
            let home_sentinel = if environment.agent_process().supports_host_identity() {
                format!("{mcp_home_logical}/.awaken-mcp-home")
            } else {
                format!("{mcp_home}/.awaken-mcp-home")
            };
            staging.authorize(admission, self.clock.as_ref())?;
            environment
                .agent_process()
                .materialize_inline(&home_sentinel, b"")
                .await
                .map_err(|error| {
                    RunError::classified(
                        "mcp_sandbox_home_failed",
                        format!("sandbox stdio MCP home could not be materialized: {error}"),
                    )
                })?;
            staging.prepare_stdio(admission, self.clock.as_ref(), server.clone(), &receipt)?;
            let command = awaken_provisioning_contract::Command {
                argv,
                cwd: environment.agent_process().workspace_cwd(),
                env: vec![awaken_provisioning_contract::EnvVar {
                    name: "HOME".into(),
                    value: awaken_provisioning_contract::EnvValue::Inline { value: mcp_home },
                    visibility: awaken_provisioning_contract::EnvVisibility::Process,
                }],
                stdio: awaken_provisioning_contract::Stdio::Piped,
            };
            // Move the already-installed owner into the task before its first poll.
            // Dropping the caller's JoinHandle cannot orphan a spawned process.
            let stage = tokio::spawn(self.clone().stage_sandbox_stdio(
                slots.clone(),
                environment,
                command,
                staging,
                admission,
                process_reaper,
            ));
            match stage.await {
                Ok(result) => result?,
                Err(error) => {
                    return Err(RunError::unavailable_classified(
                        "mcp_staging_task_failed",
                        format!("owned MCP staging task did not complete: {error}"),
                    ));
                }
            }
            return Ok(receipt);
        }
        let native_wiring = if is_acp {
            None
        } else {
            staging.authorize(admission, self.clock.as_ref())?;
            Some(
                crate::mcp::connect_materialized(std::slice::from_ref(&server))
                    .await
                    .map_err(run_error)?,
            )
        };
        let relay =
            if is_acp && actual_realization_kind == Some(CredentialRealizationKind::WorkerRelay) {
                staging.authorize(admission, self.clock.as_ref())?;
                let relay = self
                    .relay_or_start(host.relay_authority())
                    .await
                    .map_err(|error| {
                        RunError::unavailable_classified(
                            "mcp_relay_unavailable",
                            format!("could not stage Worker-held MCP route: {error}"),
                        )
                    })?;
                if !relay.is_accepting() {
                    return Err(RunError::unavailable_classified(
                        "mcp_relay_unavailable",
                        "Worker MCP relay transport is no longer serving",
                    ));
                }
                Some(relay)
            } else {
                None
            };
        if native_wiring.is_some()
            && actual_realization_kind == Some(CredentialRealizationKind::PlatformRelay)
        {
            // Native initialization is another external wait after provision.
            // Recheck the same metadata authority before publishing its wiring;
            // ACP Platform routes have no intervening wait and keep the earlier
            // check. The original staging owner still owns refused allocations.
            validate_platform_reference(
                credentials
                    .credentials
                    .expect("Platform staging admitted its materializer"),
                request
                    .credential
                    .as_ref()
                    .expect("Platform staging admitted its reference"),
                request
                    .selected_plaintext_holder
                    .as_ref()
                    .expect("Platform staging admitted its holder"),
                &request.workspace_id,
                &request.target,
            )
            .await?;
        }
        staging.publish(
            admission,
            self.clock.as_ref(),
            relay,
            server,
            native_wiring,
            &receipt,
        )?;
        Ok(receipt)
    }

    async fn stage_sandbox_stdio<S: SessionMcpStates>(
        self,
        task_slots: Arc<S>,
        environment: Arc<awaken_session_environment_contract::SessionEnvironment>,
        command: awaken_provisioning_contract::Command,
        mut owner: McpStagingOwner<S>,
        admission: StagingAdmission,
        process_reaper: Arc<dyn awaken_session_environment_contract::ProcessReaper>,
    ) -> Result<(), RunError> {
        let staging_server = owner
            .selection()
            .server
            .clone()
            .expect("owned stdio projection has transport");
        let receipt = owner.selection().receipt.clone();
        owner.authorize(admission, self.clock.as_ref())?;
        let (process, channel) = match environment.agent_process().spawn_agent(command).await {
            Ok(spawned) => spawned,
            Err(error) => {
                owner.finish_activity();
                let cleanup = self
                    .drain_selected_projection::<S>(&task_slots, owner.selection())
                    .await;
                if let Err(cleanup) = cleanup {
                    return Err(run_error(cleanup));
                }
                return Err(RunError::classified(
                    "mcp_sandbox_spawn_failed",
                    format!("sandbox stdio MCP process could not start: {error}"),
                ));
            }
        };
        let process: Arc<dyn awaken_provisioning_contract::ProcessHandle> = Arc::from(process);
        match owner.attach_process(process.clone(), admission, self.clock.as_ref()) {
            Ok(true) => {}
            Ok(false) => {
                // A concurrent canonical drain owns this tracked process
                // and is waiting on the staging activity barrier.
                owner.finish_activity();
                return Err(RunError::unavailable_classified(
                    "mcp_generation_draining",
                    "MCP generation began draining while its process was spawning",
                ));
            }
            Err(owner_error) => {
                let reap = process_reaper
                    .reap(process.as_ref(), std::time::Duration::from_secs(5))
                    .await;
                owner.finish_activity();
                if let Err(error) = reap {
                    return Err(RunError::unavailable_classified(
                        "mcp_generation_process_reap_failed",
                        format!(
                            "unadopted MCP process could not be reaped after owner transition: {error}"
                        ),
                    ));
                }
                return Err(owner_error);
            }
        }
        match crate::mcp::connect_sandbox_stdio(&staging_server, channel).await {
            Ok(wiring) => {
                let completed = owner.publish(
                    admission,
                    self.clock.as_ref(),
                    None,
                    staging_server,
                    Some(wiring),
                    &receipt,
                );
                owner.finish_activity();
                if completed.is_err() {
                    self.drain_selected_projection::<S>(&task_slots, owner.selection())
                        .await
                        .map_err(run_error)?;
                }
                completed
            }
            Err(error) => {
                owner.finish_activity();
                self.drain_selected_projection::<S>(&task_slots, owner.selection())
                    .await
                    .map_err(run_error)?;
                Err(run_error(error))
            }
        }
    }

    pub async fn publish_attachment<S: SessionMcpStates>(
        &self,
        slots: &S,
        host: &dyn McpStagingSession,
        generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        let now_unix_ms = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        if !host.mcp_generation_is_authorized_at(&generation, now_unix_ms)? {
            return Err(RunError::classified(
                "mcp_stale_ownership",
                "MCP publication lease has expired",
            ));
        }
        self.publish_projection(slots, &generation)
            .await
            .map_err(run_error)
    }

    pub async fn drain_attachment<S: SessionMcpStates>(
        &self,
        slots: &S,
        generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.drain_projection(slots, &generation)
            .await
            .map_err(run_error)
    }
}

/// Stable class of an exact-credential refusal; a revision change, a foreign
/// recipient and every other binding refusal stay distinguishable.
fn credential_rejection_code(
    error: &awaken_credential_contract::CredentialMaterialError,
) -> &'static str {
    use awaken_credential_contract::CredentialMaterialError as Material;
    match error {
        Material::RevisionMismatch => "mcp_credential_revision_mismatch",
        Material::RecipientMismatch => "mcp_credential_recipient_mismatch",
        _ => "mcp_credential_binding_rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn staging_guard_finishes_when_future_is_dropped_before_first_poll() {
        // Cause/effect rule G1: the original activity is pending, its guard is
        // captured by value before a Future is polled, then that Future drops.
        // Effect: the same activity becomes complete without executing a body
        // or creating a process. This is the guard primitive used by the owned
        // staging phase; Host S1-S5 separately cover actual spawn/drain effects.
        let activity = crate::mcp_material::McpStagingActivity::default();
        let completion = activity.wait();
        let mut completion = std::pin::pin!(completion);
        assert!(futures_util::poll!(completion.as_mut()).is_pending());
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered_body = entered.clone();
        let guard = activity.guard();
        let future = async move {
            let _guard = guard;
            entered_body.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<()>().await;
        };
        drop(future);
        assert!(!entered.load(std::sync::atomic::Ordering::SeqCst));
        assert!(futures_util::poll!(completion.as_mut()).is_ready());
    }

    #[test]
    fn stdio_requires_an_explicit_secret_environment_binding_before_credentials() {
        // Cause/effect graph: C1 transport is stdio, C2 an exact credential is
        // present, C3 a plaintext holder is selected. Effect E1 reject with the
        // stable unsupported code before materialization; otherwise E2 admit.
        // Complete decision table: only C1 && (C2 || C3) produces E1. HTTP may
        // carry either credential shape, and anonymous stdio remains supported.
        let http =
            awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test/mcp").unwrap();
        let stdio = awaken_agent_contract::McpTarget::sandbox_stdio(
            "/usr/bin/node",
            vec!["/mnt/session/uploads/server.js".into()],
        )
        .unwrap();
        for (target, is_stdio) in [(&http, false), (&stdio, true)] {
            for has_credential in [false, true] {
                for has_holder in [false, true] {
                    let result =
                        validate_stdio_credential_binding(target, has_credential, has_holder);
                    let rejected = is_stdio && (has_credential || has_holder);
                    assert_eq!(result.is_err(), rejected);
                    if let Err(error) = result {
                        assert_eq!(error.code, "mcp_stdio_credential_unsupported");
                    }
                }
            }
        }
    }
}
