// @formal evidence config.inference_access_resolved_once_at_publication
// @formal evidence config_activation.stale_publication_never_installs
#[tokio::test]
async fn plugin_configuration_admission_is_shared_by_validate_preview_and_publish() {
    // DT-CONFIG-LIFECYCLE L1 an editable but semantically invalid section may
    // remain in the lossless Draft; L2 the exact same owning pure validator
    // refuses validate, preview and publication. Every refusal leaves the
    // original Draft intact, zero durable publications and zero registrations.
    // L3 inactive malformed residue is retained and does not enter this gate.
    use awaken_executable_agent_contract::capability::{
        PluginCapability, PluginConfigurationSource,
    };
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let service = Arc::new(ConfigService::new(
        Arc::new(FakeResolver),
        Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        vec![PluginConfigurationSource::Exact {
            capability: Box::new(PluginCapability {
                id: "owned".into(),
                schema_keys: vec!["owned".into()],
                config_schema: Some(serde_json::json!({"type":"object"})),
                bound: Default::default(),
            }),
            validate: Some(|value| {
                if value.is_some_and(|value| value["valid"] == true) {
                    Ok(())
                } else {
                    Err("owner refused the test configuration".into())
                }
            }),
        }],
    ));
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let plane = ConfigPlane::new(
        service,
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let workspace = scope("workspace-plugin-config");
    let mut config = agent_config("plugin-config");
    config.plugin_ids = vec!["owned".into()];
    config
        .plugin_config
        .insert("owned".into(), serde_json::json!({"valid":false}));
    plane.put(&workspace, &config).await.unwrap();
    let stored = plane.get(&workspace, &config.id).await.unwrap();
    let issue = plane.validate(&workspace, &config).await.unwrap_err();
    assert_eq!(issue.path, "plugin_config.owned");
    assert_eq!(issue.code.as_deref(), Some("plugin_configuration_invalid"));
    let mut preview = config.clone();
    preview.id = "preview-plugin-config".into();
    assert!(
        plane
            .preview_for_execution_workspace(
                &workspace,
                workspace.as_str(),
                &preview.id,
                &preview,
                AgentInputConfig {
                    agent_id: preview.id.clone(),
                    environment: None,
                    inputs: vec![],
                    revision: 0
                }
            )
            .await
            .is_err()
    );
    assert!(plane.publish(&workspace, &config.id).await.is_err());
    assert_eq!(plane.get(&workspace, &config.id).await.unwrap(), stored);
    assert!(plane.publications(&workspace).await.unwrap().is_empty());
    assert!(catalog.current(workspace.as_str(), &config.id).is_none());
    assert!(catalog.current(workspace.as_str(), &preview.id).is_none());
    config.plugin_ids.clear();
    plane.validate(&workspace, &config).await.unwrap();
    assert_eq!(
        config.plugin_config["owned"],
        serde_json::json!({"valid":false})
    );
}

#[tokio::test]
async fn publish_resolves_scope_access_once_into_the_persisted_snapshot() {
    // Cause/effect decision table: R1 one ordinary model pool, no advisor, and
    // successful resolution -> the injected resolver is invoked exactly once
    // and its exact scope/credential pin enters the persisted fingerprinted
    // snapshot. R2 resolver failure -> one invocation and no snapshot; the
    // complete result partition is exercised by the publication-module table.
    struct CountingProviderResolver {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ModelPublicationResolver for CountingProviderResolver {
        async fn resolve_models(
            &self,
            workspace: &ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            FakeProviderResolver
                .resolve_models(workspace, selection, candidates)
                .await
        }
    }

    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let resolver = Arc::new(CountingProviderResolver {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let service = Arc::new(ConfigService::new(
        resolver.clone(),
        Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        Vec::new(),
    ));
    let plane = ConfigPlane::new(
        service,
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let scope = ScopeId::from("workspace-a");
    plane.put(&scope, &agent_config("agent-a")).await.unwrap();
    let publication = plane.publish(&scope, "agent-a").await.unwrap();
    let candidate = &publication.snapshot.resolved_spec.model_binding;
    let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
        scope_id,
        credential: Some(credential),
        ..
    } = candidate.provisioning()
    else {
        panic!("publication carries a complete provider candidate")
    };
    assert_eq!(scope_id.as_str(), "workspace-a");
    assert_eq!(credential.credential.id, "credential-workspace-a");
    assert_eq!(publication.fingerprint, publication.snapshot.fingerprint.0);
    // C3 the compilation is fresh, C4 the store has committed its row.
    // E3 only the durable row's timestamp enters registration, and the
    // returned publication reports the same fact; C3+C4=>E3.
    assert!(publication.published_at_unix_ms.is_some(), "E3");
    assert!(publication.publication_sequence.is_some(), "E3");
    assert!(publication.publication_order_verified, "E3");
    assert_eq!(
        catalog
            .current("workspace-a", "agent-a")
            .unwrap()
            .published_at_unix_ms,
        publication.published_at_unix_ms,
        "E3"
    );
    assert_eq!(
        catalog
            .current("workspace-a", "agent-a")
            .unwrap()
            .publication_sequence,
        publication.publication_sequence,
        "E3"
    );
    assert!(
        catalog
            .current("workspace-a", "agent-a")
            .unwrap()
            .publication_order_verified,
        "E3"
    );
    assert_eq!(
        resolver.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "R1 one publication resolver invocation"
    );
}

#[tokio::test]
async fn preview_registers_inline_inputs_without_authoring_persistence() {
    // Preview cause/effect decision table:
    // P1 exact preview/config/input id + valid draft -> one Coordinator
    // registration carrying the exact inline resources; P2 no ConfigRegistry
    // is supplied -> no authoring draft or StoredPublication can be written;
    // P3 the monotonic successor withdrawal -> current resolution disappears.
    let (service, catalog) = test_service_and_catalog();
    let workspace = scope("workspace-preview");
    let preview_id = "preview-causal-1";
    let inputs = AgentInputConfig {
        agent_id: preview_id.into(),
        environment: None,
        inputs: vec![InputBinding {
            binding_id: BindingId::from("memory"),
            target: InputResourceId::MemoryStore(MemoryStoreId::from("memstore-7")),
            mount_path: "/mnt/memory".into(),
            access: ResourceAccess::ReadWrite,
            instructions: Some("prefer current project decisions".into()),
        }],
        revision: 1,
    };

    service
        .preview(
            &workspace,
            preview_id,
            &agent_config(preview_id),
            inputs.clone(),
            &[],
        )
        .await
        .unwrap();
    let registration = catalog
        .current(workspace.as_str(), preview_id)
        .expect("P1 preview registration");
    assert_eq!(registration.session_profile.resources, inputs.inputs, "P1");

    service
        .registrar
        .withdraw(ExecutableAgentWithdrawal {
            workspace_id: workspace.as_str().into(),
            agent_id: preview_id.into(),
            lifecycle_revision: 2,
        })
        .await
        .unwrap();
    assert!(
        catalog.current(workspace.as_str(), preview_id).is_none(),
        "P3"
    );
}

#[tokio::test]
async fn local_publication_pins_host_access_instead_of_deferring_to_runtime() {
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let plane = ConfigPlane::new(
        Arc::new(test_service()),
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let scope = ScopeId::from("workspace-a");
    plane.put(&scope, &agent_config("agent-a")).await.unwrap();

    let publication = plane.publish(&scope, "agent-a").await.unwrap();
    assert!(matches!(
        publication
            .snapshot
            .resolved_spec
            .model_binding
            .provisioning(),
        awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
    ));
}

// P1: a registry read failure on publish surfaces as `PublishError::Store`.
#[tokio::test]
async fn publish_maps_a_registry_read_failure_to_store() {
    let err = test_service()
        .publish(&scope(DEFAULT_SCOPE), &FailingRegistry, "a", &[])
        .await
        .unwrap_err();
    assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
}

// P6: a publication-persist failure (after a clean read + compile) is `Store`.
#[tokio::test]
async fn publish_maps_a_publication_persist_failure_to_store() {
    let err = test_service()
        .publish(&scope(DEFAULT_SCOPE), &PublishFailRegistry, "a", &[])
        .await
        .unwrap_err();
    assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
}

#[tokio::test]
async fn publish_never_installs_an_artifact_from_a_stale_source_revision() {
    // Cause/effect decision table: C1 prepare observes source revision 7; C2
    // every one of the bounded current-state retries loses to durable revision
    // 8; C3 the sole store method derives its fence from the publication's own
    // source revision; C4 an observed registrar counts effect entry.
    // R1(C1+C2+C3+C4) -> StaleRevision(8), zero registrar calls and no current
    // catalog entry; C5 a malformed store reports a committed revision that
    // does not match the artifact. R2(C1 + one C2 transition) -> re-prepare revision 8,
    // persist once and register only that current artifact; the next test owns
    // R2. R3(C1+C5) -> fail closed before registration. The SQL method derives
    // C3 by construction; this test crosses its typed receipt with the sole
    // caller effect gate while adapter tables cover the transaction decision.
    struct ObservedRegistrar {
        inner: LocalExecutableAgentRegistrar,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ExecutableAgentRegistrar for ObservedRegistrar {
        async fn register(
            &self,
            command: ExecutableAgentRegistration,
        ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.register(command).await
        }
        async fn withdraw(
            &self,
            command: ExecutableAgentWithdrawal,
        ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
            self.inner.withdraw(command).await
        }
    }
    for (write, rejected_revision, rule) in [
        (
            PublicationWrite::Conflict {
                current_revision: Some(8),
            },
            8,
            "R1",
        ),
        (PublicationWrite::Committed { source_revision: 6 }, 6, "R3"),
    ] {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = Arc::new(ObservedRegistrar {
            inner: LocalExecutableAgentRegistrar::new(catalog.clone()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let service = ConfigService::new(
            Arc::new(FakeResolver),
            registrar.clone(),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
        );
        let registry = StalePublishRegistry { write };
        let err = service
            .publish(&scope(DEFAULT_SCOPE), &registry, "a", &[])
            .await
            .unwrap_err();

        assert!(
            matches!(err, PublishError::StaleRevision(Some(revision)) if revision == rejected_revision),
            "{rule}: {err:?}"
        );
        assert_eq!(
            registrar.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{rule}: zero registrar calls"
        );
        assert!(
            catalog.current(DEFAULT_SCOPE, "a").is_none(),
            "{rule}: rejected publication must not enter the live catalog"
        );
    }
}

#[tokio::test]
async fn unreviewed_publish_converges_after_one_concurrent_revision_advance() {
    // Decision rule P2 from the table above. The public unreviewed command means
    // "publish current"; reviewed publish remains exact-revision fenced and is
    // covered separately by `reviewed_publish_fences_config_and_resource_revisions`.
    let (service, catalog) = test_service_and_catalog();
    let publication = service
        .publish(
            &scope(DEFAULT_SCOPE),
            &AdvancingPublishRegistry::default(),
            "a",
            &[],
        )
        .await
        .expect("one revision advance converges through the canonical publish path");

    assert_eq!(publication.source_revision, 8, "P2 current revision");
    assert_eq!(
        catalog
            .current(DEFAULT_SCOPE, "a")
            .expect("P2 current registration")
            .source_revision,
        8,
        "P2 no stale revision enters the executable catalog"
    );
}

#[tokio::test]
async fn unreadable_or_foreign_committed_publication_never_registers() {
    // Causes: R1 source CAS first conflicts and then commits; R2 committed
    // fingerprint cannot be read back; R3 readback carries a foreign execution
    // Workspace; R4 readback is unavailable. Effects: E1 no registration after
    // any broken receipt;
    // E2 the caller receives a Store error, not a successful publication.
    // Rules R1+R2=>E1+E2, R1+R3=>E1+E2, R1+R4=>E1+E2.
    for (mode, rule) in [(1, "R2"), (2, "R3"), (3, "R4")] {
        let (service, catalog) = test_service_and_catalog();
        let registry = AdvancingPublishRegistry::default();
        registry
            .read_behavior
            .store(mode, std::sync::atomic::Ordering::SeqCst);
        assert!(
            matches!(
                service
                    .publish(&scope(DEFAULT_SCOPE), &registry, "a", &[])
                    .await,
                Err(PublishError::Store(_))
            ),
            "{rule}/E2"
        );
        assert!(catalog.current(DEFAULT_SCOPE, "a").is_none(), "{rule}/E1");
    }
}

#[tokio::test]
async fn config_plane_preserves_publish_intent_during_source_race() {
    // C1 public edge receives no fence / source fence / resource fence / both;
    // C2 the real SQLite source advances after prepare's read, before its CAS.
    // Rules: no fences -> re-prepare and install revision 2; any fence -> 409
    // equivalent StaleRevision(2), no publication and no registrar effect.
    // The resolver injects the causal cut deterministically without sleeps;
    // the real scoped store owns source CAS and publication persistence.
    struct AdvanceDuringResolution {
        store: Arc<SqliteConfigStore>,
        scope: ScopeId,
        advanced: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl ModelPublicationResolver for AdvanceDuringResolution {
        async fn resolve_models(
            &self,
            workspace: &ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            if !self
                .advanced
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let mut next = agent_config("publish-race");
                next.instructions = "current revision".into();
                self.store
                    .put_config_scoped(&self.scope, &next)
                    .await
                    .unwrap();
            }
            FakeResolver
                .resolve_models(workspace, selection, candidates)
                .await
        }
    }
    for (source, resource) in [
        (None, None),
        (Some(1), None),
        (None, Some(0)),
        (Some(1), Some(0)),
    ] {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let scope = ScopeId::from("publication-source");
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let service = ConfigService::new(
            Arc::new(AdvanceDuringResolution {
                store: store.clone(),
                scope: scope.clone(),
                advanced: std::sync::atomic::AtomicBool::new(false),
            }),
            Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
        );
        let plane = ConfigPlane::new(
            Arc::new(service),
            store.clone(),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        plane
            .put(&scope, &agent_config("publish-race"))
            .await
            .unwrap();
        let result = plane
            .publish_for_execution_workspace_at_revisions(
                &scope,
                "execution-workspace",
                "publish-race",
                source,
                resource,
            )
            .await;
        let stored = store.list_published_scoped(&scope).await.unwrap();
        if source.is_none() && resource.is_none() {
            let publication = result.expect("unreviewed current publication converges");
            assert_eq!(publication.source_revision, 2);
            assert_eq!(stored.len(), 1);
            assert_eq!(
                catalog
                    .current("execution-workspace", "publish-race")
                    .unwrap()
                    .source_revision,
                2
            );
        } else {
            assert!(matches!(result, Err(PublishError::StaleRevision(Some(2)))));
            assert!(stored.is_empty());
            assert!(
                catalog
                    .current("execution-workspace", "publish-race")
                    .is_none()
            );
        }
        assert!(
            catalog.current(scope.as_str(), "publish-race").is_none(),
            "authoring scope cannot replace the requested execution Workspace"
        );
    }
}

// reconcile-a: a registry read failure on reconcile is a returned `Err`.
#[tokio::test]
async fn reconcile_propagates_a_registry_read_failure() {
    assert!(
        test_service()
            .reconcile(&scope(DEFAULT_SCOPE), &FailingRegistry, "a", &[])
            .await
            .is_err()
    );
}

// F19c: the get_config handler returns 500 when the store read fails.

#[tokio::test]
async fn resolve_agent_config_derives_the_effective_compaction_window_for_both_realizations() {
    struct WindowResolver;
    #[async_trait::async_trait]
    impl ModelPublicationResolver for WindowResolver {
        async fn resolve_models(
            &self,
            _workspace: &ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            Ok(ResolvedPublicationModels::host(
                selection
                    .resolved()
                    .cloned()
                    .unwrap_or_else(|| ModelBinding::new("p", "m-x", "b")),
                candidates.to_vec(),
                Some(200_000),
                Some(40_000),
            ))
        }
    }
    let service = ConfigService::new(
        Arc::new(WindowResolver),
        test_registrar(),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        Vec::new(),
    );
    let pin = || ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
    // Usable budget = context_window − max_output_tokens = 200k − 40k = 160k;
    // default trigger = 3/4 × 160k = 120k.

    // NATIVE (compact section, no agent override): the effective window at ratio 1.0 —
    // the fold point IS the trigger (headroom + ratio already baked in).
    let mut cfg = agent_config("a1");
    cfg.model_binding = pin();
    cfg.plugin_ids.push("compact".into());
    let out = resolve_config(&service, cfg).await;
    assert_eq!(out.plugin_config["compact"]["max_tokens"], 120_000);
    assert!(out.plugin_config["compact"].get("trigger_ratio").is_none());

    // ACP (acp section): the same effective window flows to the CLI's compact_window.
    let mut cfg_acp = agent_config("a-acp");
    cfg_acp.model_binding = ModelSelection::Pinned(ModelBinding::new("p", "m-x", "acp:claude"));
    let out_acp = resolve_config(&service, cfg_acp).await;
    assert_eq!(out_acp.plugin_config["acp"]["compact_window"], 120_000);

    // Agent OVERRIDE (under budget) is honored verbatim, in BOTH realizations.
    let mut cfg2 = agent_config("a2");
    cfg2.model_binding = pin();
    cfg2.compaction = Some(awaken_agent_config::CompactionStrategy {
        window: Some(90_000),
        keep_recent: None,
    });
    cfg2.plugin_config
        .insert("compact".into(), serde_json::json!({}));
    cfg2.plugin_config
        .insert("acp".into(), serde_json::json!({}));
    let out2 = resolve_config(&service, cfg2).await;
    assert_eq!(out2.plugin_config["compact"]["max_tokens"], 90_000);
    assert_eq!(out2.plugin_config["acp"]["compact_window"], 90_000);

    // The typed strategy is the only trigger authority. A legacy JSON
    // trigger is overwritten at publication instead of creating a second
    // executable decision path.
    let mut cfg3 = agent_config("a3");
    cfg3.model_binding = pin();
    cfg3.plugin_ids.push("compact".into());
    cfg3.plugin_config.insert(
        "compact".into(),
        serde_json::json!({ "max_tokens": 50, "threshold": 2 }),
    );
    let out3 = resolve_config(&service, cfg3).await;
    assert_eq!(out3.plugin_config["compact"]["max_tokens"], 120_000);
    assert!(out3.plugin_config["compact"].get("threshold").is_none());

    // No compact/acp section → untouched (neither realization was opted into).
    let mut cfg4 = agent_config("a4");
    cfg4.model_binding = pin();
    let out4 = resolve_config(&service, cfg4).await;
    assert!(!out4.plugin_config.contains_key("compact"));
    assert!(!out4.plugin_config.contains_key("acp"));
}

// CEG F11d: a `compact` value that is present but NOT a JSON object (here a
// bare string) is a no-op — `apply_compaction` only reaches into an object, so a
// malformed section is left byte-identical and no window injected.
#[tokio::test]
async fn resolve_agent_config_leaves_a_non_object_compact_untouched() {
    struct WindowResolver;
    #[async_trait::async_trait]
    impl ModelPublicationResolver for WindowResolver {
        async fn resolve_models(
            &self,
            _workspace: &ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            Ok(ResolvedPublicationModels::host(
                selection.resolved().cloned().ok_or_else(|| {
                    PublicationResolutionError::Invalid("test requires a pinned model".to_string())
                })?,
                candidates.to_vec(),
                Some(200_000),
                None,
            ))
        }
    }
    let service = ConfigService::new(
        Arc::new(WindowResolver),
        test_registrar(),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        Vec::new(),
    );
    let mut cfg = agent_config("a4");
    cfg.model_binding = ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
    cfg.plugin_config
        .insert("compact".into(), serde_json::json!("not-an-object"));
    let out = resolve_config(&service, cfg).await;
    assert_eq!(
        out.plugin_config["compact"],
        serde_json::json!("not-an-object")
    );
}

#[tokio::test]
async fn publish_does_not_bake_session_resource_prompts_into_agent_instructions() {
    // Agent defaults remain authoring data until Session resolution. Publishing
    // the Agent must not bake a stale pre-merge resource prompt into its snapshot.
    let resources = Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
    resources
        .put_agent_inputs(
            DEFAULT_SCOPE,
            AgentInputConfig {
                agent_id: "agent-1".into(),
                environment: None,
                inputs: vec![InputBinding {
                    binding_id: BindingId::from("memory"),
                    target: InputResourceId::MemoryStore(MemoryStoreId::from("memstore-7")),
                    mount_path: "/mnt/memory/prefs".into(),
                    access: ResourceAccess::ReadWrite,
                    instructions: Some("user preferences".into()),
                }],
                revision: 1,
            },
        )
        .await
        .unwrap();

    let scope = ScopeId::from(DEFAULT_SCOPE);
    let plane = plane_with(
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        None,
        Some(resources),
    )
    .await;
    plane.put(&scope, &agent_config("agent-1")).await.unwrap();
    let publication = plane.publish(&scope, "agent-1").await.unwrap();

    // Only the authored Agent instructions are compiled. The final resource
    // prompt is generated from Effective Session inputs at preparation time.
    let instructions = &publication.snapshot.resolved_spec.instructions;
    assert!(instructions.starts_with("be helpful"));
    assert!(!instructions.contains("/mnt/memory/prefs"));
    assert!(!instructions.contains("user preferences"));
}

#[tokio::test]
async fn publish_pins_agent_model_catalog_and_session_defaults_revision() {
    let resources = Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
    resources
        .put_agent_inputs(
            DEFAULT_SCOPE,
            AgentInputConfig {
                agent_id: "pinned-inputs".into(),
                environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                    environment_id: "env-production".into(),
                    revision: 9,
                }),
                inputs: vec![],
                revision: 1,
            },
        )
        .await
        .unwrap();
    let tool = ToolDescriptor::pinned(
        "builtin",
        "search",
        "search",
        serde_json::json!({"type": "object"}),
    );
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let service = ConfigService::new(
        Arc::new(FakeResolver),
        Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        Vec::new(),
    )
    .with_resources(resources.clone());
    let plane = ConfigPlane::new(
        Arc::new(service),
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![tool.clone()])),
    );
    let scope = ScopeId::from(DEFAULT_SCOPE);
    let mut config = agent_config("pinned-inputs");
    config.tool_ids.push(tool.id.clone());
    plane.put(&scope, &config).await.unwrap();
    let publication = plane.publish(&scope, &config.id).await.unwrap();

    let metadata = &publication.snapshot.metadata;
    assert_eq!(metadata.source.agent_id.0, config.id);
    assert_eq!(metadata.source.revision, 1);
    assert_eq!(metadata.publication_version.0, publication.fingerprint);
    assert_eq!(metadata.fingerprint.0, publication.fingerprint);
    let kinds: Vec<_> = metadata
        .resolution
        .inputs
        .iter()
        .map(|input| input.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "agent_config",
            "agent_session_defaults",
            "model_binding",
            "tool"
        ]
    );
    assert_eq!(
        metadata.resolution.inputs[1].version,
        awaken_runtime_contract::ResolvedInputVersion::Revision(1)
    );
    assert_eq!(
        metadata.resolution.inputs[3].version,
        awaken_runtime_contract::ResolvedInputVersion::ContentHash(tool.content_hash())
    );

    use awaken_executable_agent_contract::ExecutableAgentProfileSource as _;
    let view = catalog
        .session_profile_in(DEFAULT_SCOPE, "pinned-inputs")
        .expect("matching published defaults");
    assert_eq!(view.environment.unwrap().revision, 9);

    resources
        .put_agent_inputs(
            DEFAULT_SCOPE,
            AgentInputConfig {
                agent_id: "pinned-inputs".into(),
                environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                    environment_id: "env-production".into(),
                    revision: 9,
                }),
                inputs: vec![],
                revision: 2,
            },
        )
        .await
        .unwrap();
    assert!(
        catalog
            .session_profile_in(DEFAULT_SCOPE, "pinned-inputs")
            .is_some(),
        "registered Coordinator view remains frozen when Control defaults later change"
    );
    assert_eq!(
        publication.agent_inputs.as_ref().unwrap().revision,
        1,
        "the publication owns the exact Resource defaults used at compile time"
    );
}

#[tokio::test]
async fn reviewed_publish_fences_config_and_resource_revisions() {
    // Reviewed-publish cause/effect table: R1 stale config + exact resources
    // -> StaleRevision; R2 exact config + stale resources ->
    // StaleResourceRevision; R3 both exact -> one self-contained publication.
    let resources = Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
    resources
        .put_agent_inputs(
            DEFAULT_SCOPE,
            AgentInputConfig {
                agent_id: "reviewed".into(),
                environment: None,
                inputs: vec![],
                revision: 1,
            },
        )
        .await
        .unwrap();
    let plane = plane_with(
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        None,
        Some(resources),
    )
    .await;
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &agent_config("reviewed")).await.unwrap();

    assert!(matches!(
        plane.publish_at_revisions(&scope, "reviewed", 99, 1).await,
        Err(PublishError::StaleRevision(Some(1)))
    ));
    assert!(matches!(
        plane.publish_at_revisions(&scope, "reviewed", 1, 99).await,
        Err(PublishError::StaleResourceRevision(1))
    ));
    let publication = plane
        .publish_at_revisions(&scope, "reviewed", 1, 1)
        .await
        .expect("R3 matching reviewed aggregate publishes");
    assert_eq!(publication.source_revision, 1);
    assert_eq!(publication.agent_inputs.unwrap().revision, 1);
}

#[tokio::test]
async fn publish_resolves_auto_to_the_first_offering_and_keeps_the_source_auto() {
    let plane = static_plane(Some(Arc::new(FakeResolver))).await;
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &auto_config("mgmt")).await.unwrap();
    let publication = plane.publish(&scope, "mgmt").await.unwrap();

    // The compiled publication carries the resolved concrete binding +
    // the remaining offerings as pool candidates (ADR-0052 D5).
    let spec = &publication.snapshot.resolved_spec;
    assert_eq!(spec.model_binding.identity().1, "m-first");
    assert_eq!(spec.model_candidates.len(), 1);
    assert_eq!(spec.model_candidates[0].identity().1, "m-second");

    // The stored *source* config is still Auto — so a later catalog change can
    // re-resolve it (reconcile returns true for policy-owned sources).
    assert!(plane.reconcile(&scope, "mgmt").await.unwrap());
}

#[tokio::test]
async fn managed_projection_preserves_the_published_authoring_model_id() {
    // Causes: C1 a Target carries a user-facing endpoint name while the
    // executable binding carries only the resolved provider identity; C2
    // Auto/Profile has no public authored id. Effects: E1 Target projection
    // round-trips the authored id exactly; E2 policy selection falls back to
    // the immutable resolved binding. This case exercises decision rule C1.
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let plane = ConfigPlane::new(
        Arc::new(ConfigService::new(
            Arc::new(FakeResolver),
            Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
        )),
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let scope = ScopeId::from(DEFAULT_SCOPE);
    let mut config = agent_config("managed-target");
    config.model_binding = ModelSelection::Target {
        target: awaken_agent_config::ModelTarget {
            model_id: "m-first".into(),
            provider_id: Some("openai".into()),
            api_dialect: Some("open_ai_chat".into()),
            protocol_endpoint_id: None,
            endpoint_name: Some("edge".into()),
        },
        backend_ref: "genai".into(),
        configuration: Default::default(),
    };
    plane.put(&scope, &config).await.unwrap();
    plane.publish(&scope, &config.id).await.unwrap();

    use awaken_executable_agent_contract::ExecutableAgentProfileSource as _;
    let view = catalog
        .session_profile_in(DEFAULT_SCOPE, &config.id)
        .unwrap();
    // Rule: provider-qualified Target (C1) -> full API id (E1) + exact
    // published execution model_ref (E2); neither substitutes for the other.
    let public_model = "m-first;provider=openai;api=open_ai_chat;endpoint=edge";
    assert_eq!(view.model.as_deref(), Some(public_model), "E1");
    assert_eq!(view.execution_model_ref.as_deref(), Some("m-first"), "E2");
}

#[tokio::test]
async fn reconcile_re_publishes_auto_but_skips_pinned() {
    let plane = static_plane(Some(Arc::new(FakeResolver))).await;
    let scope = ScopeId::from(DEFAULT_SCOPE);

    // A pinned agent: reconcile is a no-op (operator pin is authoritative).
    plane.put(&scope, &agent_config("pinned")).await.unwrap();
    plane.publish(&scope, "pinned").await.unwrap();
    assert!(!plane.reconcile(&scope, "pinned").await.unwrap());

    // An auto agent: reconcile re-publishes (idempotent by content address).
    plane.put(&scope, &auto_config("auto")).await.unwrap();
    plane.publish(&scope, "auto").await.unwrap();
    assert!(plane.reconcile(&scope, "auto").await.unwrap());
    assert_eq!(
        plane
            .get_versioned(&scope, "auto")
            .await
            .unwrap()
            .unwrap()
            .revision,
        1,
        "an exact policy fact replay must not manufacture a source revision"
    );

    // A missing agent: skipped, not an error.
    assert!(!plane.reconcile(&scope, "ghost").await.unwrap());
}

#[tokio::test]
async fn reconciliation_skips_terminal_agents_and_exact_port_rejects_them() {
    // Lifecycle reconciliation decision table:
    // | rule | current lifecycle | policy binding | reconcile | revision/publication |
    // | L1   | Published         | Auto           | eligible  | ordinary reconcile semantics |
    // | L2   | Disabled          | Auto           | false     | byte/revision/publication stable |
    // | L3   | Archived          | Auto           | false     | byte/revision/publication stable |
    // | L4   | Disabled/Archived | exact revision | error     | byte/revision stable |
    // Constraints: reconciliation refreshes only executable Published
    // agents; it is never a terminal-lifecycle mutation authority.
    for (id, disabled_at, archived_at, rule) in [
        ("disabled-auto", Some("disabled"), None, "L2/L4"),
        ("archived-auto", None, Some("archived"), "L3/L4"),
    ] {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            store.clone(),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from(format!("reconcile-{id}"));
        let published = auto_config(id);
        plane.put(&scope, &published).await.unwrap();
        plane.publish(&scope, id).await.unwrap();
        let publication = plane
            .latest_publication(&scope, id)
            .await
            .unwrap()
            .map(|publication| {
                (
                    publication.publication_id,
                    publication.fingerprint,
                    publication.source_revision,
                )
            });

        let mut terminal = published;
        terminal.disabled_at = disabled_at.map(str::to_string);
        terminal.archived_at = archived_at.map(str::to_string);
        store.put_config_scoped(&scope, &terminal).await.unwrap();
        let before = plane.get_versioned(&scope, id).await.unwrap().unwrap();

        assert!(!plane.reconcile(&scope, id).await.unwrap(), "{rule}");
        let after = plane.get_versioned(&scope, id).await.unwrap().unwrap();
        assert_eq!(after.revision, before.revision, "{rule} revision");
        assert_eq!(after.config, before.config, "{rule} config");
        assert_eq!(
            plane
                .latest_publication(&scope, id)
                .await
                .unwrap()
                .map(|publication| (
                    publication.publication_id,
                    publication.fingerprint,
                    publication.source_revision,
                )),
            publication,
            "{rule} publication"
        );

        let registry = ScopedConfig::new(store, scope);
        let error = plane
            .service()
            .advance_reconciliation_revision(&registry, &before.config, before.revision)
            .await
            .expect_err(rule);
        assert!(error.contains("published"), "{rule}: {error}");
        let exact_after = registry.get_config_revision(id).await.unwrap().unwrap();
        assert_eq!(exact_after.revision, before.revision, "{rule} exact port");
        assert_eq!(exact_after.config, before.config, "{rule} exact port");
    }
}

#[tokio::test]
async fn reconcile_advances_source_revision_before_changed_policy_fingerprint() {
    // Cause/effect decision table (FMECA registration conflict
    // S8/O7/D5=280): C1 Auto Agent revision 1 is published; C2 policy
    // resolution changes the executable fingerprint; C3 authoring intent is
    // otherwise unchanged and includes immutable legacy permission bytes; C4 a legacy release already persisted the changed
    // fingerprint at revision 1 before registration conflicted.
    // R1(C1+C2+C3+C4) -> ordinary replay confirms the conflict, CAS creates
    // revision 2 without canonicalizing the historical bytes, then one
    // revision-2 publication/register succeeds.
    // R2(exact replay) -> no bump (covered above). R3(CAS conflict) -> stale
    // error and no registration.
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let registrar = Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone()));
    let tools = Arc::new(crate::tool_catalog::StaticToolCatalog(vec![]));
    let initial = ConfigPlane::new(
        Arc::new(ConfigService::new(
            Arc::new(FakeResolver),
            registrar.clone(),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
        )),
        store.clone(),
        tools.clone(),
    );
    let scope = ScopeId::from("workspace-policy-change");
    let mut source = auto_config("auto-change");
    source.plugin_config.insert(
        "permission".into(),
        serde_json::json!({
            "default_behavior": "ask",
            "rules": [{ "pattern": "write", "behavior": "ask" }]
        }),
    );
    store.put_config_scoped(&scope, &source).await.unwrap();
    let first = initial.publish(&scope, "auto-change").await.unwrap();
    assert_eq!(first.source_revision, 1, "R1");

    let changed = ConfigPlane::new(
        Arc::new(ConfigService::new(
            Arc::new(ChangedPolicyResolver),
            registrar,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
        )),
        store.clone(),
        tools,
    );
    let registry = ScopedConfig::new(store.clone(), scope.clone());
    let legacy_duplicate = changed
        .service()
        .preview_publication(&scope, &registry, "auto-change", &[])
        .await
        .unwrap();
    assert_eq!(legacy_duplicate.source_revision, 1, "C4");
    assert_ne!(legacy_duplicate.fingerprint, first.fingerprint, "C4");
    store
        .seed_publication_history_for_test(&scope, &legacy_duplicate)
        .await
        .unwrap();
    assert!(
        changed.reconcile(&scope, "auto-change").await.unwrap(),
        "R1"
    );
    let second = changed
        .latest_publication(&scope, "auto-change")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.source_revision, 2, "R1");
    assert_ne!(first.fingerprint, second.fingerprint, "R1");
    assert_eq!(
        changed
            .get_versioned(&scope, "auto-change")
            .await
            .unwrap()
            .unwrap()
            .config
            .plugin_config["permission"],
        source.plugin_config["permission"],
        "R1/C3 exact historical bytes"
    );
    assert_eq!(
        catalog
            .current(scope.as_str(), "auto-change")
            .expect("R1 current registration")
            .source_revision,
        2,
        "R1"
    );
}

#[tokio::test]
async fn reconciler_adapter_republishes_the_named_auto_agents() {
    use crate::binding_resolver::{ConfigServiceReconciler, PublicationBindingReconciler};

    let plane = static_plane(Some(Arc::new(FakeResolver))).await;
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &auto_config("assistant")).await.unwrap();
    plane.publish(&scope, "assistant").await.unwrap();
    plane.put(&scope, &agent_config("pinned")).await.unwrap();
    plane.publish(&scope, "pinned").await.unwrap();

    // The catalog-write path drives one seam over a fixed id set; only the auto
    // one is re-published.
    let reconciler = ConfigServiceReconciler::new(
        plane.clone(),
        DEFAULT_SCOPE,
        DEFAULT_SCOPE,
        vec!["assistant".to_string(), "pinned".to_string()],
    );
    assert_eq!(reconciler.reconcile().await.unwrap(), 1);
}

#[tokio::test]
async fn observation_reconcile_republishes_policy_agents_in_every_owned_scope() {
    use crate::binding_resolver::{ConfigServiceReconciler, PublicationBindingReconciler};

    // Cause/effect graph:
    // C1 configs span ordinary and reserved scopes; C2 Auto is policy-bound;
    // C3 Pinned is operator-owned; C4 one observation event invokes reconcile_all.
    // E1 every Auto is republished in its own scope/execution Workspace; E2
    // Pinned is unchanged; E3 no scope is flattened into another.
    //
    // Decision rule O1: C1+C2+C3+C4 => three E1 publications + E2 + E3.
    // Store-failure propagation is covered by FailingScopedRegistry; single-id
    // policy/pinned/missing rules are covered by the test above.
    let plane = static_plane(Some(Arc::new(FakeResolver))).await;
    for (scope, id) in [("workspace-a", "auto-a"), ("workspace-b", "auto-b")] {
        let scope = ScopeId::from(scope);
        plane.put(&scope, &auto_config(id)).await.unwrap();
        plane.publish(&scope, id).await.unwrap();
    }
    let reserved = ScopeId::from(RESERVED_ADMIN_SCOPE);
    plane
        .put(&reserved, &auto_config("assistant"))
        .await
        .unwrap();
    plane
        .publish_for_execution_workspace(&reserved, "platform-workspace", "assistant")
        .await
        .unwrap();
    let pinned_scope = ScopeId::from("workspace-a");
    plane
        .put(&pinned_scope, &agent_config("pinned"))
        .await
        .unwrap();
    plane.publish(&pinned_scope, "pinned").await.unwrap();

    let reconciler = ConfigServiceReconciler::new(
        plane,
        RESERVED_ADMIN_SCOPE,
        "platform-workspace",
        Vec::new(),
    );
    assert_eq!(reconciler.reconcile_all().await.unwrap(), 3, "O1");
}

#[tokio::test]
async fn pinned_publication_uses_the_explicit_host_resolver() {
    let plane = static_plane(None).await;
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &agent_config("pinned")).await.unwrap();
    let publication = plane.publish(&scope, "pinned").await.unwrap();
    assert_eq!(
        publication
            .snapshot
            .resolved_spec
            .model_binding
            .identity()
            .1,
        "m"
    );
}

#[tokio::test]
async fn admin_tools_compile_only_in_the_reserved_scope() {
    use crate::tool_catalog::{RESERVED_ADMIN_SCOPE, ScopedToolCatalog};
    use awaken_runtime_contract::resolved::ToolDescriptor;

    // A catalog where `admin_x` is reserved-scope only (ADR-0052 D3).
    let admin = ToolDescriptor::pinned(
        "admin",
        "admin_x",
        "a management tool",
        serde_json::json!({"type": "object"}),
    );
    let catalog = ScopedToolCatalog::new(Vec::new(), RESERVED_ADMIN_SCOPE, vec![admin]);
    let plane = plane_with(Arc::new(catalog), None, None).await;

    // A config that names the admin tool.
    let mut cfg = agent_config("mgmt");
    cfg.tool_ids = vec!["admin_x".to_string()];

    // In the reserved scope it compiles: the descriptor is visible there.
    assert!(
        plane
            .validate(&ScopeId::from(RESERVED_ADMIN_SCOPE), &cfg)
            .await
            .is_ok(),
        "admin tool must resolve in the reserved scope"
    );

    // In any tenant scope it fails closed — the admin tool is not even disclosed,
    // so the same config hits UnknownTool at compile.
    let err = plane
        .validate(&ScopeId::from("wrkspc_acme"), &cfg)
        .await
        .unwrap_err();
    assert_eq!(err.path, "tools", "an unknown tool is a `tools` issue");
    assert!(
        err.message.contains("unknown tool") && err.message.contains("admin_x"),
        "tenant scope must reject the admin tool: {}",
        err.message
    );
}

#[tokio::test]
async fn no_resource_store_compiles_byte_identically() {
    // Without a wired resource store, instructions are the base verbatim.
    let scope = ScopeId::from(DEFAULT_SCOPE);
    let plane = static_plane(None).await;
    plane.put(&scope, &agent_config("agent-2")).await.unwrap();
    let publication = plane.publish(&scope, "agent-2").await.unwrap();
    assert_eq!(
        publication.snapshot.resolved_spec.instructions,
        "be helpful"
    );
}

// ==== CEG section 04: extra publish / resolve / handler coverage ====

// ---- publish + resolve_agent_config (F8/F10) ----

#[tokio::test]
async fn publish_missing_config_is_not_stored() {
    // P2: no config stored for the id → NotStored (before any resolve/compile).
    let plane = static_plane(None).await;
    let scope = ScopeId::from(DEFAULT_SCOPE);
    let err = plane.publish(&scope, "ghost").await.unwrap_err();
    assert!(matches!(err, PublishError::NotStored(_)), "{err:?}");
}

#[tokio::test]
async fn publish_is_unresolvable_when_the_catalog_has_no_model() {
    // P4: Auto + a resolver, but the resolver reports no provider-backed model.
    let plane = static_plane(Some(Arc::new(ErrResolver))).await;
    let scope = ScopeId::from(DEFAULT_SCOPE);
    plane.put(&scope, &auto_config("mgmt")).await.unwrap();
    let err = plane.publish(&scope, "mgmt").await.unwrap_err();
    assert!(matches!(err, PublishError::Unresolvable(_)), "{err:?}");
}

#[tokio::test]
async fn publish_compile_failure_when_config_names_an_unknown_tool() {
    // P5: resolve succeeds, compile fails because a named tool is not in the
    // (empty) catalog → Compile (not Unresolvable).
    let plane = static_plane(Some(Arc::new(FakeResolver))).await;
    let scope = ScopeId::from(DEFAULT_SCOPE);
    let mut cfg = agent_config("mgmt");
    cfg.tool_ids = vec!["ghost_tool".to_string()];
    plane.put(&scope, &cfg).await.unwrap();
    let err = plane.publish(&scope, "mgmt").await.unwrap_err();
    assert!(matches!(err, PublishError::Compile(_)), "{err:?}");
    assert!(err.to_string().contains("ghost_tool"), "{err}");
}

// ---- validate (F12) ----

#[tokio::test]
async fn validate_reports_a_resolver_failure_as_a_model_issue() {
    // F12a: an Auto binding that cannot resolve is a `model`-field issue.
    let plane = static_plane(Some(Arc::new(ErrResolver))).await;
    let issue = plane
        .validate(&ScopeId::from(DEFAULT_SCOPE), &auto_config("mgmt"))
        .await
        .unwrap_err();
    assert_eq!(issue.path, "model");
}

// ---- HTTP request_scope (F17) ----

#[tokio::test]
async fn web_compilation_rejects_every_config_entry_before_publication_effects() {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Effects(AtomicUsize);
    #[async_trait::async_trait]
    impl ModelPublicationResolver for Effects {
        async fn resolve_models(
            &self,
            _: &ScopeId,
            _: &ModelSelection,
            _: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("model resolution must not run".into())
        }
    }
    #[async_trait::async_trait]
    impl crate::credential_reference::CredentialReferenceValidator for Effects {
        async fn validate(
            &self,
            _: &ScopeId,
            _: &awaken_runtime_contract::credential::CredentialRef,
        ) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("credential validation must not run".into())
        }
    }
    // The same disabled, later duplicate is invalid through put, validation,
    // preview and durable publication. None may reach model/credential ports.
    let effects = Arc::new(Effects(AtomicUsize::new(0)));
    let mut service = ConfigService::new(
        effects.clone(),
        test_registrar(),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        Vec::new(),
    );
    service.credential_reference_validator = Some(effects.clone());
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let service = Arc::new(service);
    let plane = ConfigPlane::new(
        service.clone(),
        store.clone(),
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let workspace = scope("web-compiler-parity");
    let registry = ScopedConfig::new(store.clone(), workspace.clone());
    let mut config = agent_config("web-parity");
    config.toolsets = vec![ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy::default(),
        overrides: vec![
            ToolPolicyOverride::with_optional_configuration(
                "web_fetch",
                ToolExecutionPolicy::default(),
                Some(json!({"type":"web_fetch","max_content_tokens":0})),
            ),
            ToolPolicyOverride::with_optional_configuration(
                "web_fetch",
                ToolExecutionPolicy {
                    enabled: false,
                    ..Default::default()
                },
                Some(json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}})),
            ),
        ],
    }];
    let path = "toolsets[0].overrides[1].configuration.domains.domains";
    let error = service
        .validate(&workspace, &config, &[])
        .await
        .unwrap_err();
    assert_eq!(error.path, path);
    assert_eq!(error.code.as_deref(), Some("invalid_web_domain_count"));
    let error = service.put(&registry, &config).await.unwrap_err();
    assert!(
        error.contains(path) && error.contains("invalid_web_domain_count"),
        "{error}"
    );
    assert!(registry.get_config(&config.id).await.unwrap().is_none());
    let inputs = AgentInputConfig {
        agent_id: config.id.clone(),
        environment: None,
        inputs: vec![],
        revision: 1,
    };
    let error = service
        .preview(&workspace, &config.id, &config, inputs, &[])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("invalid_web_domain_count"),
        "{error}"
    );
    // Seed the real store directly to model a historical invalid frozen draft;
    // publication must revalidate it rather than trust authoring's past result.
    registry.put_config(&config).await.unwrap();
    let error = service
        .publish(&workspace, &registry, &config.id, &[])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("invalid_web_domain_count"),
        "{error}"
    );
    // Both audited final-write ports must compile before committing business
    // state or an effect. A rejected draft leaves its original audit pending;
    // correcting that same draft can commit through the same transaction.
    for with_effect in [false, true] {
        let mut candidate = config.clone();
        candidate.id = format!("web-audit-{with_effect}");
        let audit = awaken_agent_config::ManagementAuditRecord {
            tool: "admin_patch_agent".into(),
            call_id: candidate.id.clone(),
            summary: "Web admission parity".into(),
        };
        assert_eq!(
            plane
                .record_management_audit(&workspace, &audit)
                .await
                .unwrap(),
            AuditedConfigWrite::Applied
        );
        let effect = awaken_agent_config::ManagementEffect::UpsertAgentInputs {
            config: AgentInputConfig {
                agent_id: candidate.id.clone(),
                environment: None,
                inputs: vec![],
                revision: 1,
            },
        };
        let outcome = if with_effect {
            plane
                .put_with_audit_effect(&workspace, &candidate, 0, &audit, Some(&effect))
                .await
        } else {
            plane
                .put_with_audit(&workspace, &candidate, 0, &audit)
                .await
        };
        let error = outcome.unwrap_err();
        assert!(
            error.contains(path) && error.contains("invalid_web_domain_count"),
            "{error}"
        );
        assert!(
            plane
                .get_versioned(&workspace, &candidate.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !plane
                .get_management_audit(&workspace, &audit.tool, &audit.call_id)
                .await
                .unwrap()
                .unwrap()
                .business_committed
        );
        assert!(
            plane
                .pending_management_effects(&workspace)
                .await
                .unwrap()
                .is_empty()
        );
        candidate.toolsets[0].overrides.pop();
        let applied = if with_effect {
            plane
                .put_with_audit_effect(&workspace, &candidate, 0, &audit, Some(&effect))
                .await
        } else {
            plane
                .put_with_audit(&workspace, &candidate, 0, &audit)
                .await
        }
        .unwrap();
        assert_eq!(applied, AuditedConfigWrite::Applied);
        let durable = plane
            .get_versioned(&workspace, &candidate.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            durable.config.toolsets[0].overrides[0]
                .configuration
                .as_ref()
                .unwrap()["max_content_tokens"],
            0
        );
    }
    assert_eq!(effects.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn declared_mcp_configuration_uses_one_admission_for_validate_preview_and_publish() {
    // L4 declared input+cancel policy -> all three existing ConfigPlane paths
    // accept through the same source/parser, without connecting; L5 unknown
    // field -> all three refuse, preserving Draft/publications/catalog. S1-S7
    // owns other partitions; provider_push keeps all its original assertions.
    use awaken_runtime_contract::agent_bindings::{ToolsetPolicy, ToolsetSource};
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let service = Arc::new(ConfigService::new(
        Arc::new(FakeResolver),
        Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        vec![mcp_configuration_source()],
    ));
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let plane = ConfigPlane::new(
        service,
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    );
    let workspace = scope("workspace-mcp-source");
    let policy = serde_json::json!({"notifications":{
        "notifications/phone/input":{"event_id":"/id","delivery":{"kind":"message","text":"/text"}},
        "notifications/phone/interrupt":{"event_id":"/id","delivery":{"kind":"cancel_response","input_method":"notifications/phone/input","input_event_id":"/target"}}
    }});
    for valid in [false, true] {
        let mut config = agent_config(if valid {
            "mcp-source-valid"
        } else {
            "mcp-source-invalid"
        });
        config.plugin_ids = vec!["mcp:phone".into()];
        config.mcp_servers = vec![mcp_server_binding("phone")];
        config.toolsets = vec![ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "phone".into(),
            },
            default: Default::default(),
            overrides: Vec::new(),
        }];
        config.plugin_config.insert(
            "mcp:phone".into(),
            if valid {
                policy.clone()
            } else {
                serde_json::json!({"unknown":true})
            },
        );
        plane.put(&workspace, &config).await.unwrap();
        let stored = plane.get(&workspace, &config.id).await.unwrap();
        assert_eq!(
            plane.validate(&workspace, &config).await.is_ok(),
            valid,
            "L4/L5 validate"
        );
        let mut preview = config.clone();
        preview.id = format!("preview-{}", config.id);
        assert_eq!(
            plane
                .preview_for_execution_workspace(
                    &workspace,
                    workspace.as_str(),
                    &preview.id,
                    &preview,
                    AgentInputConfig {
                        agent_id: preview.id.clone(),
                        environment: None,
                        inputs: vec![],
                        revision: 0
                    }
                )
                .await
                .is_ok(),
            valid,
            "L4/L5 preview"
        );
        assert_eq!(
            plane.publish(&workspace, &config.id).await.is_ok(),
            valid,
            "L4/L5 publish"
        );
        assert_eq!(
            plane.get(&workspace, &config.id).await.unwrap(),
            stored,
            "pure admission does not rewrite authoring"
        );
        assert_eq!(
            catalog.current(workspace.as_str(), &config.id).is_some(),
            valid
        );
        assert_eq!(
            catalog.current(workspace.as_str(), &preview.id).is_some(),
            valid
        );
        if !valid {
            assert!(plane.publications(&workspace).await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn unselected_mcp_binding_errors_remain_with_the_original_agent_compiler() {
    // S9 the exact plugin admission succeeds despite an unrelated binding that
    // cannot advertise metadata; the subsequent complete Agent compiler still
    // refuses its empty name. No new publication Unresolvable/409 is invented.
    use awaken_executable_agent_contract::capability::{
        PluginCapability, PluginConfigurationSource,
    };
    let service = ConfigService::new(
        Arc::new(FakeResolver),
        Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
            ExecutableAgentCatalog::new(),
        ))),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        vec![
            mcp_configuration_source(),
            PluginConfigurationSource::Exact {
                capability: Box::new(PluginCapability {
                    id: "owned".into(),
                    schema_keys: vec![],
                    config_schema: None,
                    bound: Default::default(),
                }),
                validate: None,
            },
        ],
    );
    let mut config = agent_config("unselected-invalid-mcp");
    config.plugin_ids = vec!["owned".into()];
    config.mcp_servers = vec![mcp_server_binding("")];
    let before = config.clone();
    let issue = service
        .validate(&scope("workspace-mcp-source"), &config, &[])
        .await
        .unwrap_err();
    assert_eq!(
        issue.code, None,
        "original compiler refusal, not plugin_configuration_invalid"
    );
    assert_eq!(issue.path, "tools");
    assert_eq!(config, before);
}
