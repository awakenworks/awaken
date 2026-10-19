//! Provider hint and compatibility presentation at the existing local entry.

use super::*;

#[tokio::test]
async fn local_projection_failure_keeps_its_provider_hint_and_failure_budget_class() {
    // L1-L2 compose the real local adapter, phase driver, root failure CAS and
    // application error. C1 the existing install role reports a provision
    // with a 6-second hint or an ordinary retryable fault; C2 Control reports
    // that original effect through its ordinary failure command.
    // L1 provision => original not-ready code + Some(h<=6s), zero failed
    // attempts; L2 ordinary fault => no invented hint, one failed attempt.
    // Both leave Preparing and no terminal cleanup fence. No hint is durable.
    for (rule, failure, hinted, attempts) in [("L1", 4, true, 0), ("L2", 1, false, 1)] {
        let repo = sqlite_session_repository("local hint repository").await;
        let session_id = format!("local-hint-{rule}");
        create(repo.as_ref(), persisted(&session_id, false, "preparing")).await;
        let runtime = Arc::new(super::super::creation::RecordingCreateRuntime::default());
        runtime.projection_failure.store(failure, Ordering::SeqCst);
        let app = SessionApplicationBuilder::fixture(
            runtime,
            Arc::new(NoopMcpRealizer),
            crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .configuration(SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            local_realization_owner: "local-hint-owner".into(),
            ..Default::default()
        })
        .build()
        .unwrap();
        let error = app
            .realize_session(&SessionId::from(session_id.clone()))
            .await
            .unwrap_err();
        let awaken_session_realization_contract::SessionRealizationDriveError::Effect(effect) =
            error
        else {
            panic!("{rule} original effect expected: {error}");
        };
        assert_eq!(
            effect.retry_after().is_some(),
            hinted,
            "{rule} hint presence"
        );
        if let Some(wait) = effect.retry_after() {
            assert!(
                wait <= std::time::Duration::from_secs(6),
                "{rule} wait never extended"
            );
            assert_eq!(
                effect.run_error().code,
                awaken_session_realization_contract::ENVIRONMENT_PROVISIONING_CODE,
                "{rule} original code"
            );
        }
        let root = repo.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(
            root.realization_progress().attempts(),
            attempts,
            "{rule} original classification controls the budget"
        );
        assert_eq!(
            root.execution,
            awaken_session_state_contract::SessionExecutionState::Preparing,
            "{rule}"
        );
        assert!(
            !root.terminal_cleanup.is_fenced(),
            "{rule} no terminal cleanup"
        );
    }
}

#[test]
fn realization_presentation_retains_messages_with_one_contract_error() {
    // P1-P3: one contract failure algebra enters the old application report
    // and update-error presentation. Only presentation keeps its contextual
    // prefix; fault/code/message and failure variants have no mirrored owner.
    use awaken_session_realization_contract::{
        SessionRealizationControlFailure as C, SessionRealizationDriveError as E,
    };
    for (rule, error, expected) in [
        (
            "P1",
            E::Effect(RunError::unavailable("original").into()),
            "Session realization effect failed: run failed: original".to_string(),
        ),
        (
            "P2",
            E::Control(C::Unavailable("original".into())),
            format!(
                "Session realization control failed: {}",
                C::Unavailable("original".into())
            ),
        ),
        (
            "P3",
            E::DidNotConverge,
            "Session realization did not converge".to_string(),
        ),
    ] {
        assert_eq!(
            crate::realization::realization_failure_message(&error),
            expected,
            "{rule} report"
        );
        assert_eq!(
            crate::SessionUpdateError::Realization(error).to_string(),
            format!("Session realization failed: {expected}"),
            "{rule} public context"
        );
    }
}
