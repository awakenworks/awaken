use super::*;

impl CatalogModelPublicationResolver {
    pub(super) async fn backend_candidate(
        &self,
        binding: ModelBinding,
        sources: &[CredentialSource],
        model_selection: BackendModelSelection,
        session_configuration: &awaken_runtime_contract::resolved::AcpSessionConfiguration,
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        Self::validate_acp_binding(&binding, model_selection)?;
        let mut matches = sources.iter().filter(|source| {
            source.status == CredentialStatus::Active
                && source.kind == CredentialKind::WorkerLocal
                && source.worker_local_binding.as_ref().is_some_and(|local| {
                    local.driver_id == binding.backend_ref
                        && (binding.provider_identity_ref.is_empty()
                            || binding.provider_identity_ref == source.id.0)
                })
        });
        let source =
            matches
                .next()
                .ok_or_else(|| PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: format!(
                        "no active Worker-local binding is registered for {}",
                        binding.backend_ref
                    ),
                })?;
        if matches.next().is_some() {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!(
                    "multiple Worker-local bindings are registered for {}; select an exact identity",
                    binding.backend_ref
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
        let mut resolved_binding = binding;
        resolved_binding
            .provider_identity_ref
            .clone_from(&source.id.0);
        let required_credential = awaken_worker_registry::WorkerCredentialRevision {
            id: source.id.0.clone(),
            revision,
        };
        let (capability_adapter_version, capability_fingerprint, negotiated) = self
            .verified_acp_capability(
                &resolved_binding.backend_ref,
                Some(&required_credential),
                wall_clock_ms(),
            )
            .await?;
        validate_acp_session_configuration(&resolved_binding, session_configuration, &negotiated)?;
        Ok(ResolvedModelCandidate::backend_owned(
            resolved_binding,
            CredentialRef {
                id: source.id.0.clone(),
                revision,
            },
            model_selection,
            capability_adapter_version,
            capability_fingerprint,
            session_configuration.clone(),
        ))
    }

    pub(super) async fn verified_acp_capability(
        &self,
        backend_ref: &str,
        credential: Option<&awaken_worker_registry::WorkerCredentialRevision>,
        now_ms: u64,
    ) -> Result<
        (
            String,
            String,
            awaken_acp_contract::NegotiatedAcpCapabilities,
        ),
        PublicationResolutionError,
    > {
        let workers = self.workers.as_ref().ok_or_else(|| {
            PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(
                    credential.map_or("", |credential| credential.id.as_str()),
                    "",
                    backend_ref,
                ),
                reason: "live Worker capability observations are unavailable".into(),
            }
        })?;
        let snapshots = workers.list().await.map_err(|error| {
            PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(
                    credential.map_or("", |credential| credential.id.as_str()),
                    "",
                    backend_ref,
                ),
                reason: format!("Worker capability observations are unavailable: {error}"),
            }
        })?;
        let mut fingerprints = snapshots
            .into_iter()
            .map(|registered| registered.snapshot)
            .filter(|worker| {
                worker.state.accepts_work()
                    && worker.expires_at_ms > now_ms
                    && credential.is_none_or(|credential| {
                        worker
                            .credential_observations
                            .iter()
                            .any(|observation| observation.is_selectable_at(credential, now_ms))
                    })
            })
            .flat_map(|worker| worker.acp_capability_observations)
            .filter(|capability| {
                capability.valid_until_ms > now_ms
                    && capability.observation.observed_at_ms <= now_ms
                    && capability.observation.backend_ref == backend_ref
                    && capability.observation.state
                        == awaken_acp_contract::AcpCapabilityObservationState::Verified
            })
            .filter_map(|capability| {
                capability.observation.fingerprint.and_then(|fingerprint| {
                    capability.observation.negotiated.map(|negotiated| {
                        (
                            capability.observation.adapter_version,
                            fingerprint,
                            negotiated,
                        )
                    })
                })
            })
            .collect::<Vec<_>>();
        fingerprints.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        fingerprints.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
        match (fingerprints.pop(), fingerprints.is_empty()) {
            (Some((version, fingerprint, negotiated)), true)
                if !version.trim().is_empty() && !fingerprint.trim().is_empty() =>
            {
                Ok((version, fingerprint, negotiated))
            }
            (None, _) => Err(PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(
                    credential.map_or("", |credential| credential.id.as_str()),
                    "",
                    backend_ref,
                ),
                reason: format!("no fresh verified ACP capability is available for {backend_ref}"),
            }),
            _ => Err(PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(
                    credential.map_or("", |credential| credential.id.as_str()),
                    "",
                    backend_ref,
                ),
                reason: format!(
                    "multiple incompatible ACP capability fingerprints are live for {backend_ref}"
                ),
            }),
        }
    }
}

pub(super) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
