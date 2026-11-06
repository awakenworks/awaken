use super::*;

// The SAME recording impl belongs to App unit construction tests only. Shared
// test-support consumers select a real Dispatch owner's feed; they cannot acquire
// Coordinator feed authority from this observation/placement probe. Direct
// cfg(test) matches its real compilation scope (the parent alone is insufficient
// for the structural source guard). Counter and return body are unchanged.
#[cfg(test)]
#[async_trait::async_trait]
impl awaken_dispatch_contract::DispatchOperationalFeed for ExecutionObservationProbe {
    async fn events_after(
        &self,
        cursor: awaken_dispatch_contract::DispatchCursor,
        _limit: usize,
        _run: Option<&awaken_agent_contract::agent::run::Id>,
    ) -> Result<awaken_dispatch_contract::DispatchPage, awaken_dispatch_contract::DispatchError>
    {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(awaken_dispatch_contract::DispatchPage {
            events: Vec::new(),
            next_cursor: cursor,
        })
    }
}

#[derive(Default)]
struct ConstructionClock(AtomicUsize);

impl awaken_time::WallClock for ConstructionClock {
    fn now(&self) -> awaken_time::UtcTime {
        self.0.fetch_add(1, Ordering::SeqCst);
        awaken_time::UtcTime::from_epoch_ms(100)
    }
}

#[tokio::test]
async fn builder_rejects_invalid_configuration_before_observing_or_starting_services() {
    // C1 owner empty/whitespace; C2 budget zero. B1 C1 => typed owner refusal;
    // B2 !C1+C2 => typed budget refusal. Neither may sample the clock, create a
    // runtime incarnation, dispatch Work, or start physical realization.
    for (owner, budget, expected) in [
        ("", 3, SessionApplicationBuildError::EmptyRealizationOwner),
        (
            " \t\n",
            3,
            SessionApplicationBuildError::EmptyRealizationOwner,
        ),
        (
            "runtime",
            0,
            SessionApplicationBuildError::EmptyRealizationRetryBudget,
        ),
    ] {
        let store = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let clock = Arc::new(ConstructionClock::default());
        let environment = Arc::new(RecordingEnvironmentSource::default());
        let runtime = Arc::new(creation::RecordingCreateRuntime::default());
        let probe = Arc::new(ExecutionObservationProbe::default());
        let built = SessionApplicationBuilder::new(
            SessionExecutor::colocated(runtime.clone(), Arc::new(NoopMcpRealizer)),
            SessionPersistence::new(store, clock.clone()),
            environment.clone(),
            SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe.clone()),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .configuration(SessionApplicationConfiguration {
            local_realization_owner: owner.into(),
            realization_retry_budget: budget,
            ..Default::default()
        })
        .build();
        assert!(matches!(built, Err(error) if error == expected));
        assert_eq!(clock.0.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0);
        assert_eq!(environment.dispatch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn builder_freezes_normalized_owner_and_preserves_exact_creation_replay() {
    // B3 legal configuration => one immutable owner and one existing Store/Clock
    // binding. B4 exact create replay => same root, no repeated realization.
    // B8 queries and its root role share that same Repository; reads of an
    // existing or absent root neither prepare Runtime state nor dispatch Work.
    // B9 an owner mismatch is non-disclosing NotFound, never a second root.
    // Early-selection equivalence: selected Store/Clock and the cached sink
    // enter the existing builder before any command. Root/replay/read effects
    // below detect a hidden fallback; the paired public API docs reject later
    // Repository replacement even when test-support is enabled.
    let store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let clock = Arc::new(ConstructionClock::default());
    let persistence = SessionPersistence::new(store, clock.clone());
    let sink = persistence.environment_binding_sink();
    let runtime = Arc::new(creation::RecordingCreateRuntime::default());
    let app = SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        persistence,
        Arc::new(RecordingEnvironmentSource::default()),
        {
            let probe = std::sync::Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
        },
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(SessionApplicationConfiguration {
        local_realization_owner: "  runtime-a  ".into(),
        realization_retry_budget: 2,
        ..Default::default()
    })
    .build()
    .unwrap();
    crate::lifecycle::tests::assert_composed_identity(&app).await;
    crate::credentials::tests::assert_composed_identity(&app);
    assert_eq!(app.local_realization_owner(), "runtime-a");
    assert_eq!(app.configuration.local_realization_owner, "runtime-a");
    assert!(Arc::ptr_eq(&sink, &app.environment_binding_sink()));
    assert_eq!(clock.0.load(Ordering::SeqCst), 1);
    let created = app
        .creation_commands
        .create_session(creation::creation_command("builder-replay"))
        .await
        .unwrap();
    let preparations = runtime.preparations.load(Ordering::SeqCst);
    let replay = app
        .creation_commands
        .create_session(creation::creation_command("builder-replay"))
        .await
        .unwrap();
    assert_eq!(created, replay);
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), preparations);
    let queries = app.queries();
    assert!(
        Arc::ptr_eq(&queries, &app.queries()),
        "B8 one query service"
    );
    let roots: Arc<dyn SessionRootReads> = queries.clone();
    assert_eq!(
        roots.session(&created.session_id).await.unwrap(),
        created,
        "B8"
    );
    let owner = roots.owner(&created.session_id).await.unwrap();
    let projection = queries
        .read_session_projection(created.session_id.as_str(), Some(&owner))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(projection.session, created, "B8 exact durable projection");
    assert_eq!(projection.owner_scope, owner, "B8 exact owner");
    assert!(
        matches!(
            queries
                .read_session_projection(created.session_id.as_str(), Some("foreign-owner"))
                .await,
            Err(SessionProjectionRecoveryError::NotFound)
        ),
        "B9"
    );
    assert!(
        queries
            .read_session_projection("absent-builder-session", None)
            .await
            .unwrap()
            .is_none(),
        "B8 absent"
    );
    assert_eq!(
        runtime.preparations.load(Ordering::SeqCst),
        preparations,
        "B8/B9 no effects"
    );
}

#[tokio::test]
async fn builder_enabled_retention_propagates_authority_failure_instead_of_silent_success() {
    // B5 catalog-only assembly explicitly has no retention. B6 complete retained
    // resources must consult the supplied authority, including its failure;
    // B7 recovery uses that same authority and settles the retained generation.
    let resources = awaken_resource_persistence::ephemeral(
        system_clock(),
        Arc::new(creation::RecordingMemoryInvalidator::default()),
    )
    .await
    .unwrap();
    let authorities = resources.authorities();
    let references = Arc::new(RecordingReferenceIndex::default());
    references.fail_replace.store(true, Ordering::SeqCst);
    for enabled in [false, true] {
        let mut capability = SessionResources::catalog(authorities.resource_registry());
        if enabled {
            capability = capability.with_retention(
                references.clone(),
                Arc::new(UnusedFileCatalog),
                resources.purge_scheduler(),
            );
        }
        let store = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let app = SessionApplicationBuilder::fixture(
            Arc::new(NoopRuntime),
            Arc::new(NoopMcpRealizer),
            SessionPersistence::new(store.clone(), system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            {
                let probe = std::sync::Arc::new(crate::ExecutionObservationProbe::default());
                crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
            },
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .resources(capability)
        .build()
        .unwrap();
        let session = persisted("retention", true, "idle");
        create(store.as_ref(), session.clone()).await;
        let candidate = app
            .mutations
            .session_aggregate(&session.session_id)
            .await
            .unwrap();
        let initial_revision = candidate.revision;
        let result = app
            .resource_manifests
            .resource_references()
            .commit("workspace", candidate, "builder-retention", Vec::new())
            .await
            .map(crate::SnapshotCommit::into_session);
        assert_eq!(result.is_err(), enabled);
        if enabled {
            let retained = app
                .mutations
                .session_aggregate(&session.session_id)
                .await
                .unwrap();
            assert_eq!(
                retained.revision, initial_revision,
                "retention failure blocks root CAS"
            );
            references.fail_replace.store(false, Ordering::SeqCst);
            let _ = app
                .resource_manifests
                .resource_references()
                .commit("workspace", retained, "builder-retention", Vec::new())
                .await
                .unwrap()
                .into_session();
        }
    }
}

#[tokio::test]
async fn builder_freezes_execution_services_and_mount_checks_reject_foreign_instances_without_reads()
 {
    // C1 exact same object, C2 equal data from another object, C3 one replaced
    // role. R1 all exact -> mount accepted. R2 any C2/C3 -> refused before any
    // dispatch read, Worker observation, compiler call or Runtime preparation.
    // Repeating the checks never installs or replaces a source.
    for topology in [
        SessionExecutionPlacement::LocalWorker,
        SessionExecutionPlacement::RegisteredWorker,
    ] {
        let probe = Arc::new(ExecutionObservationProbe::default());
        let foreign = Arc::new(ExecutionObservationProbe::default());
        let dispatch: Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed> = probe.clone();
        let workers: Arc<dyn awaken_worker_contract::WorkerIncarnationSource> = probe.clone();
        let placement: Arc<dyn awaken_session_execution_contract::SessionPreparationPlacement> =
            probe.clone();
        let foreign_dispatch: Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed> =
            foreign.clone();
        let foreign_workers: Arc<dyn awaken_worker_contract::WorkerIncarnationSource> =
            foreign.clone();
        let foreign_placement: Arc<
            dyn awaken_session_execution_contract::SessionPreparationPlacement,
        > = foreign.clone();
        let store = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let clock = Arc::new(ConstructionClock::default());
        let runtime = Arc::new(creation::RecordingCreateRuntime::default());
        let app = SessionApplicationBuilder::new(
            SessionExecutor::colocated(runtime.clone(), Arc::new(NoopMcpRealizer)),
            SessionPersistence::new(store, clock.clone()),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionExecutionAdmission::new(dispatch.clone(), workers.clone(), placement.clone()),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .configuration(SessionApplicationConfiguration {
            execution_placement: topology,
            ..Default::default()
        })
        .build()
        .unwrap();
        assert_eq!(clock.0.load(Ordering::SeqCst), 1);
        assert!(app.execution_admission().verify_dispatch(&dispatch).is_ok());
        assert!(
            app.execution_admission()
                .verify_dispatch(&foreign_dispatch)
                .is_err()
        );
        for (observations, compiler, accepted) in [
            (&workers, &placement, true),
            (&foreign_workers, &placement, false),
            (&workers, &foreign_placement, false),
            (&foreign_workers, &foreign_placement, false),
        ] {
            assert_eq!(
                app.execution_admission()
                    .verify_worker_admission(observations, compiler)
                    .is_ok(),
                accepted
            );
        }
        assert!(app.execution_admission().verify_dispatch(&dispatch).is_ok());
        assert!(
            app.execution_admission()
                .verify_worker_admission(&workers, &placement)
                .is_ok()
        );
        assert_eq!(probe.0.load(Ordering::SeqCst), 0);
        assert_eq!(foreign.0.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0);
        // P4 positive observability control AFTER the original zero-read table:
        // the selected unit feed must increment the SAME counter and retain its
        // cursor. A detached/no-op implementation cannot make the earlier
        // no-effect assertions pass vacuously; the foreign instance stays unused.
        let cursor = awaken_dispatch_contract::DispatchCursor(7);
        let page = dispatch.events_after(cursor, 1, None).await.unwrap();
        assert!(page.events.is_empty());
        assert_eq!(page.next_cursor, cursor);
        assert_eq!(probe.0.load(Ordering::SeqCst), 1);
        assert_eq!(foreign.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn required_tool_compiler_precedes_session_creation_and_update_effects() {
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    compiler_precedes_effects(repository).await;
}

#[tokio::test]
async fn required_tool_compiler_postgres_preserves_the_frozen_configuration() {
    // An explicitly configured backend must run the same rejection/persistence
    // table; the ordinary unconfigured run makes no PostgreSQL coverage claim.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let repository = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .expect("configured Web policy PostgreSQL"),
    );
    compiler_precedes_effects(repository).await;
}

async fn compiler_precedes_effects(repository: Arc<dyn crate::SessionStore>) {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };
    let case_id = format!(
        "{}-{}",
        std::process::id(),
        system_clock().now().as_unix_nanos()
    );
    let runtime = Arc::new(creation::RecordingCreateRuntime::default());
    let environment = Arc::new(RecordingEnvironmentSource::default());
    let app = application_with_runtime(runtime.clone(), repository.clone(), environment.clone());
    let invalid = awaken_session_configuration_contract::SessionToolConfiguration {
        toolsets: vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: vec![ToolPolicyOverride::with_optional_configuration(
                "web_fetch",
                ToolExecutionPolicy::default(),
                Some(
                    serde_json::json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}}),
                ),
            )],
        }],
        client_tools: vec![],
    };
    // Both direct creation modes run the required real Web compiler before
    // creating a durable root or requesting any runtime/environment effects.
    for durable_only in [false, true] {
        let id = format!(
            "{}-{}",
            if durable_only {
                "web-invalid-accept"
            } else {
                "web-invalid-create"
            },
            &case_id
        );
        let mut command = creation::creation_command(&id);
        command.tools = invalid.clone();
        let result = if durable_only {
            app.creation_commands.accept_session(command).await
        } else {
            app.creation_commands.create_session(command).await
        };
        let SessionCreationError::Rejected(error) = result.unwrap_err() else {
            panic!("configuration must be rejected")
        };
        assert!(error.to_string().contains("invalid_web_domain_count"));
        assert!(repository.get(&SessionId::from(id)).await.is_err());
    }
    let inherited_id = format!("web-invalid-inherited-{}", &case_id);
    let mut inherited = creation::creation_command(&inherited_id);
    inherited.intent.control.toolsets = invalid.toolsets.clone();
    assert!(
        matches!(app.creation_commands.create_session(inherited).await, Err(SessionCreationError::Rejected(error)) if error.code == "invalid_web_domain_count")
    );
    assert!(
        repository
            .get(&SessionId::from(inherited_id))
            .await
            .is_err()
    );
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0);
    assert_eq!(environment.dispatch_calls.load(Ordering::SeqCst), 0);
    let mut accepted = creation::creation_command(&format!("web-update-{}", &case_id));
    let mut expected = invalid.clone();
    expected.toolsets[0].overrides[0].configuration = Some(serde_json::json!({
        "type":"web_fetch", "max_content_tokens":0,
        "domains":{"type":"allow","domains":["Example.com","Example.com"]},
        "url_sources":{
            "client_tool_results":{"type":"none"},
            "server_tool_results":{"type":"all"},
            "user_input":{"type":"all"}
        }
    }));
    accepted.tools = expected.clone();
    accepted.intent.control.toolsets = expected.toolsets.clone();
    let created = app
        .creation_commands
        .create_session(accepted)
        .await
        .unwrap();
    assert_eq!(created.tools, expected);
    assert_eq!(
        created.frozen_baseline().unwrap().toolsets,
        expected.toolsets
    );
    let cold = application_with_runtime(runtime.clone(), repository.clone(), environment);

    let before = repository.get(&created.session_id).await.unwrap();
    let effects_before = runtime.preparations.load(Ordering::SeqCst);
    let error = cold
        .update_commands
        .update_session(
            &created.session_id,
            SessionUpdateCommand {
                title: Some(SessionFieldUpdate::Replace("must not persist".into())),
                metadata: None,
                budget: None,
                tools: Some(invalid),
                mcp_update: None,
                idempotency_key: None,
                request_fingerprint: "invalid-web-update".into(),
                if_match: Some(before.revision),
            },
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("invalid_web_domain_count"),
        "{error}"
    );
    let after = repository.get(&created.session_id).await.unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.title, before.title);
    assert_eq!(after.tools, before.tools);
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), effects_before);
}

#[tokio::test]
async fn session_observation_keeps_an_absent_runtime_observer_without_effects() {
    // L2a cause/effect rule: an admitted Session uses the existing Runtime
    // fixture's neutral None observer. The narrow role must return None without
    // preparing execution, dispatching Work or mutating the same durable root.
    // RecordingEnvironmentSource.get remains unreachable: observation must not
    // reopen that catalog. L1 Some/actual HTTP delivery stays in streaming.rs;
    // L2b typed rejection is exercised with the real Coordinator Runtime.
    let store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(creation::RecordingCreateRuntime::default());
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let application =
        application_with_runtime(runtime.clone(), store.clone(), environments.clone());
    let created = application
        .creation_commands
        .create_session(creation::creation_command("absent-observer"))
        .await
        .unwrap();
    let preparations = runtime.preparations.load(Ordering::SeqCst);
    let dispatches = environments.dispatch_calls.load(Ordering::SeqCst);
    let observed = crate::SessionObservation::subscribe_session_thread_live(
        application.observations.as_ref(),
        &created.session_id,
        created.session_id.as_str(),
    )
    .await
    .unwrap();
    assert!(observed.is_none(), "L2a original neutral result");
    assert_eq!(
        runtime.preparations.load(Ordering::SeqCst),
        preparations,
        "L2a"
    );
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        dispatches,
        "L2a"
    );
    assert_eq!(
        store.get(&created.session_id).await.unwrap(),
        created,
        "L2a exact root"
    );
}

#[test]
fn required_tool_compiler_postgres_preserves_the_frozen_configuration_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::construction::required_tool_compiler_postgres_preserves_the_frozen_configuration",
    );
}

mod frozen_tools;
mod instance_lifecycle;
mod rejection_precedence;
