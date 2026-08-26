//! Host-side provisioning for a launched ACP CLI: the [`LaunchResolver`] that turns
//! publication-pinned inference access into concrete launch inputs. Managed
//! candidates project their exact endpoint and claimed vault credential;
//! backend-owned candidates project only their model policy and leave account,
//! endpoint, refresh, and login material to the local CLI.

use awaken_run_executor_acp::{
    AcpCli, ConfigHome, LaunchResolver, OpenError, ProcessSecretRequirement, ResolvedModel,
};
#[cfg(test)]
use awaken_runtime_contract::CredentialRealizationKind;
use awaken_runtime_contract::CredentialUsage;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{
    BackendModelSelection, ModelProvisioning, ResolvedModelCandidate,
};
use std::path::PathBuf;
use std::sync::Arc;

enum AcpCredentialAuthority {
    Local(crate::PinnedCredentialMaterializer),
    Brokered(Arc<dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>),
    BackendOwned,
}

/// Resolves an ACP run from its published access and a worker-side exact credential
/// materializer. This adapter cannot query the model catalog or select a credential.
pub struct PublishedAcpLaunchResolver {
    cli: AcpCli,
    store_dir: Option<PathBuf>,
    credentials: AcpCredentialAuthority,
}

impl PublishedAcpLaunchResolver {
    #[must_use]
    pub fn new(
        cli: AcpCli,
        store_dir: Option<PathBuf>,
        credentials: crate::PinnedCredentialMaterializer,
    ) -> Self {
        Self {
            cli,
            store_dir,
            credentials: AcpCredentialAuthority::Local(credentials),
        }
    }

    /// Install one image-backed backend-owned ACP route without inventing a
    /// credential source. Provider-backed candidates remain fail-closed.
    #[must_use]
    pub fn backend_owned(cli: AcpCli, store_dir: Option<PathBuf>) -> Self {
        Self {
            cli,
            store_dir,
            credentials: AcpCredentialAuthority::BackendOwned,
        }
    }

    /// Install the hosted, attempt-time Gateway grant materializer. It owns no
    /// Provider plaintext; its paired broker consumes only one-shot lease refs.
    #[must_use]
    pub fn brokered(
        cli: AcpCli,
        store_dir: Option<PathBuf>,
        materializer: Arc<dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>,
    ) -> Self {
        Self {
            cli,
            store_dir,
            credentials: AcpCredentialAuthority::Brokered(materializer),
        }
    }

    fn candidate<'a>(
        &self,
        activation: &'a RunActivation,
    ) -> Result<&'a ResolvedModelCandidate, OpenError> {
        let model_ref = activation.effective_model_ref();
        activation
            .snapshot
            .resolved_spec
            .candidate_for_model(model_ref)
            .ok_or_else(|| {
                OpenError(format!(
                    "model {model_ref} is outside the publication-pinned candidate set"
                ))
            })
    }

    async fn resolve_model(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<ResolvedModel, OpenError> {
        let model_ref = activation.effective_model_ref();
        let candidate = self.candidate(activation)?;
        let ModelProvisioning::Provider {
            endpoint,
            credential,
            acp,
            ..
        } = candidate.provisioning()
        else {
            if let ModelProvisioning::BackendOwned {
                model_selection,
                acp,
                ..
            } = candidate.provisioning()
            {
                let coherent = match model_selection {
                    BackendModelSelection::Default => candidate.binding().model_ref.is_empty(),
                    BackendModelSelection::Exact => {
                        !candidate.binding().model_ref.trim().is_empty()
                    }
                };
                if !coherent {
                    return Err(OpenError(
                        "published backend model policy is incoherent".into(),
                    ));
                }
                if acp.capability_adapter_version.trim().is_empty()
                    || acp.capability_fingerprint.trim().is_empty()
                {
                    return Err(OpenError(
                        "published backend capability pin is missing".into(),
                    ));
                }
                return Ok(ResolvedModel::backend_owned(
                    *model_selection,
                    candidate.binding().model_ref.clone(),
                    self.cli.id,
                    &acp.capability_adapter_version,
                    &acp.capability_fingerprint,
                    acp.session_configuration.clone(),
                ));
            }
            return Err(OpenError(format!(
                "published model {model_ref} has no provider endpoint"
            )));
        };
        let endpoint = endpoint.clone();
        if endpoint.base_url.trim().is_empty() || endpoint.upstream_model.trim().is_empty() {
            return Err(OpenError(format!(
                "published model {model_ref} has incomplete endpoint coordinates"
            )));
        }
        let credentials = match &self.credentials {
            AcpCredentialAuthority::Brokered(materializer) => {
                return materializer
                    .materialize(self.cli, activation, context)
                    .await;
            }
            AcpCredentialAuthority::Local(credentials) => credentials,
            AcpCredentialAuthority::BackendOwned => {
                return Err(OpenError(
                    "credential_realization_unavailable: provider-backed ACP requires an installed exact materializer"
                        .into(),
                ));
            }
        };
        let credential_artifact = credentials
            .plan_claimed_credential_artifact(
                candidate,
                context,
                self.cli.managed_credential_delivery,
            )
            .map_err(OpenError)?;
        let process_secret = if credential_artifact.is_some()
            || !self.cli.managed_credential_delivery.allows_process_secret()
        {
            None
        } else {
            credentials
                .plan_claimed_process_secret(candidate, context)
                .map_err(OpenError)?
                .map(
                    |reference| match credential.as_ref().map(|access| &access.usage) {
                        Some(CredentialUsage::EnvironmentVariable { name }) => {
                            ProcessSecretRequirement::for_environment(reference, name)
                        }
                        _ => ProcessSecretRequirement::new(reference),
                    },
                )
        };
        if credential.is_some() && credential_artifact.is_none() && process_secret.is_none() {
            return Err(OpenError(
                "credential_realization_kind_unsupported: selected ACP credential cannot be provisioned"
                    .into(),
            ));
        }
        Ok(match acp {
            Some(acp) => ResolvedModel::managed_with_acp(
                endpoint.base_url,
                endpoint.upstream_model,
                process_secret,
                credential_artifact,
                acp.as_ref().clone(),
            ),
            None => ResolvedModel::managed(
                endpoint.base_url,
                endpoint.upstream_model,
                process_secret,
                credential_artifact,
            ),
        })
    }

    /// Open the exact thread config home. Failure is terminal: allowing the CLI
    /// to discover its default home would bypass claim-pinned configuration.
    fn config_home_env(&self, thread_id: &str) -> Result<Vec<(String, String)>, OpenError> {
        let home = ConfigHome::open(self.store_dir.as_deref(), thread_id)
            .map_err(|error| OpenError(format!("local_config_home_unavailable: {error}")))?;
        let Some(config_home_env) = self.cli.config_home_env else {
            return Ok(Vec::new());
        };
        Ok(vec![(
            config_home_env.to_string(),
            home.root().display().to_string(),
        )])
    }
}

#[async_trait::async_trait]
impl LaunchResolver for PublishedAcpLaunchResolver {
    async fn model(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<ResolvedModel, OpenError> {
        self.resolve_model(activation, context).await
    }

    fn extra_env(&self, activation: &RunActivation) -> Result<Vec<(String, String)>, OpenError> {
        match self.candidate(activation)?.provisioning() {
            ModelProvisioning::BackendOwned { .. } => Ok(Vec::new()),
            ModelProvisioning::Provider { .. } => self.config_home_env(&activation.thread_id.0),
            ModelProvisioning::Remote { .. } => Err(OpenError(
                "remote model provisioning cannot be projected as ACP".into(),
            )),
            ModelProvisioning::HostExecutor => Err(OpenError(
                "host-executor model cannot be projected as ACP".into(),
            )),
        }
    }

    fn secret_broker(
        &self,
    ) -> Option<std::sync::Arc<dyn awaken_provisioning_contract::SecretBroker>> {
        match &self.credentials {
            AcpCredentialAuthority::Local(credentials) => {
                Some(std::sync::Arc::new(credentials.clone()))
            }
            AcpCredentialAuthority::Brokered(materializer) => Some(materializer.secret_broker()),
            AcpCredentialAuthority::BackendOwned => None,
        }
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let credentials = match &self.credentials {
            AcpCredentialAuthority::Local(credentials) => credentials,
            AcpCredentialAuthority::Brokered(materializer) => {
                return materializer.credential_realization_capabilities(self.cli);
            }
            AcpCredentialAuthority::BackendOwned => return Default::default(),
        };
        let (material_sources, recipient_bound_envelopes) =
            credentials.material_source_capabilities();
        let (realization_kind, material_type) = match self.cli.managed_credential_delivery {
            awaken_run_executor_acp::ManagedCredentialDelivery::ProcessSecret => (
                awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment,
                awaken_runtime_contract::credential::PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE,
            ),
            awaken_run_executor_acp::ManagedCredentialDelivery::Artifact(_) => (
                awaken_runtime_contract::CredentialRealizationKind::PrivateSecretFile,
                awaken_runtime_contract::credential::PRIVATE_SECRET_FILE_MATERIAL_TYPE,
            ),
        };
        awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources,
            realization_kinds: [realization_kind].into_iter().collect(),
            recipient_bound_envelopes,
            extension_consumers: [(
                format!(
                    "{}acp:{}",
                    awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX,
                    self.cli.id
                ),
                [material_type.to_string()].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            alternatives: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialError, CredentialKind, InMemorySecretStore, SecretRef,
        SecretStore,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_runtime_contract::{
        AttemptCredentialBinding, AttemptCredentialRealization, AttemptOwnershipError,
        AttemptOwnershipVerifier, CredentialAccess, CredentialExecutionPolicy,
        CredentialMaterialSource, CredentialRealizationReceipt, CredentialRealizationRecordError,
        CredentialRealizationRecorder, CredentialRef, CredentialUsage, InferenceEndpoint,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CurrentOwnership;

    #[async_trait::async_trait]
    impl AttemptOwnershipVerifier for CurrentOwnership {
        async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
            Ok(())
        }
    }

    struct AcceptReceipt;

    #[async_trait::async_trait]
    impl CredentialRealizationRecorder for AcceptReceipt {
        async fn record(
            &self,
            _receipt: CredentialRealizationReceipt,
        ) -> Result<(), CredentialRealizationRecordError> {
            Ok(())
        }
    }

    struct CountingSecrets {
        inner: InMemorySecretStore,
        gets: AtomicUsize,
        fail_get: AtomicBool,
    }

    impl CountingSecrets {
        fn new(fail_get: bool) -> Self {
            Self {
                inner: InMemorySecretStore::new(),
                gets: AtomicUsize::new(0),
                fail_get: AtomicBool::new(fail_get),
            }
        }
    }

    #[async_trait::async_trait]
    impl SecretStore for CountingSecrets {
        async fn put(
            &self,
            reference: &SecretRef,
            secret: RedactedString,
        ) -> Result<(), CredentialError> {
            self.inner.put(reference, secret).await
        }

        async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            if self.fail_get.load(Ordering::SeqCst) {
                return Err(CredentialError::Storage(
                    "planned unavailable material".into(),
                ));
            }
            self.inner.get(reference).await
        }

        async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
            self.inner.delete(reference).await
        }
    }

    struct DecisionOwnership(bool);

    #[async_trait::async_trait]
    impl AttemptOwnershipVerifier for DecisionOwnership {
        async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
            self.0.then_some(()).ok_or(AttemptOwnershipError::Lost)
        }
    }

    struct DecisionRecorder {
        accepts: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl CredentialRealizationRecorder for DecisionRecorder {
        async fn record(
            &self,
            _receipt: CredentialRealizationReceipt,
        ) -> Result<(), CredentialRealizationRecordError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.accepts
                .then_some(())
                .ok_or_else(|| CredentialRealizationRecordError("planned receipt rejection".into()))
        }
    }

    fn attempt_context(
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
    ) -> awaken_runtime_contract::RuntimeRunContext {
        attempt_context_with(
            candidate,
            true,
            CredentialRealizationKind::ProcessSecretEnvironment,
            Arc::new(CurrentOwnership),
            Arc::new(AcceptReceipt),
        )
    }

    fn test_acp_profile() -> awaken_runtime_contract::resolved::AcpExecutionProfile {
        awaken_runtime_contract::resolved::AcpExecutionProfile {
            capability_fingerprint: "sha256:test-capability".into(),
            capability_adapter_version: "test".into(),
            session_configuration: Default::default(),
        }
    }

    fn attempt_context_with(
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        include_binding: bool,
        kind: CredentialRealizationKind,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
        recorder: Arc<dyn CredentialRealizationRecorder>,
    ) -> awaken_runtime_contract::RuntimeRunContext {
        let credential = match candidate.provisioning() {
            ModelProvisioning::Provider {
                credential: Some(access),
                ..
            } => access.credential.clone(),
            _ => panic!("test candidate must carry a credential"),
        };
        let binding = AttemptCredentialBinding {
            candidate_fingerprint: awaken_runtime_contract::candidate_fingerprint(candidate)
                .unwrap(),
            credential,
            selected_plaintext_holder: awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            ),
            selected_realization_kind: kind,
            claim_epoch: 1,
        };
        awaken_runtime_contract::RuntimeRunContext::new()
            .with_ownership(ownership)
            .with_credential_realization(AttemptCredentialRealization::new(
                include_binding.then_some(binding).into_iter().collect(),
                recorder,
            ))
    }

    fn claude() -> AcpCli {
        *awaken_run_executor_acp::acp_cli("claude").unwrap()
    }

    struct BrokeredLeaseMaterializer {
        calls: Arc<AtomicUsize>,
        capabilities: awaken_runtime_contract::CredentialRealizationCapabilities,
    }

    struct BrokeredLeaseStore;

    #[async_trait::async_trait]
    impl awaken_provisioning_contract::SecretBroker for BrokeredLeaseStore {
        async fn materialize(
            &self,
            _reference: &str,
        ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
            Err(awaken_provisioning_contract::SandboxError::new(
                "hosted ACP leases are process-only",
            ))
        }

        async fn materialize_process(
            &self,
            reference: &str,
        ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
            (reference == "lease://hosted-attempt")
                .then(|| b"short-lived-lease".to_vec())
                .ok_or_else(|| awaken_provisioning_contract::SandboxError::new("unknown lease"))
        }

        async fn write_back(
            &self,
            _reference: &str,
            _bytes: Vec<u8>,
        ) -> Result<(), awaken_provisioning_contract::SandboxError> {
            Err(awaken_provisioning_contract::SandboxError::new(
                "hosted ACP leases are read-only",
            ))
        }
    }

    #[async_trait::async_trait]
    impl awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer for BrokeredLeaseMaterializer {
        async fn materialize(
            &self,
            cli: AcpCli,
            activation: &RunActivation,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<ResolvedModel, OpenError> {
            assert_eq!(cli.id, "claude");
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ResolvedModel::cloud_managed_gateway(
                "https://gateway.example/v1",
                activation.effective_model_ref(),
                "lease://hosted-attempt",
            ))
        }

        fn secret_broker(&self) -> Arc<dyn awaken_provisioning_contract::SecretBroker> {
            Arc::new(BrokeredLeaseStore)
        }

        fn credential_realization_capabilities(
            &self,
            _cli: AcpCli,
        ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
            self.capabilities.clone()
        }
    }

    fn activation(
        model: Option<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
    ) -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("th".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    delegation_limits: Default::default(),
                    model_binding: model.unwrap_or_else(|| {
                        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                            ModelBinding::new("p", "published-model", "acp:claude"),
                        )
                    }),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: Vec::new(),
            delegation_origin: None,
            model_ref_override: None,
            data_subject_id: None,
            tool_capability_narrowing: Default::default(),
        }
    }

    #[test]
    fn acp_realization_capabilities_follow_the_selected_cli_delivery_contract() {
        // C1 process-secret CLI -> only process-secret admission. C2 artifact
        // CLI -> only private-file admission. Advertising their union makes
        // the claim solver choose a mechanism the selected CLI cannot consume.
        for (cli, expected, unexpected) in [
            (
                claude(),
                CredentialRealizationKind::ProcessSecretEnvironment,
                CredentialRealizationKind::PrivateSecretFile,
            ),
            (
                *awaken_run_executor_acp::acp_cli("codex").unwrap(),
                CredentialRealizationKind::PrivateSecretFile,
                CredentialRealizationKind::ProcessSecretEnvironment,
            ),
        ] {
            let resolver = PublishedAcpLaunchResolver::new(
                cli,
                None,
                crate::PinnedCredentialMaterializer::new(
                    Arc::new(InMemoryCredentialRepo::new()),
                    Arc::new(InMemorySecretStore::new()),
                ),
            );
            let capabilities = resolver.credential_realization_capabilities();
            assert!(
                capabilities.realization_kinds.contains(&expected),
                "{}",
                cli.id
            );
            assert!(
                !capabilities.realization_kinds.contains(&unexpected),
                "{}",
                cli.id
            );
            assert_eq!(
                capabilities
                    .acp_backend_realization_kind(&format!("acp:{}", cli.id))
                    .expect("coherent backend mapping"),
                Some(expected),
                "{}",
                cli.id
            );
        }
    }

    #[tokio::test]
    async fn brokered_authority_is_used_only_for_provider_candidates() {
        // Cause/effect graph: Provider + brokered authority -> one attempt-time
        // grant and one opaque lease requirement; BackendOwned -> pinned local
        // backend policy with no grant. Capabilities and the one-shot broker are
        // delegated to the same authority, so launch and admission cannot drift.
        //
        // Decision table:
        // | Rule | candidate | authority | grant calls | result |
        // | H1 | Provider | brokered | 1 | gateway URL + opaque lease |
        // | H2 | BackendOwned | brokered | 0 | backend-owned model policy |
        let calls = Arc::new(AtomicUsize::new(0));
        let capabilities = awaken_runtime_contract::CredentialRealizationCapabilities {
            realization_kinds: [CredentialRealizationKind::PlatformProviderAdapter]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let authority = Arc::new(BrokeredLeaseMaterializer {
            calls: calls.clone(),
            capabilities: capabilities.clone(),
        });
        let resolver = PublishedAcpLaunchResolver::brokered(claude(), None, authority);

        let provider =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                ModelBinding::new("anthropic", "published-model", "acp:claude"),
                "anthropic@1",
                "anthropic-messages@1",
                "ws",
                Some(CredentialAccess::new(
                    CredentialRef {
                        id: "provider-credential".into(),
                        revision: 7,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::self_hosted_provider(),
                )),
                InferenceEndpoint {
                    adapter_kind: "anthropic".into(),
                    api_dialect: "anthropic_messages".into(),
                    base_url: "https://provider.example/v1".into(),
                    upstream_model: "published-model".into(),
                    processing_placement: None,
                },
                test_acp_profile(),
            )
            .expect("coherent brokered ACP provider candidate");
        let resolved = resolver
            .model(
                &activation(Some(provider)),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("H1 brokered provider grant");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "H1");
        let ResolvedModel::Managed {
            base_url,
            process_secret,
            ..
        } = resolved
        else {
            panic!("H1 must project managed gateway access")
        };
        assert_eq!(base_url, "https://gateway.example/v1", "H1");
        let reference = process_secret.expect("H1 lease requirement");
        assert_eq!(reference.reference(), "lease://hosted-attempt", "H1");
        assert_eq!(
            resolver
                .secret_broker()
                .expect("H1 broker")
                .materialize_process(reference.reference())
                .await
                .expect("H1 lease material"),
            b"short-lived-lease"
        );
        assert_eq!(resolver.credential_realization_capabilities(), capabilities);

        let backend = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("local", "codex-model", "acp:claude"),
            CredentialRef {
                id: "backend-owned".into(),
                revision: 1,
            },
            BackendModelSelection::Exact,
            "test",
            "sha256:test-capability",
            Default::default(),
        )
        .expect("coherent backend-owned candidate");
        assert!(matches!(
            resolver
                .model(
                    &activation(Some(backend)),
                    &awaken_runtime_contract::RuntimeRunContext::new(),
                )
                .await
                .expect("H2 backend-owned projection"),
            ResolvedModel::BackendOwned { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "H2");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolves_only_snapshot_endpoint_and_persisted_credential() {
        let repo = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("persisted-key")),
                oauth_command: None,
            },
            secrets.as_ref(),
            repo.as_ref(),
        );
        let source = source.await.unwrap();
        let model =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                ModelBinding::new("anthropic", "published-model", "acp:claude"),
                "anthropic@1",
                "anthropic-messages@1",
                "ws",
                Some(CredentialAccess::new(
                    CredentialRef {
                        id: source.id.0,
                        revision: 1,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::self_hosted_provider(),
                )),
                InferenceEndpoint {
                    adapter_kind: "anthropic".into(),
                    api_dialect: "anthropic_messages".into(),
                    base_url: "https://db.example/v1".into(),
                    upstream_model: "upstream-model".into(),
                    processing_placement: None,
                },
                test_acp_profile(),
            )
            .expect("coherent persisted ACP provider candidate");
        let resolver = PublishedAcpLaunchResolver::new(
            claude(),
            None,
            crate::PinnedCredentialMaterializer::new(repo, secrets),
        );
        let context = attempt_context(&model);
        let resolved = resolver
            .model(&activation(Some(model.clone())), &context)
            .await
            .unwrap();
        let ResolvedModel::Managed {
            base_url,
            model,
            process_secret,
            ..
        } = &resolved
        else {
            panic!("provider candidate must resolve as managed")
        };
        assert_eq!(base_url, "https://db.example/v1");
        assert_eq!(model, "upstream-model");
        let reference = process_secret
            .as_ref()
            .expect("credential-bearing model has process requirement")
            .reference();
        assert!(!reference.contains("persisted-key"));
        assert!(!format!("{resolved:?}").contains(reference));
        let broker = resolver.secret_broker().unwrap();
        assert_eq!(
            broker.materialize_process(reference).await.unwrap(),
            b"persisted-key"
        );
        assert!(
            broker.materialize_process(reference).await.is_err(),
            "process reference is one-shot"
        );
    }

    /// ACP credential-realization cause graph:
    ///
    /// exact binding -> process-secret mechanism -> opaque requirement planned ->
    /// current ownership -> secret open -> durable receipt -> process material.
    /// Planning never opens material. Every later failure terminates broker
    /// consumption; ownership failure precedes secret access and receipt failure
    /// prevents material from reaching the process.
    ///
    /// Credential publication first proves its sealed material by read-back;
    /// `gets` below counts only opens after launch planning begins. The material
    /// cause is therefore its current launch-time availability, including loss
    /// after a valid publication.
    ///
    /// | Rule | binding | mechanism | ownership | launch material | receipt | planned | gets | records | process |
    /// |---|---|---|---|---|---|---|---:|---:|---|
    /// | A1 | missing | exact | current | present | accept | no | 0 | 0 | reject |
    /// | A2 | exact | wrong | current | present | accept | no | 0 | 0 | reject |
    /// | A3 | exact | exact | lost | present | accept | yes | 0 | 0 | reject |
    /// | A4 | exact | exact | current | missing | accept | yes | 1 | 0 | reject |
    /// | A5 | exact | exact | current | present | reject | yes | 1 | 1 | reject |
    /// | A6 | exact | exact | current | present | accept | yes | 1 | 1 | material |
    #[tokio::test(flavor = "multi_thread")]
    async fn credential_launch_cases_are_generated_from_the_decision_table() {
        #[derive(Clone, Copy)]
        struct Rule {
            id: &'static str,
            include_binding: bool,
            kind: CredentialRealizationKind,
            owns: bool,
            material_available: bool,
            receipt_accepts: bool,
            expected_gets: usize,
            expected_records: usize,
            plans: bool,
            launches: bool,
        }

        let rules = [
            Rule {
                id: "A1",
                include_binding: false,
                kind: CredentialRealizationKind::ProcessSecretEnvironment,
                owns: true,
                material_available: true,
                receipt_accepts: true,
                expected_gets: 0,
                expected_records: 0,
                plans: false,
                launches: false,
            },
            Rule {
                id: "A2",
                include_binding: true,
                kind: CredentialRealizationKind::WorkerRelay,
                owns: true,
                material_available: true,
                receipt_accepts: true,
                expected_gets: 0,
                expected_records: 0,
                plans: false,
                launches: false,
            },
            Rule {
                id: "A3",
                include_binding: true,
                kind: CredentialRealizationKind::ProcessSecretEnvironment,
                owns: false,
                material_available: true,
                receipt_accepts: true,
                expected_gets: 0,
                expected_records: 0,
                plans: true,
                launches: false,
            },
            Rule {
                id: "A4",
                include_binding: true,
                kind: CredentialRealizationKind::ProcessSecretEnvironment,
                owns: true,
                material_available: false,
                receipt_accepts: true,
                expected_gets: 1,
                expected_records: 0,
                plans: true,
                launches: false,
            },
            Rule {
                id: "A5",
                include_binding: true,
                kind: CredentialRealizationKind::ProcessSecretEnvironment,
                owns: true,
                material_available: true,
                receipt_accepts: false,
                expected_gets: 1,
                expected_records: 1,
                plans: true,
                launches: false,
            },
            Rule {
                id: "A6",
                include_binding: true,
                kind: CredentialRealizationKind::ProcessSecretEnvironment,
                owns: true,
                material_available: true,
                receipt_accepts: true,
                expected_gets: 1,
                expected_records: 1,
                plans: true,
                launches: true,
            },
        ];

        for rule in rules {
            let repo = Arc::new(InMemoryCredentialRepo::new());
            let secrets = Arc::new(CountingSecrets::new(false));
            let source = enter_credential(
                CredentialCreateParams {
                    workspace_id: "ws".into(),
                    kind: CredentialKind::Vault,
                    provider_id: Some("anthropic".into()),
                    env_key: None,
                    secret: Some(RedactedString::new("decision-key")),
                    oauth_command: None,
                },
                secrets.as_ref(),
                repo.as_ref(),
            )
            .await
            .unwrap_or_else(|error| panic!("{} fixture: {error}", rule.id));
            let launch_get_baseline = secrets.gets.load(Ordering::SeqCst);
            secrets
                .fail_get
                .store(!rule.material_available, Ordering::SeqCst);
            let candidate =
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                    ModelBinding::new("anthropic", "published-model", "acp:claude"),
                    "anthropic@1",
                    "anthropic-messages@1",
                    "ws",
                    Some(CredentialAccess::new(
                        CredentialRef {
                            id: source.id.0,
                            revision: 1,
                        },
                        CredentialMaterialSource::ControlPlaneReference,
                        CredentialUsage::ProviderAdapter,
                        CredentialExecutionPolicy::self_hosted_provider(),
                    )),
                    InferenceEndpoint {
                        adapter_kind: "anthropic".into(),
                        api_dialect: "anthropic_messages".into(),
                        base_url: "https://db.example/v1".into(),
                        upstream_model: "upstream-model".into(),
                        processing_placement: None,
                    },
                    test_acp_profile(),
                )
                .expect("coherent decision-table ACP provider candidate");
            let record_calls = Arc::new(AtomicUsize::new(0));
            let context = attempt_context_with(
                &candidate,
                rule.include_binding,
                rule.kind,
                Arc::new(DecisionOwnership(rule.owns)),
                Arc::new(DecisionRecorder {
                    accepts: rule.receipt_accepts,
                    calls: record_calls.clone(),
                }),
            );
            let resolver = PublishedAcpLaunchResolver::new(
                claude(),
                None,
                crate::PinnedCredentialMaterializer::new(repo, secrets.clone()),
            );

            let plan = resolver.model(&activation(Some(candidate)), &context).await;
            assert_eq!(plan.is_ok(), rule.plans, "{} planning verdict", rule.id);
            assert_eq!(
                secrets.gets.load(Ordering::SeqCst),
                launch_get_baseline,
                "{} planning never opens material",
                rule.id
            );
            let result = match plan {
                Ok(model) => {
                    let ResolvedModel::Managed { process_secret, .. } = &model else {
                        panic!("provider candidate must resolve as managed")
                    };
                    let reference = process_secret
                        .as_ref()
                        .expect("planned credential requirement")
                        .reference();
                    assert!(!format!("{model:?}").contains(reference));
                    resolver
                        .secret_broker()
                        .unwrap()
                        .materialize_process(reference)
                        .await
                }
                Err(error) => Err(awaken_provisioning_contract::SandboxError::new(
                    error.to_string(),
                )),
            };
            assert_eq!(result.is_ok(), rule.launches, "{} launch verdict", rule.id);
            if let Ok(material) = result {
                assert_eq!(material, b"decision-key", "{} exact material", rule.id);
            }
            assert_eq!(
                secrets.gets.load(Ordering::SeqCst) - launch_get_baseline,
                rule.expected_gets,
                "{} launch-time secret opens",
                rule.id
            );
            assert_eq!(
                record_calls.load(Ordering::SeqCst),
                rule.expected_records,
                "{} receipt attempts",
                rule.id
            );
        }
    }

    #[tokio::test]
    async fn backend_owned_resolution_never_opens_managed_config_or_material() {
        // Cause graph: published BackendOwned candidate -> explicit model policy
        // -> CLI projection. The managed config-home/materializer branches have no
        // edge from this variant.
        //
        // Decision table:
        // L1 Default + empty model -> BackendOwned(Default), empty extra env
        // L2 Exact + model id      -> BackendOwned(Exact), empty extra env
        for (rule, selection, model) in [
            ("L1", BackendModelSelection::Default, ""),
            ("L2", BackendModelSelection::Exact, "gpt-exact"),
        ] {
            let candidate =
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
                    ModelBinding::new("local-codex", model, "acp:codex"),
                    CredentialRef {
                        id: "local-codex".into(),
                        revision: 9,
                    },
                    selection,
                    "test",
                    "sha256:test-capability",
                    Default::default(),
                )
                .expect("coherent backend-owned model policy");
            let resolver = PublishedAcpLaunchResolver::backend_owned(
                *awaken_run_executor_acp::acp_cli("codex").unwrap(),
                Some(std::path::PathBuf::from("/path/that/must/not/be/opened")),
            );
            let activation = activation(Some(candidate));
            let resolved = resolver
                .model(
                    &activation,
                    &awaken_runtime_contract::RuntimeRunContext::new(),
                )
                .await
                .unwrap_or_else(|error| panic!("{rule}: {error}"));
            assert!(
                matches!(
                    resolved,
                    ResolvedModel::BackendOwned {
                        model_selection,
                        ref model,
                        ..
                    } if model_selection == selection && model == activation.effective_model_ref()
                ),
                "{rule}"
            );
            assert!(
                resolver.extra_env(&activation).unwrap().is_empty(),
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn missing_provider_candidate_fails_closed() {
        let resolver = PublishedAcpLaunchResolver::new(
            claude(),
            None,
            crate::PinnedCredentialMaterializer::new(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
            ),
        );
        assert!(
            resolver
                .model(
                    &activation(None),
                    &awaken_runtime_contract::RuntimeRunContext::new(),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backend_owned_resolver_rejects_provider_material_without_an_authority() {
        // Cause/effect graph: an image-backed Worker may execute BackendOwned
        // ACP without a credential store, but a Provider candidate requires an
        // exact materialization authority and must fail before launch.
        //
        // Decision table:
        // | Rule | candidate | materializer | outcome |
        // | B1 | BackendOwned | absent | resolve without secret broker |
        // | B2 | Provider | absent | credential_realization_unavailable |
        let resolver = PublishedAcpLaunchResolver::backend_owned(claude(), None);
        assert!(resolver.secret_broker().is_none(), "B1");
        assert!(
            resolver
                .credential_realization_capabilities()
                .realization_kinds
                .is_empty(),
            "B1"
        );

        let repo = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("must-not-open")),
                oauth_command: None,
            },
            secrets.as_ref(),
            repo.as_ref(),
        )
        .await
        .unwrap();
        let provider =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                ModelBinding::new("anthropic", "published-model", "acp:claude"),
                "anthropic@1",
                "anthropic-messages@1",
                "ws",
                Some(CredentialAccess::new(
                    CredentialRef {
                        id: source.id.0,
                        revision: 1,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::self_hosted_provider(),
                )),
                InferenceEndpoint {
                    adapter_kind: "anthropic".into(),
                    api_dialect: "anthropic_messages".into(),
                    base_url: "https://gateway.example/v1".into(),
                    upstream_model: "upstream-model".into(),
                    processing_placement: None,
                },
                test_acp_profile(),
            )
            .expect("coherent unavailable-material ACP provider candidate");
        let error = resolver
            .model(
                &activation(Some(provider)),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .unwrap_err();
        assert!(error.0.contains("credential_realization_unavailable"), "B2");
    }

    #[test]
    fn extra_env_points_the_cli_at_the_threads_config_home() {
        let base = std::env::temp_dir().join(format!("awaken-aclr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let r = PublishedAcpLaunchResolver::new(
            claude(),
            Some(base.clone()),
            crate::PinnedCredentialMaterializer::new(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
            ),
        );
        let env = r.config_home_env("thr_x").unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        assert!(std::path::Path::new(&env[0].1).is_dir());
        assert!(env[0].1.contains("thr_x"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn config_home_failure_is_terminal_and_never_uses_the_default_home() {
        let base = std::env::temp_dir().join(format!("awaken-aclr-invalid-{}", std::process::id()));
        let _ = std::fs::remove_file(&base);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::write(&base, b"not a directory").unwrap();
        let resolver = PublishedAcpLaunchResolver::new(
            claude(),
            Some(base.clone()),
            crate::PinnedCredentialMaterializer::new(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
            ),
        );
        let error = resolver.config_home_env("thr_x").unwrap_err();
        assert!(error.0.starts_with("local_config_home_unavailable:"));
        let _ = std::fs::remove_file(base);
    }
}
