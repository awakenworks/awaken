// Runtime realization of one Session-owned MCP attachment generation.
#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for ManagedHost {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        use awaken_runtime_contract::{CredentialRealizationKind, PlaintextBoundary};

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

        let execution_backend = self
            .host
            .session_slots
            .read(&request.generation.session_id, |slot| {
                slot.backend_ref.clone()
            })
            .flatten()
            .map(|backend_ref| awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref));
        let is_acp = execution_backend
            .as_ref()
            .is_some_and(|backend| backend.is_acp());

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
                    PlaintextBoundary::Platform => {
                        return Err(RunError::classified(
                            "mcp_gateway_provisioner_required",
                            "Platform-held MCP credentials require an external gateway provisioner",
                        ));
                    }
                };
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
                    && realization_kind == CredentialRealizationKind::WorkerRelay
                    && access.policy.model_exposure
                        != awaken_runtime_contract::ModelExposurePolicy::VirtualOnly
                {
                    return Err(RunError::classified(
                        "mcp_model_exposure_forbidden",
                        "authenticated ACP MCP requires explicit VirtualOnly authorization for its generation-scoped relay capability",
                    ));
                }
                if is_acp
                    && realization_kind == CredentialRealizationKind::WorkerRelay
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
                    RunError::unavailable_classified(
                        "mcp_material_source_unavailable",
                        "MCP credential requires a configured material resolver",
                    )
                })?;
                let installed = match realization_kind {
                    CredentialRealizationKind::WorkerRelay => injector.worker_relay_capabilities(),
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
                        crate::host::acp_mcp_client_injection_capabilities(
                            injector,
                            cli,
                            request.target.http_url().is_some(),
                        )
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
                        Some(refresh) => Some(Box::new(crate::mcp::McpRefreshMaterial(
                            self.credential_refresh_factory
                                .as_ref()
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
                        None => self.credential_refresh_factory.as_ref().map(|factory| {
                            Box::new(crate::mcp::McpRefreshMaterial(factory.bearer_reloader(
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
                (Some(bearer), refresh, Some(realization_kind))
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
        let staged_relay =
            if is_acp && actual_realization_kind == Some(CredentialRealizationKind::WorkerRelay) {
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
