//! The existing Workspace surface cannot serve private roots before trusted ingress.
use super::*;
use awaken_session_contract::{SessionMutationPayload, SessionPrivateBinding, SessionRepository};

#[tokio::test]
async fn workspace_only_session_routes_cannot_read_or_mutate_private_roots_on_either_router() {
    let repository = support::session_repository("private Session fixture").await;
    let state = support::shared_state_over(EchoFake::default(), repository.clone()).await;
    let first = router(state.clone());
    let ordinary = create_owned(&first, Some("ws_a")).await;
    let original = repository
        .get(&SessionId::from(ordinary.as_str()))
        .await
        .unwrap();
    let fresh = awaken_session_contract::PersistedSession::frozen_with_budget(
        "private-session",
        original.frozen_baseline().unwrap().clone(),
        Default::default(),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let private = SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "ws_a".into(),
        1,
    )
    .unwrap()
    .bind_fresh(fresh)
    .unwrap();
    repository
        .create(
            "ws_a",
            private.clone(),
            SessionMutationPayload::default_create_idempotency(&private),
            Vec::new(),
        )
        .await
        .unwrap();
    let durable = repository
        .get(&SessionId::from("private-session"))
        .await
        .unwrap();
    let second = router(support::shared_state_over(EchoFake::default(), repository.clone()).await);
    for app in [&first, &second] {
        assert_eq!(
            call_owned(
                app,
                "GET",
                &format!("/v1/sessions/{ordinary}"),
                Some("ws_a")
            )
            .await,
            StatusCode::OK
        );
        // Invalid write bodies must never reach decoders or handlers. A resolved
        // Workspace, including one carrying broad management authority, supplies
        // no private Account/application proof on either warm or cold ingress.
        for (method, suffix) in [
            ("GET", ""),
            ("POST", ""),
            ("DELETE", ""),
            ("POST", "/archive"),
            ("GET", "/events"),
            ("POST", "/events"),
            ("GET", "/events/stream"),
            ("GET", "/threads"),
            ("GET", "/resources"),
            ("POST", "/resources"),
        ] {
            assert_eq!(
                call_owned(
                    app,
                    method,
                    &format!("/v1/sessions/private-session{suffix}"),
                    Some("ws_a")
                )
                .await,
                StatusCode::NOT_FOUND,
                "{method} {suffix}"
            );
        }
    }
    let visible = state.list_sessions_scoped_durable("ws_a").await.unwrap();
    assert!(
        visible
            .iter()
            .all(|session| session.id != "private-session")
    );
    assert_eq!(
        repository
            .get(&SessionId::from("private-session"))
            .await
            .unwrap(),
        durable
    );
}
