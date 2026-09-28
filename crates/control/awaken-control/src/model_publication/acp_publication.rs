use super::*;

impl CatalogModelPublicationResolver {
    pub(super) async fn backend_candidate(
        &self,
        binding: ModelBinding,
        sources: &[CredentialSource],
        model_selection: BackendModelSelection,
        session_configuration: &awaken_runtime_contract::resolved::AcpSessionConfiguration,
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        self.validate_acp_binding(&binding, model_selection)?;
        let mut matches = sources.iter().filter(|source| {
            source.status == CredentialStatus::Active
                && source.kind == CredentialKind::WorkerLocal
                && source.worker_local_binding.as_ref().is_some_and(|local| {
                    local.driver_id == binding.identity().2
                        && (binding.identity().0.is_empty() || binding.identity().0 == source.id.0)
                })
        });
        let source =
            matches
                .next()
                .ok_or_else(|| PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: format!(
                        "no active Worker-local binding is registered for {}",
                        binding.identity().2
                    ),
                })?;
        if matches.next().is_some() {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!(
                    "multiple Worker-local bindings are registered for {}; select an exact identity",
                    binding.identity().2
                ),
            });
        }
        let revision = u64::try_from(source.version)
            .ok()
            .filter(|revision| *revision > 0)
            .ok_or_else(|| PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!(
                    "Worker-local source {} has an invalid revision",
                    source.id.0
                ),
            })?;
        let (_, model, backend) = binding.identity();
        let resolved_binding =
            ModelBinding::parse(source.id.0.as_str(), model, backend).map_err(|error| {
                PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: error.to_string(),
                }
            })?;
        let required_credential = awaken_worker_contract::WorkerCredentialRevision {
            id: source.id.0.clone(),
            revision,
        };
        let (capability_adapter_version, capability_fingerprint, negotiated) = self
            .verified_acp_capability(
                resolved_binding.identity().2,
                Some(&required_credential),
                wall_clock_ms(),
            )
            .await?;
        validate_acp_session_configuration(&resolved_binding, session_configuration, &negotiated)?;
        let error_binding = resolved_binding.clone();
        ResolvedModelCandidate::try_backend_owned(
            resolved_binding,
            CredentialRef {
                id: source.id.0.clone(),
                revision,
            },
            model_selection,
            capability_adapter_version,
            capability_fingerprint,
            session_configuration.clone(),
        )
        .map(|candidate| {
            candidate.with_acp_tool_alias_support(self.acp_capabilities.iter().any(|row| {
                row.backend_ref == error_binding.identity().2 && row.supports_tool_aliases
            }))
        })
        .map_err(|error| PublicationResolutionError::CandidateUnavailable {
            binding: error_binding,
            reason: error.to_string(),
        })
    }

    pub(super) async fn verified_acp_capability(
        &self,
        backend_ref: &str,
        credential: Option<&awaken_worker_contract::WorkerCredentialRevision>,
        now_ms: u64,
    ) -> Result<
        (
            String,
            String,
            awaken_acp_contract::NegotiatedAcpCapabilities,
        ),
        PublicationResolutionError,
    > {
        // The candidate every refusal below names: the Worker-local credential
        // (or none) on this ACP backend. Both coordinates are caller data, so
        // they are admitted once here and reported against their source.
        let candidate = ModelBinding::parse(
            credential.map_or("", |credential| credential.id.as_str()),
            "",
            backend_ref,
        )
        .map_err(|error| {
            PublicationResolutionError::invalid_binding(
                format!("Worker capability lookup for backend `{backend_ref}`"),
                error,
            )
        })?;
        let workers = self.workers.as_ref().ok_or_else(|| {
            PublicationResolutionError::CandidateUnavailable {
                binding: candidate.clone(),
                reason: "live Worker capability observations are unavailable".into(),
            }
        })?;
        let snapshots = workers.list().await.map_err(|error| {
            PublicationResolutionError::CandidateUnavailable {
                binding: candidate.clone(),
                reason: format!("Worker capability observations are unavailable: {error}"),
            }
        })?;
        match awaken_worker_contract::select_verified_acp_capability(
            &snapshots,
            backend_ref,
            credential,
            now_ms,
        ) {
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Available(evidence) => Ok((
                evidence.adapter_version,
                evidence.fingerprint,
                evidence.negotiated,
            )),
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Unavailable => {
                Err(PublicationResolutionError::CandidateUnavailable {
                    binding: candidate,
                    reason: format!(
                        "no fresh verified ACP capability is available for {backend_ref}"
                    ),
                })
            }
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Incompatible => {
                Err(PublicationResolutionError::CandidateUnavailable {
                    binding: candidate,
                    reason: format!(
                        "multiple incompatible ACP capability fingerprints are live for {backend_ref}"
                    ),
                })
            }
        }
    }
}

pub(super) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
