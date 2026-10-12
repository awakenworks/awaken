use super::*;

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
        let built = SessionApplication::builder(
            runtime.clone(),
            Arc::new(NoopMcpRealizer),
            SessionPersistence::new(store, clock.clone()),
            environment.clone(),
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
    }
}

#[tokio::test]
async fn builder_freezes_normalized_owner_and_preserves_exact_creation_replay() {
    // B3 legal configuration => one immutable owner and one existing Store/Clock
    // binding. B4 exact create replay => same root, no repeated realization.
    let store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let clock = Arc::new(ConstructionClock::default());
    let persistence = SessionPersistence::new(store, clock.clone());
    let sink = persistence.environment_binding_sink();
    let runtime = Arc::new(creation::RecordingCreateRuntime::default());
    let app = SessionApplication::builder(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        persistence,
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .configuration(SessionApplicationConfiguration {
        local_realization_owner: "  runtime-a  ".into(),
        realization_retry_budget: 2,
        ..Default::default()
    })
    .build()
    .unwrap();
    assert_eq!(app.local_realization_owner(), "runtime-a");
    assert_eq!(app.configuration.local_realization_owner, "runtime-a");
    assert!(Arc::ptr_eq(&sink, &app.environment_binding_sink()));
    assert_eq!(clock.0.load(Ordering::SeqCst), 1);
    let created = app
        .create_session(creation::creation_command("builder-replay"))
        .await
        .unwrap();
    let preparations = runtime.preparations.load(Ordering::SeqCst);
    let replay = app
        .create_session(creation::creation_command("builder-replay"))
        .await
        .unwrap();
    assert_eq!(created, replay);
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), preparations);
}

#[tokio::test]
async fn builder_enabled_retention_propagates_authority_failure_instead_of_silent_success() {
    // B5 catalog-only assembly explicitly has no retention. B6 complete retained
    // resources must consult the supplied authority, including its failure;
    // B7 recovery uses that same authority and settles the retained generation.
    let resources = awaken_resource_persistence::ephemeral(system_clock())
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
        let app = SessionApplication::builder(
            Arc::new(NoopRuntime),
            Arc::new(NoopMcpRealizer),
            SessionPersistence::new(store.clone(), system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
        )
        .resources(capability)
        .build()
        .unwrap();
        let session = persisted("retention", true, "idle");
        create(store.as_ref(), session.clone()).await;
        let candidate = app.session_aggregate(&session.session_id).await.unwrap();
        let initial_revision = candidate.revision;
        let result = app
            .commit_resource_snapshot("workspace", candidate, "builder-retention", Vec::new())
            .await;
        assert_eq!(result.is_err(), enabled);
        if enabled {
            let retained = app.session_aggregate(&session.session_id).await.unwrap();
            assert_eq!(
                retained.revision, initial_revision,
                "retention failure blocks root CAS"
            );
            references.fail_replace.store(false, Ordering::SeqCst);
            app.commit_resource_snapshot("workspace", retained, "builder-retention", Vec::new())
                .await
                .unwrap();
        }
    }
}
