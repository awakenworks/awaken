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

use std::sync::{Arc, Weak};

use awaken_credential_materializer::{
    CredentialRefreshFactory, PinnedCredentialMaterializer, PlatformMcpEgressProvisioner,
};
use awaken_session_error_contract::RunError;

use crate::mcp::credential_target::exact_credential_realization_target;
use crate::mcp::error::run_error;
use crate::mcp::{McpGenerationAuthority, McpSessionSlots, McpSessions};

fn validate_stdio_credential_binding(
    target: &awaken_session_mcp_contract::McpTarget,
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
    /// Whether the exact generation is authorized at `now_unix_ms`.
    fn mcp_generation_is_authorized_at(
        &self,
        generation: &awaken_session_mcp_contract::McpGenerationRef,
        now_unix_ms: u64,
    ) -> bool;

    /// The authority a newly started Worker relay admits routes by.
    fn relay_authority(&self) -> Weak<dyn McpGenerationAuthority>;

    /// The execution backend frozen for the Session, when projected.
    fn execution_backend(
        &self,
        session: &str,
    ) -> Option<awaken_runtime_contract::resolved::Backend>;

    /// Whether the Session's Environment provider enforces secret substitution
    /// with no-bypass networking.
    fn supports_secret_egress_without_bypass(&self, session: &str) -> Result<bool, RunError>;

    /// The Session's frozen Environment network policy, when projected.
    fn frozen_network(&self, session: &str) -> Option<awaken_provisioning_contract::NetworkPolicy>;

    /// The installed process-private client-injection capabilities of one
    /// ACP adapter, or `None` when the adapter cannot consume them.
    fn acp_client_injection_capabilities(
        &self,
        materializer: &PinnedCredentialMaterializer,
        cli: &str,
        http_transport: bool,
    ) -> Result<Option<awaken_runtime_contract::CredentialRealizationCapabilities>, RunError>;

    /// The Session's resident Environment, if one is realized.
    async fn resident_environment(
        &self,
        session: &str,
    ) -> Option<Arc<awaken_session_environment_contract::SessionEnvironment>>;

    /// The Agent frozen for the Session.
    fn frozen_agent(&self, session: &str) -> Option<String>;

    /// Realize the Session's Environment for `agent`.
    async fn realize_environment(&self, session: &str, agent: &str) -> Result<(), String>;
}

/// The credential materialization a staging call may use. The Managed
/// adapter owns these; staging borrows them for one call.
#[derive(Clone, Copy, Default)]
pub struct McpCredentialMaterialization<'a> {
    pub credentials: Option<&'a PinnedCredentialMaterializer>,
    pub refresh_factory: Option<&'a dyn CredentialRefreshFactory>,
    pub platform_egress: Option<&'a dyn PlatformMcpEgressProvisioner>,
}

// Runtime realization of one Session-owned MCP attachment generation.
impl McpSessions {
    pub async fn stage_attachment<S: McpSessionSlots>(
        &self,
        slots: &S,
        host: &dyn McpStagingSession,
        credentials: McpCredentialMaterialization<'_>,
        request: awaken_session_mcp_contract::StageMcpAttachment,
    ) -> Result<awaken_session_mcp_contract::McpRealizationReceipt, RunError> {
        use awaken_runtime_contract::{CredentialRealizationKind, PlaintextBoundary};

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
            .unwrap_or(true)
        {
            return Err(RunError::unavailable_classified(
                "session_environment_quiescing",
                "MCP staging is closed while the Session Environment quiesces",
            ));
        }
        let now_unix_ms = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        if !host.mcp_generation_is_authorized_at(&request.generation, now_unix_ms) {
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
        if let Some(existing) = self.projection(slots, &request.generation) {
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
        self.forget_replayable_removed_projection(slots, &request);
        match self.renew_projection(slots, &request) {
            Ok(Some(receipt)) => return Ok(receipt),
            Ok(None) => {}
            Err(error) => {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    error.to_string(),
                ));
            }
        }

        let execution_backend = host.execution_backend(request.generation.session_id().as_str());
        let is_acp = execution_backend
            .as_ref()
            .is_some_and(|backend| backend.is_acp());

        let sandbox_stdio = request.target.sandbox_stdio_target().cloned();
        validate_stdio_credential_binding(
            &request.target,
            request.credential.is_some(),
            request.selected_plaintext_holder.is_some(),
        )?;

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
                    && mediated
                    && access.policy.model_exposure
                        != awaken_runtime_contract::ModelExposurePolicy::VirtualOnly
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
                            .and_then(|url| {
                                awaken_session_mcp_contract::McpTarget::identity(url).ok()
                            })
                            .map(|identity| identity.host)
                            .ok_or_else(|| {
                                RunError::classified(
                                    "mcp_relay_target_invalid",
                                    "Worker relay requires one canonical HTTP MCP target",
                                )
                            })?;
                        let network = host.frozen_network(request.generation.session_id().as_str());
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
                            relay.matches_exposure(exposure) && relay.is_live()
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
                    let upstream = request.target.http_url().ok_or_else(|| {
                        RunError::classified(
                            "mcp_relay_target_invalid",
                            "a mediated MCP credential requires one canonical HTTP MCP target",
                        )
                    })?;
                    let route = provisioner
                        .provision(awaken_credential_materializer::PlatformMcpEgressRequest {
                            holder,
                            credential: &access.credential,
                            workspace_id: &request.workspace_id,
                            session_id: request.generation.session_id().as_str(),
                            attachment_id: &request.generation.attachment_id().0,
                            generation: request.generation.generation().version(),
                            runtime_incarnation: request.generation.runtime_incarnation(),
                            lease_epoch: request.generation.lease_epoch().get(),
                            upstream_url: upstream,
                        })
                        .await
                        .map_err(|error| {
                            RunError::unavailable_classified(
                                "mcp_gateway_route_unavailable",
                                format!(
                                    "mcp server `{}` mediated route is unavailable: {error}",
                                    request.name
                                ),
                            )
                        })?;
                    // Nothing is resolved here. The Host is a caller of the
                    // mediator, never a holder of this credential.
                    (None, None, Some(realization_kind), Some(route))
                } else {
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
                            awaken_runtime_contract::CredentialMaterialError::Unavailable => {
                                RunError::unavailable_classified(
                                    "mcp_material_source_unavailable",
                                    format!(
                                        "mcp server `{}` credential material is temporarily unavailable",
                                        request.name
                                    ),
                                )
                            }
                            error => RunError::classified(
                                "mcp_credential_revision_mismatch",
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
        let projection_request = request.clone();
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
        let receipt = awaken_session_mcp_contract::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder.clone(),
            actual_realization_kind,
            receipt_fingerprint: request_fingerprint,
        };
        let (native_wiring, mcp_process, projection_already_inserted) = if is_acp {
            (None, None, false)
        } else if let Some(target) = sandbox_stdio.as_ref() {
            // A spawned MCP helper must have the one installed cleanup owner.
            // Refuse before realizing an Environment or entering Staging when
            // a Host was assembled without that process authority.
            let process_reaper = self.process_reaper().map_err(run_error)?;
            let session_id = &request.generation.session_id();
            let environment = match host.resident_environment(session_id.as_str()).await {
                Some(environment) => environment,
                None => {
                    let agent_id = host.frozen_agent(session_id.as_str()).ok_or_else(|| {
                            RunError::classified(
                                "mcp_sandbox_unavailable",
                                "sandbox stdio MCP requires a frozen Session Agent before Environment realization",
                            )
                        })?;
                    host.realize_environment(session_id.as_str(), &agent_id)
                        .await
                        .map_err(|error| {
                            RunError::classified(
                                "mcp_sandbox_unavailable",
                                format!(
                                    "sandbox stdio MCP could not realize its Session Environment: {error}"
                                ),
                            )
                        })?;
                    let realized = host.resident_environment(session_id.as_str()).await;
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
            let staging = crate::mcp_material::McpStagingActivity::default();
            self.insert_projection(
                slots,
                crate::mcp_material::McpGenerationProjection {
                    request: projection_request.clone(),
                    receipt: receipt.clone(),
                    server: Some(server.clone()),
                    native_wiring: None,
                    mcp_process: None,
                    staging: Some(staging.clone()),
                    drain: Arc::new(tokio::sync::Mutex::new(())),
                    state: crate::mcp_material::McpProjectionState::Staging,
                },
            )
            .map_err(run_error)?;
            let sessions = self.clone();
            let task_slots = slots.clone();
            let generation = request.generation.clone();
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
            // Create the guard before constructing/spawning the Future. Tokio
            // may drop a spawned task during shutdown before its first poll;
            // keeping the guard in the Future's captured state makes that drop
            // finish the already-installed Staging owner as well.
            let staging_activity = staging.guard();
            let staging_server = server.clone();
            let stage = tokio::spawn(async move {
                let _staging_activity = staging_activity;
                let (process, channel) =
                    match environment.agent_process().spawn_agent(command).await {
                        Ok(spawned) => spawned,
                        Err(error) => {
                            staging.finish();
                            let cleanup = sessions.drain_projection(&task_slots, &generation).await;
                            if let Err(cleanup) = cleanup {
                                return Err(run_error(cleanup));
                            }
                            return Err(RunError::classified(
                                "mcp_sandbox_spawn_failed",
                                format!("sandbox stdio MCP process could not start: {error}"),
                            ));
                        }
                    };
                let process: Arc<dyn awaken_provisioning_contract::ProcessHandle> =
                    Arc::from(process);
                match sessions.attach_staging_process(&task_slots, &generation, process.clone()) {
                    Ok(true) => {}
                    Ok(false) => {
                        // A concurrent canonical drain owns this tracked process
                        // and is waiting on the staging activity barrier.
                        staging.finish();
                        return Err(RunError::unavailable_classified(
                            "mcp_generation_draining",
                            "MCP generation began draining while its process was spawning",
                        ));
                    }
                    Err(owner_error) => {
                        let reap = process_reaper
                            .reap(process.as_ref(), std::time::Duration::from_secs(5))
                            .await;
                        staging.finish();
                        if let Err(error) = reap {
                            return Err(RunError::unavailable_classified(
                                "mcp_generation_process_reap_failed",
                                format!(
                                    "unadopted MCP process could not be reaped after owner transition: {error}"
                                ),
                            ));
                        }
                        return Err(run_error(owner_error));
                    }
                }
                match crate::mcp::connect_sandbox_stdio(&staging_server, channel).await {
                    Ok(wiring) => {
                        let completed = sessions.complete_staging(&task_slots, &generation, wiring);
                        staging.finish();
                        completed.map_err(run_error)
                    }
                    Err(error) => {
                        staging.finish();
                        sessions
                            .drain_projection(&task_slots, &generation)
                            .await
                            .map_err(run_error)?;
                        Err(run_error(error))
                    }
                }
            });
            match stage.await {
                Ok(result) => result?,
                Err(error) => {
                    return Err(RunError::unavailable_classified(
                        "mcp_staging_task_failed",
                        format!("owned MCP staging task did not complete: {error}"),
                    ));
                }
            }
            (None, None, true)
        } else {
            (
                Some(
                    crate::mcp::connect_materialized(std::slice::from_ref(&server))
                        .await
                        .map_err(run_error)?,
                ),
                None,
                false,
            )
        };
        // Staging is the sole route-creation boundary.  Runtime construction is
        // a projection reader and must never repair or recreate credential-
        // bearing effects behind the durable realization protocol's back.
        let staged_relay =
            if is_acp && actual_realization_kind == Some(CredentialRealizationKind::WorkerRelay) {
                let relay = self
                    .relay_or_start(host.relay_authority())
                    .await
                    .map_err(|error| {
                        RunError::classified(
                            "mcp_relay_unavailable",
                            format!("could not stage Worker-held MCP route: {error}"),
                        )
                    })?;
                if !relay.is_live() {
                    return Err(RunError::unavailable_classified(
                        "mcp_relay_unavailable",
                        "Worker MCP relay transport is no longer serving",
                    ));
                }
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
        if !projection_already_inserted
            && let Err(error) = self.insert_projection(
                slots,
                crate::mcp_material::McpGenerationProjection {
                    request: projection_request,
                    receipt: receipt.clone(),
                    server: Some(server),
                    native_wiring,
                    mcp_process,
                    staging: None,
                    drain: Arc::new(tokio::sync::Mutex::new(())),
                    state: crate::mcp_material::McpProjectionState::Staged,
                },
            )
        {
            // A route is private and not yet visible, but retaining its bearer
            // after the exact projection failed to stage would still be a leak.
            if let Some(relay) = staged_relay {
                relay.remove_route(&request.generation);
            }
            return Err(run_error(error));
        }
        Ok(receipt)
    }

    pub async fn publish_attachment<S: McpSessionSlots>(
        &self,
        slots: &S,
        host: &dyn McpStagingSession,
        generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        let now_unix_ms = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        if !host.mcp_generation_is_authorized_at(&generation, now_unix_ms) {
            return Err(RunError::classified(
                "mcp_stale_ownership",
                "MCP publication lease has expired",
            ));
        }
        self.publish_projection(slots, &generation)
            .await
            .map_err(run_error)
    }

    pub async fn drain_attachment<S: McpSessionSlots>(
        &self,
        slots: &S,
        generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.drain_projection(slots, &generation)
            .await
            .map_err(run_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_requires_an_explicit_secret_environment_binding_before_credentials() {
        // Cause/effect graph: C1 transport is stdio, C2 an exact credential is
        // present, C3 a plaintext holder is selected. Effect E1 reject with the
        // stable unsupported code before materialization; otherwise E2 admit.
        // Complete decision table: only C1 && (C2 || C3) produces E1. HTTP may
        // carry either credential shape, and anonymous stdio remains supported.
        let http =
            awaken_session_mcp_contract::McpTarget::parse_http("https://mcp.example.test/mcp")
                .unwrap();
        let stdio = awaken_session_mcp_contract::McpTarget::sandbox_stdio(
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
