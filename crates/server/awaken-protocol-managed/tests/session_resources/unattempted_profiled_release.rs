use super::*;

#[tokio::test]
async fn profiled_release_closes_only_a_never_attempted_work_unit_preparation() {
    // Wire/application cause-effect rules: W1 exact WorkUnit mode + Preparing
    // + no admitted effects -> existing release returns verified cleanup;
    // W2 Interactive mode remains live -> reject without root writes;
    // W3 replay -> identical root/response; W4 wrong Workspace -> hidden.
    // No Run terminal is supplied as authorization and no Repository publication
    // is requested. Application tests own every unsafe constituent and CAS race.
    let sessions = support::session_repository("unattempted profiled release").await;
    let state = Arc::new(
        {
            let session_fixture_runtime = AcceptingFake::default();
            let session_fixture_clock = awaken_time::system_clock();
            awaken_protocol_managed_testkit::managed_state_from_parts(
                std::sync::Arc::new(session_fixture_runtime),
                None,
                sessions.clone(),
                awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                    .execution,
                session_fixture_clock,
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
        }
        .with_resource_registry(resource_registry().await),
    );
    let app = profiled_router(state.clone(), "default");
    for (id, mode, succeeds) in [
        ("profiled-unattempted", "work_unit", true),
        ("profiled-interactive-preparing", "interactive", false),
    ] {
        call(
            &app,
            "POST",
            "/v1/awaken/sessions",
            Some(json!({"session_id":id,"mode":mode,"agent_id":"coder"})),
        )
        .await
        .expect_for(StatusCode::ACCEPTED, "W1/W2 create");
        let before = sessions.get(&SessionId::from(id)).await.unwrap();
        assert_eq!(before.execution, SessionExecutionState::Preparing);
        assert_eq!(before.realization_progress().attempts(), 0);
        assert!(
            state.archive_session(id).await.is_err(),
            "public archive stays IdleOnly"
        );
        let path = format!("/v1/awaken/sessions/{id}/release");
        if succeeds {
            let reply = call(&app, "POST", &path, Some(json!({})))
                .await
                .expect_for(StatusCode::OK, "W1");
            assert!(
                reply.get("repository_publication").is_none(),
                "W1 no publication"
            );
            let completed = sessions.get(&SessionId::from(id)).await.unwrap();
            assert!(
                completed
                    .verified_terminal_cleanup()
                    .unwrap()
                    .is_completed()
            );
            let replay = call(&app, "POST", &path, Some(json!({})))
                .await
                .expect_for(StatusCode::OK, "W3");
            assert_eq!(reply, replay);
            assert_eq!(sessions.get(&SessionId::from(id)).await.unwrap(), completed);
        } else {
            call(&app, "POST", &path, Some(json!({})))
                .await
                .expect_for(StatusCode::BAD_REQUEST, "W2");
            assert_eq!(sessions.get(&SessionId::from(id)).await.unwrap(), before);
        }
    }
    let foreign = profiled_router(state, "foreign");
    call(
        &foreign,
        "POST",
        "/v1/awaken/sessions/profiled-unattempted/release",
        Some(json!({})),
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "W4");
}
