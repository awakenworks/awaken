//! Cross-boundary publication-to-materialization acceptance tests.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_agent_config::ModelSelection;
    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_config_service::{ModelPublicationResolver, ResolvedPublicationModels};
    use awaken_credential_contract::{CredentialPurpose, CredentialTarget};
    use awaken_credential_materializer::CredentialInferenceMaterializer;
    use awaken_credential_vault::SecretStore;
    use awaken_credential_vault::repo::CredentialRepo;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialError, CredentialKind, CredentialStatus,
        InMemorySecretStore, SecretRef,
    };
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };
    use awaken_runtime_contract::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use awaken_control::model_publication::CatalogModelPublicationResolver;

    /// Author a catalog with one anthropic offering for `model`, and optionally a
    /// workspace credential `(provider, active)`. The secret is a fake — resolution
    /// and executor construction never call the network, so every branch is
    /// reachable offline.
    struct TestServices {
        resolver: CatalogModelPublicationResolver,
        materializer: CredentialInferenceMaterializer,
        catalog: Arc<InMemoryCatalogRepo>,
        credentials: Arc<InMemoryCredentialRepo>,
        secrets: Arc<InMemorySecretStore>,
    }

    struct CurrentOwnership;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::AttemptOwnershipVerifier for CurrentOwnership {
        async fn verify_current(
            &self,
        ) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
            Ok(())
        }
    }

    struct AcceptReceipt;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::CredentialRealizationRecorder for AcceptReceipt {
        async fn record(
            &self,
            _receipt: awaken_runtime_contract::CredentialRealizationReceipt,
        ) -> Result<(), awaken_runtime_contract::CredentialRealizationRecordError> {
            Ok(())
        }
    }

    fn attempt_binding(
        candidate: &ResolvedModelCandidate,
    ) -> Option<awaken_runtime_contract::AttemptCredentialBinding> {
        let ModelProvisioning::Provider {
            credential: Some(access),
            ..
        } = candidate.provisioning()
        else {
            return None;
        };
        Some(awaken_runtime_contract::AttemptCredentialBinding {
            candidate_fingerprint: awaken_runtime_contract::candidate_fingerprint(candidate)
                .unwrap(),
            credential: access.credential.clone(),
            selected_plaintext_holder: awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
            selected_realization_kind:
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            claim_epoch: 1,
        })
    }

    fn attempt_realization(
        candidates: &[&ResolvedModelCandidate],
    ) -> awaken_runtime_contract::AttemptCredentialRealization {
        awaken_runtime_contract::AttemptCredentialRealization::new(
            candidates
                .iter()
                .filter_map(|candidate| attempt_binding(candidate))
                .collect(),
            Arc::new(AcceptReceipt),
        )
    }

    fn attempt_context(candidate: &ResolvedModelCandidate) -> RuntimeRunContext {
        RuntimeRunContext::new()
            .with_ownership(Arc::new(CurrentOwnership))
            .with_credential_realization(attempt_realization(&[candidate]))
    }

    fn direct_resolver(
        catalog: Arc<InMemoryCatalogRepo>,
        credentials: Arc<InMemoryCredentialRepo>,
    ) -> CatalogModelPublicationResolver {
        let policy = awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider();
        let holder = policy
            .allowed_plaintext_holders
            .iter()
            .next()
            .expect("self-hosted Provider policy has an exact holder")
            .clone();
        CatalogModelPublicationResolver::from_repo(catalog, credentials)
            .with_direct_provider_credential_execution(policy, holder)
    }

    async fn provider(model: &str, credential: Option<(&str, bool)>) -> TestServices {
        let catalog = Arc::new(InMemoryCatalogRepo::new());
        catalog
            .put_provider(Provider {
                id: ProviderId::new("anthropic"),
                slug: "anthropic".into(),
                display_name: "Anthropic".into(),
                version: 1,
            })
            .await
            .unwrap();
        catalog
            .put_endpoint(ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep1"),
                provider_id: ProviderId::new("anthropic"),
                dialect: ApiDialect::AnthropicMessages,
                base_url: Some("https://api.anthropic.com/v1/".into()),
                timeout_secs: 300,
                display_name: "prod".into(),
                version: 1,
            })
            .await
            .unwrap();
        catalog
            .put_offering(Offering {
                model_id: model.into(),
                provider_id: ProviderId::new("anthropic"),
                protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
                dialect: ApiDialect::AnthropicMessages,
                upstream_model: None,
                source: Default::default(),
                status: Default::default(),
                last_seen_at_unix_ms: None,
            })
            .await
            .unwrap();

        let secrets = Arc::new(InMemorySecretStore::new());
        let creds = Arc::new(InMemoryCredentialRepo::new());
        if let Some((prov, active)) = credential {
            let source = enter_credential(
                CredentialCreateParams {
                    workspace_id: "ws".into(),
                    kind: CredentialKind::Vault,
                    provider_id: Some(prov.into()),
                    env_key: Some("ANTHROPIC_API_KEY".into()),
                    secret: Some(RedactedString::new("sk-test-fake")),
                    oauth_command: None,
                },
                secrets.as_ref(),
                creds.as_ref(),
            )
            .await
            .unwrap();
            if !active {
                let mut row = creds.get(&source.id).await.unwrap();
                row.status = CredentialStatus::Disabled;
                creds.put(row).await.unwrap();
            }
        }
        TestServices {
            resolver: direct_resolver(catalog.clone(), creds.clone()),
            materializer: CredentialInferenceMaterializer::new(creds.clone(), secrets.clone()),
            catalog,
            credentials: creds,
            secrets,
        }
    }

    struct CountingSecrets {
        inner: InMemorySecretStore,
        gets: AtomicUsize,
        fail_get: AtomicBool,
    }

    impl CountingSecrets {
        fn new() -> Self {
            Self {
                inner: InMemorySecretStore::new(),
                gets: AtomicUsize::new(0),
                fail_get: AtomicBool::new(false),
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
    impl awaken_runtime_contract::AttemptOwnershipVerifier for DecisionOwnership {
        async fn verify_current(
            &self,
        ) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
            self.0
                .then_some(())
                .ok_or(awaken_runtime_contract::AttemptOwnershipError::Lost)
        }
    }

    struct DecisionRecorder {
        accepts: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl awaken_runtime_contract::CredentialRealizationRecorder for DecisionRecorder {
        async fn record(
            &self,
            _receipt: awaken_runtime_contract::CredentialRealizationReceipt,
        ) -> Result<(), awaken_runtime_contract::CredentialRealizationRecordError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.accepts.then_some(()).ok_or_else(|| {
                awaken_runtime_contract::CredentialRealizationRecordError(
                    "planned receipt rejection".into(),
                )
            })
        }
    }

    async fn realization_fixture() -> (
        CredentialInferenceMaterializer,
        ResolvedModelCandidate,
        Arc<CountingSecrets>,
    ) {
        let secrets = Arc::new(CountingSecrets::new());
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("decision-table-secret")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let candidate = ResolvedModelCandidate::try_provider(
            ModelBinding::new("anthropic", "claude-x", "genai"),
            "anthropic@1",
            "endpoint@1",
            "ws",
            Some(
                awaken_runtime_contract::CredentialAccess::new(
                    awaken_runtime_contract::CredentialRef {
                        id: source.id.0,
                        revision: 1,
                    },
                    awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                    awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                    awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
                )
                .with_target(CredentialTarget::new(
                    CredentialPurpose::ProviderAdapter,
                    "anthropic",
                )),
            ),
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "https://provider.invalid/v1".into(),
                upstream_model: "claude-x".into(),
                processing_placement: None,
            },
        )
        .expect("coherent inference publication candidate");
        (
            CredentialInferenceMaterializer::new(credentials, secrets.clone()),
            candidate,
            secrets,
        )
    }

    #[tokio::test]
    async fn attempt_realization_cause_graph_decision_table() {
        // Cause graph:
        // binding ─> ownership ─> secret open ─> receipt commit ─> executor
        // each failed cause ───────────────────────────────────> fail closed
        // Constraint: the fixture publication already carries the exact
        // provider target and deployment-selected holder; this table varies
        // only attempt-time causes after publication admission.
        struct Rule {
            id: &'static str,
            binding: bool,
            ownership_current: bool,
            secret_available: bool,
            receipt_accepts: bool,
            expected_gets: usize,
            expected_receipts: usize,
            expected_executor: bool,
        }
        let rules = [
            Rule {
                id: "R1 missing binding",
                binding: false,
                ownership_current: true,
                secret_available: true,
                receipt_accepts: true,
                expected_gets: 0,
                expected_receipts: 0,
                expected_executor: false,
            },
            Rule {
                id: "R2 stale claim",
                binding: true,
                ownership_current: false,
                secret_available: true,
                receipt_accepts: true,
                expected_gets: 0,
                expected_receipts: 0,
                expected_executor: false,
            },
            Rule {
                id: "R3 material unavailable",
                binding: true,
                ownership_current: true,
                secret_available: false,
                receipt_accepts: true,
                expected_gets: 1,
                expected_receipts: 0,
                expected_executor: false,
            },
            Rule {
                id: "R4 receipt fenced",
                binding: true,
                ownership_current: true,
                secret_available: true,
                receipt_accepts: false,
                expected_gets: 1,
                expected_receipts: 1,
                expected_executor: false,
            },
            Rule {
                id: "R5 exact success",
                binding: true,
                ownership_current: true,
                secret_available: true,
                receipt_accepts: true,
                expected_gets: 1,
                expected_receipts: 1,
                expected_executor: true,
            },
        ];

        for rule in rules {
            let (materializer, candidate, secrets) = realization_fixture().await;
            // Fixture publication performs the canonical seal write/readback
            // preflight. The decision table measures only effect-edge opens,
            // so establish a zero counter after that independent writer cause.
            secrets.gets.store(0, Ordering::SeqCst);
            secrets
                .fail_get
                .store(!rule.secret_available, Ordering::SeqCst);
            let receipt_calls = Arc::new(AtomicUsize::new(0));
            let bindings = rule
                .binding
                .then(|| attempt_binding(&candidate).unwrap())
                .into_iter()
                .collect();
            let context = RuntimeRunContext::new()
                .with_ownership(Arc::new(DecisionOwnership(rule.ownership_current)))
                .with_credential_realization(
                    awaken_runtime_contract::AttemptCredentialRealization::new(
                        bindings,
                        Arc::new(DecisionRecorder {
                            accepts: rule.receipt_accepts,
                            calls: receipt_calls.clone(),
                        }),
                    ),
                );
            let actual = materializer
                .materialize_candidate(&candidate, &context)
                .await;
            assert_eq!(actual.is_some(), rule.expected_executor, "{}", rule.id);
            assert_eq!(
                secrets.gets.load(Ordering::SeqCst),
                rule.expected_gets,
                "{} secret opens",
                rule.id
            );
            assert_eq!(
                receipt_calls.load(Ordering::SeqCst),
                rule.expected_receipts,
                "{} receipt writes",
                rule.id
            );
        }
    }

    fn activation_with_fallback(primary: &str, fallback: &str) -> RunActivation {
        RunActivation::new(
            RunId("run".into()),
            ThreadId("thread".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snapshot".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: (!fallback.is_empty())
                        .then(|| {
                            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                                ModelBinding::new("openai", fallback, "genai"),
                            )
                        })
                        .into_iter()
                        .collect(),
                    catalog_fingerprint: CatalogFingerprint("catalog".into()),
                    instructions: String::new(),
                    max_steps: 2,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("anthropic", primary, "genai"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("catalog".into()),
            },
            Vec::new(),
        )
    }

    async fn resolve_activation(
        resolver: &CatalogModelPublicationResolver,
        activation: &RunActivation,
    ) -> Result<ResolvedPublicationModels, awaken_config_service::PublicationResolutionError> {
        let (primary, fallbacks) = if let Some(model_ref) = activation.model_ref_override.as_ref() {
            (
                activation
                    .snapshot
                    .resolved_spec
                    .candidate_for_model(model_ref)
                    .ok_or_else(|| {
                        awaken_config_service::PublicationResolutionError::Invalid(format!(
                            "model {model_ref} is outside the published candidate set"
                        ))
                    })?
                    .binding()
                    .clone(),
                Vec::new(),
            )
        } else {
            (
                activation
                    .snapshot
                    .resolved_spec
                    .model_binding
                    .binding()
                    .clone(),
                activation
                    .snapshot
                    .resolved_spec
                    .model_candidates
                    .iter()
                    .map(|candidate| candidate.binding().clone())
                    .collect(),
            )
        };
        resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("ws"),
                &ModelSelection::Pinned(primary),
                &fallbacks,
            )
            .await
    }

    #[tokio::test]
    async fn resolves_a_configured_model_to_an_executor() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
        let models = resolve_activation(&p.resolver, &activation).await.unwrap();
        assert!(
            p.materializer
                .materialize_candidate(&models.primary, &attempt_context(&models.primary))
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn publication_rejects_a_pool_when_any_candidate_cannot_be_pinned() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let error = resolve_activation(
            &p.resolver,
            &activation_with_fallback("claude-x", "missing-fallback"),
        )
        .await
        .expect_err("a partial candidate pool must not be published");

        assert!(error.to_string().contains("missing-fallback"));
    }

    #[tokio::test]
    async fn credential_materializer_rejects_host_executor_candidates() {
        let p = provider("configured", None).await;
        let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding::new("host", "embedded", "native"),
        );
        assert!(
            p.materializer
                .materialize_candidate(&candidate, &RuntimeRunContext::new())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unconfigured_model_is_rejected() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("no-such-model", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_model_without_a_credential_is_rejected() {
        let p = provider("claude-x", None).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_non_active_credential_is_rejected() {
        let p = provider("claude-x", Some(("anthropic", false))).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_credential_for_another_provider_is_rejected() {
        let p = provider("claude-x", Some(("openai", true))).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_credential_never_switches_to_a_new_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
        let pinned = resolve_activation(&p.resolver, &activation).await.unwrap();
        let ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            credential: Some(credential),
            ..
        } = pinned.primary.provisioning()
        else {
            panic!("publication must carry a complete provider candidate")
        };
        assert_eq!(provider_ref, "anthropic@1");
        assert_eq!(route_ref, "ep1@1");
        let pinned_id = credential.credential.id.clone();

        let second = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY_2".into()),
                secret: Some(RedactedString::new("sk-test-second")),
                oauth_command: None,
            },
            p.secrets.as_ref(),
            p.credentials.as_ref(),
        )
        .await
        .unwrap();
        assert_ne!(pinned_id, second.id.0);
        let candidate = pinned.primary.clone();
        assert!(
            p.materializer
                .materialize_candidate(&candidate, &attempt_context(&candidate))
                .await
                .is_some()
        );

        let mut old = p
            .credentials
            .get(&awaken_credential_contract::CredentialSourceId(pinned_id))
            .await
            .unwrap();
        old.status = CredentialStatus::Disabled;
        p.credentials.put(old).await.unwrap();
        assert!(
            p.materializer
                .materialize_candidate(&candidate, &attempt_context(&candidate))
                .await
                .is_none(),
            "revoking the pinned credential fails closed instead of selecting the new default"
        );
    }

    #[tokio::test]
    async fn publication_selects_credentials_only_from_the_supplied_scope() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let other = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-b".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY_B".into()),
                secret: Some(RedactedString::new("sk-test-b")),
                oauth_command: None,
            },
            p.secrets.as_ref(),
            p.credentials.as_ref(),
        )
        .await
        .unwrap();
        let model = ModelBinding::new("anthropic", "claude-x", "genai");
        let resolved = p
            .resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("workspace-b"),
                &ModelSelection::Pinned(model),
                &[],
            )
            .await
            .unwrap();
        let ModelProvisioning::Provider {
            scope_id,
            credential: Some(credential),
            ..
        } = resolved.primary.provisioning()
        else {
            panic!("publication must carry a complete provider candidate")
        };
        assert_eq!(credential.credential.id, other.id.0);
        assert_eq!(scope_id.as_str(), "workspace-b");
        assert!(
            p.materializer
                .materialize_candidate(&resolved.primary, &attempt_context(&resolved.primary))
                .await
                .is_some()
        );

        let mut provisioning = resolved.primary.provisioning().clone();
        let ModelProvisioning::Provider { scope_id, .. } = &mut provisioning else {
            unreachable!()
        };
        *scope_id = "ws".into();
        let forged = ResolvedModelCandidate::try_from_parts(
            resolved.primary.binding().clone(),
            provisioning,
        )
        .expect("cross-workspace fixture is intrinsically coherent");
        assert!(
            p.materializer
                .materialize_candidate(&forged, &attempt_context(&forged))
                .await
                .is_none(),
            "execution rejects a credential whose persisted owner differs from the snapshot scope"
        );
    }

    #[tokio::test]
    async fn runtime_never_weakens_the_published_injection_policy() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let resolved = p
            .resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("ws"),
                &ModelSelection::Pinned(ModelBinding::new("anthropic", "claude-x", "genai")),
                &[],
            )
            .await
            .unwrap();
        let ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        } = resolved.primary.provisioning()
        else {
            panic!("publication must carry a credential pin")
        };
        let direct_credential = serde_json::from_value(serde_json::json!({
            "credential": {
                "id": credential.credential.id,
                "revision": credential.credential.revision
            },
            "injection": "direct",
            "usage": { "type": "provider_adapter" },
            "policy": credential.policy
        }))
        .unwrap();
        let ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            access_kind,
            scope_id,
            endpoint,
            unspecified_reasoning,
            acp,
            ..
        } = resolved.primary.provisioning().clone()
        else {
            unreachable!()
        };
        let forged = ResolvedModelCandidate::try_from_parts(
            resolved.primary.binding().clone(),
            ModelProvisioning::Provider {
                provider_ref,
                route_ref,
                access_kind,
                scope_id,
                credential: Some(Box::new(direct_credential)),
                endpoint,
                unspecified_reasoning,
                acp,
            },
        )
        .expect("direct-injection fixture is intrinsically coherent");

        assert!(
            p.materializer
                .materialize_candidate(&forged, &attempt_context(&forged))
                .await
                .is_none(),
            "a reference-only materializer cannot downgrade a direct-only publication policy"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_route_is_independent_of_a_later_catalog_update() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
        let pinned = resolve_activation(&p.resolver, &activation).await.unwrap();
        p.catalog
            .put_endpoint(ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep1"),
                provider_id: ProviderId::new("anthropic"),
                dialect: ApiDialect::AnthropicMessages,
                base_url: Some("https://new.example/v1/".into()),
                timeout_secs: 300,
                display_name: "new".into(),
                version: 2,
            })
            .await
            .unwrap();
        assert!(
            p.materializer
                .materialize_candidate(&pinned.primary, &attempt_context(&pinned.primary))
                .await
                .is_some(),
            "execution uses the publication-pinned endpoint without consulting the updated catalog"
        );
    }

    #[tokio::test]
    async fn cross_provider_fallback_uses_only_dispatch_pinned_access() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        p.catalog
            .put_provider(Provider {
                id: ProviderId::new("openai"),
                slug: "openai".into(),
                display_name: "OpenAI".into(),
                version: 3,
            })
            .await
            .unwrap();
        p.catalog
            .put_endpoint(ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep-openai"),
                provider_id: ProviderId::new("openai"),
                dialect: ApiDialect::OpenAiChat,
                base_url: Some("https://api.openai.com/v1/".into()),
                timeout_secs: 300,
                display_name: "openai-prod".into(),
                version: 7,
            })
            .await
            .unwrap();
        p.catalog
            .put_offering(Offering {
                model_id: "gpt-x".into(),
                provider_id: ProviderId::new("openai"),
                protocol_endpoint_id: ProtocolEndpointId::new("ep-openai"),
                dialect: ApiDialect::OpenAiChat,
                upstream_model: None,
                source: Default::default(),
                status: Default::default(),
                last_seen_at_unix_ms: None,
            })
            .await
            .unwrap();
        enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_API_KEY".into()),
                secret: Some(RedactedString::new("sk-test-openai")),
                oauth_command: None,
            },
            p.secrets.as_ref(),
            p.credentials.as_ref(),
        )
        .await
        .unwrap();

        let pinned =
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", "gpt-x"))
                .await
                .unwrap();
        assert_eq!(
            std::iter::once(&pinned.primary)
                .chain(pinned.candidates.iter())
                .map(|candidate| candidate.binding().model_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-x", "gpt-x"]
        );
        let ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            ..
        } = pinned.candidates[0].provisioning()
        else {
            panic!("fallback must be provider-backed")
        };
        assert_eq!(provider_ref, "openai@3");
        assert_eq!(route_ref, "ep-openai@7");

        let ModelProvisioning::Provider {
            credential: Some(primary_credential),
            ..
        } = pinned.primary.provisioning()
        else {
            panic!("primary must carry its credential pin")
        };
        let primary_id = awaken_credential_contract::CredentialSourceId(
            primary_credential.credential.id.clone(),
        );
        let mut primary_row = p.credentials.get(&primary_id).await.unwrap();
        primary_row.status = CredentialStatus::Disabled;
        p.credentials.put(primary_row).await.unwrap();
        let primary_candidate = pinned.primary;
        let fallback_candidate = pinned.candidates[0].clone();
        assert!(
            p.materializer
                .materialize_candidate(&primary_candidate, &attempt_context(&primary_candidate),)
                .await
                .is_none()
        );
        assert!(
            p.materializer
                .materialize_candidate(&fallback_candidate, &attempt_context(&fallback_candidate),)
                .await
                .is_some(),
            "the already-pinned fallback remains materializable"
        );
    }
}
