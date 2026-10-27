#[cfg(test)]
mod scoped_cases {
    use super::super::*;
    use awaken_dispatch_contract::{DispatchCursor, DispatchError, DispatchPage};
    use awaken_dispatch_contract::{DispatchExecutionReads, DispatchOperationalFeed};
    use awaken_protocol_managed_testkit::{
        RehydrateFake, create_session_fixture, environment_fixture, sample_persisted,
    };
    use awaken_session_application_contract::SessionRunDispatchObservation;
    use awaken_session_application_contract::SessionRunExecutionPhase as Phase;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt as _;

    struct Probe {
        phase: Phase,
        unavailable: bool,
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl DispatchOperationalFeed for Probe {
        async fn events_after(
            &self,
            _: DispatchCursor,
            _: usize,
            _: Option<&awaken_agent_contract::agent::run::Id>,
        ) -> Result<DispatchPage, DispatchError> {
            panic!("zero native page must not read the delivery feed")
        }
    }
    #[async_trait::async_trait]
    impl DispatchExecutionReads for Probe {
        async fn observe_execution(
            &self,
            _: &awaken_agent_contract::agent::run::Id,
            _: &awaken_agent_contract::ThreadId,
        ) -> Result<Phase, DispatchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.unavailable {
                Err(DispatchError::Unavailable(
                    "probe dependency unavailable".into(),
                ))
            } else {
                Ok(self.phase)
            }
        }
    }

    async fn fixture(
        phase: Phase,
        unavailable: bool,
    ) -> (
        SessionRunDispatchReads,
        Arc<Probe>,
        Arc<dyn awaken_session_repository_contract::SessionStore>,
    ) {
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1));
        let repository = awaken_protocol_managed_testkit::ephemeral_session_repository().await;
        let mut root = sample_persisted("observed");
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(ref mut baseline) =
            root.baseline
        else {
            unreachable!()
        };
        baseline.mutation_policy =
            awaken_session_configuration_contract::resource::SessionMutationPolicy::Frozen;
        create_session_fixture(repository.as_ref(), "workspace", root).await;
        create_session_fixture(
            repository.as_ref(),
            "workspace",
            sample_persisted("managed"),
        )
        .await;
        let runtime = Arc::new(RehydrateFake::default());
        runtime
            .committed_by_thread
            .lock()
            .unwrap()
            .insert("observed".into(), Vec::new());
        runtime.run_ids_by_thread.lock().unwrap().insert(
            "observed".into(),
            awaken_agent_contract::agent::run::Id("run".into()),
        );
        let dispatch = Arc::new(Probe {
            phase,
            unavailable,
            calls: AtomicUsize::new(0),
        });
        let sessions = Arc::new(
            awaken_session_application::SessionApplicationBuilder::new(
                awaken_session_execution_contract::SessionExecutor::colocated(
                    runtime.clone(),
                    runtime,
                ),
                awaken_session_application::SessionPersistence::new(
                    repository.clone(),
                    clock.clone(),
                ),
                environment_fixture(clock).execution,
                awaken_session_application::SessionExecutionAdmission::fixture_with_dispatch(
                    dispatch.clone(),
                ),
                Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
            .build()
            .unwrap(),
        );
        let foreign = Arc::new(Probe {
            phase,
            unavailable: false,
            calls: AtomicUsize::new(0),
        });
        assert!(
            SessionRunDispatchReads::new(&sessions, foreign).is_err(),
            "one original Dispatch source"
        );
        (
            SessionRunDispatchReads::new(&sessions, dispatch.clone()).unwrap(),
            dispatch,
            repository,
        )
    }

    async fn request(
        reads: SessionRunDispatchReads,
        scope: Option<&str>,
        session: &str,
        run: &str,
    ) -> axum::response::Response {
        let router = awaken_protocol_awaken::session_run_dispatch_router(Arc::new(reads));
        let mut request = axum::http::Request::builder()
            .uri(format!("/v1/awaken/sessions/{session}/runs/{run}/dispatch"))
            .body(axum::body::Body::empty())
            .unwrap();
        if let Some(scope) = scope {
            request
                .extensions_mut()
                .insert(awaken_tenancy::WorkspaceScope(scope.into()));
        }
        router.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn native_dispatch_read_requires_exact_ownership_and_preserves_unknown_or_dependency() {
        // C1 exact/foreign/missing Workspace; C2 profiled/Managed/missing root;
        // C3 exact/missing primary Run; C4 one canonical/foreign Dispatch owner;
        // C5 phase or failed source. R1 C1+C2+C3+C4 -> exact typed phase, including
        // Unavailable (never fabricate Running). R2 wrong C1/C2/C3 -> refuse before
        // Dispatch I/O; R3 !C4 -> construction refusal; R4 source failure -> 503.
        // Every rule preserves the persisted root; no Run or delivery is submitted.
        for phase in [
            Phase::Unavailable,
            Phase::AdmissionPending,
            Phase::Admitted,
            Phase::RecoveryPending,
        ] {
            let (reads, probe, repository) = fixture(phase, false).await;
            let before = repository
                .get(&awaken_session_types::SessionId::from("observed"))
                .await
                .unwrap();
            let response = request(reads, Some("workspace"), "observed", "run").await;
            assert_eq!(response.status(), axum::http::StatusCode::OK, "R1");
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            let observation: SessionRunDispatchObservation =
                serde_json::from_slice(&bytes).unwrap();
            assert_eq!(observation.phase, phase);
            assert_eq!(observation.session_id, "observed");
            assert_eq!(observation.run_id.0, "run");
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                repository
                    .get(&awaken_session_types::SessionId::from("observed"))
                    .await
                    .unwrap(),
                before
            );
        }
        for (scope, session, run) in [
            (Some("foreign"), "observed", "run"),
            (None, "observed", "run"),
            (Some("workspace"), "missing", "run"),
            (Some("workspace"), "managed", "run"),
            (Some("workspace"), "observed", "foreign-run"),
        ] {
            let (reads, probe, _) = fixture(Phase::Admitted, false).await;
            assert_eq!(
                request(reads, scope, session, run).await.status(),
                axum::http::StatusCode::NOT_FOUND,
                "R2"
            );
            assert_eq!(probe.calls.load(Ordering::SeqCst), 0, "R2 no Dispatch read");
        }
        let (reads, probe, _) = fixture(Phase::Admitted, true).await;
        assert_eq!(
            request(reads, Some("workspace"), "observed", "run")
                .await
                .status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "R4"
        );
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    }
}
