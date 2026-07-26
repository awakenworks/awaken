//! Host-side provisioning for a launched ACP CLI: the [`LaunchResolver`] that turns
//! publication-pinned inference access into concrete launch inputs. Endpoint/model
//! coordinates come only from the immutable snapshot and the exact credential is
//! materialized from the persisted vault. Process environment may advertise which
//! CLI a worker can host, but never supplies provider execution facts.

use awaken_run_executor_acp::{
    AcpCli, ConfigHome, CredentialArtifactRequirement, LaunchResolver, OpenError,
    ProcessSecretRequirement, ResolvedModel,
};
#[cfg(test)]
use awaken_runtime_contract::CredentialRealizationKind;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::ModelProvisioning;
use std::path::PathBuf;

/// Resolves an ACP run from its published access and a worker-side exact credential
/// materializer. This adapter cannot query the model catalog or select a credential.
pub struct PublishedAcpLaunchResolver {
    cli: AcpCli,
    store_dir: Option<PathBuf>,
    credentials: crate::PinnedCredentialMaterializer,
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
            credentials,
        }
    }

    fn resolve_model(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<ResolvedModel, OpenError> {
        let model_ref = activation.effective_model_ref();
        let candidate = activation
            .snapshot
            .resolved_spec
            .candidate_for_model(model_ref)
            .ok_or_else(|| {
                OpenError(format!(
                    "model {model_ref} is outside the publication-pinned candidate set"
                ))
            })?;
        let ModelProvisioning::Provider {
            endpoint,
            credential,
            ..
        } = &candidate.provisioning
        else {
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
        let artifact_reference = self
            .credentials
            .plan_claimed_credential_artifact(candidate, context, self.cli.id)
            .map_err(OpenError)?;
        let credential_artifact = artifact_reference.map(|reference| {
            CredentialArtifactRequirement::new(
                reference,
                crate::credential_artifact::relative_path(self.cli.id)
                    .expect("only registered artifact codecs issue references"),
            )
        });
        let process_secret = if credential_artifact.is_some() {
            None
        } else {
            self.credentials
                .plan_claimed_process_secret(candidate, context)
                .map_err(OpenError)?
                .map(ProcessSecretRequirement::new)
        };
        if credential.is_some() && credential_artifact.is_none() && process_secret.is_none() {
            return Err(OpenError(
                "credential_realization_kind_unsupported: selected ACP credential cannot be provisioned"
                    .into(),
            ));
        }
        Ok(ResolvedModel {
            base_url: endpoint.base_url,
            model: endpoint.upstream_model,
            process_secret,
            credential_artifact,
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

impl LaunchResolver for PublishedAcpLaunchResolver {
    fn model(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<ResolvedModel, OpenError> {
        self.resolve_model(activation, context)
    }

    fn extra_env(&self, activation: &RunActivation) -> Result<Vec<(String, String)>, OpenError> {
        self.config_home_env(&activation.thread_id.0)
    }

    fn secret_broker(
        &self,
    ) -> Option<std::sync::Arc<dyn awaken_provisioning_contract::SecretBroker>> {
        Some(std::sync::Arc::new(self.credentials.clone()))
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let (material_sources, recipient_bound_envelopes) =
            self.credentials.material_source_capabilities();
        awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources,
            realization_kinds: [
                awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment,
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ]
            .into_iter()
            .collect(),
            recipient_bound_envelopes,
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

    fn attempt_context_with(
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        include_binding: bool,
        kind: CredentialRealizationKind,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
        recorder: Arc<dyn CredentialRealizationRecorder>,
    ) -> awaken_runtime_contract::RuntimeRunContext {
        let credential = match &candidate.provisioning {
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
            tool_capability_narrowing: Default::default(),
        }
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
        let model = awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
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
            },
        );
        let resolver = PublishedAcpLaunchResolver::new(
            claude(),
            None,
            crate::PinnedCredentialMaterializer::new(repo, secrets),
        );
        let context = attempt_context(&model);
        let resolved = resolver
            .model(&activation(Some(model.clone())), &context)
            .unwrap();
        assert_eq!(resolved.base_url, "https://db.example/v1");
        assert_eq!(resolved.model, "upstream-model");
        let reference = resolved
            .process_secret
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
    /// | Rule | binding | mechanism | ownership | material | receipt | planned | gets | records | process |
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
            let secrets = Arc::new(CountingSecrets::new(!rule.material_available));
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
            let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
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
                },
            );
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

            let plan = resolver.model(&activation(Some(candidate)), &context);
            assert_eq!(plan.is_ok(), rule.plans, "{} planning verdict", rule.id);
            assert_eq!(
                secrets.gets.load(Ordering::SeqCst),
                0,
                "{} planning never opens material",
                rule.id
            );
            let result = match plan {
                Ok(model) => {
                    let reference = model
                        .process_secret
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
                secrets.gets.load(Ordering::SeqCst),
                rule.expected_gets,
                "{} secret opens",
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

    #[test]
    fn missing_provider_candidate_fails_closed() {
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
                .is_err()
        );
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
