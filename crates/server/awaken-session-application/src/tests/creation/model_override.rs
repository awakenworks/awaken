//! The Session model-override decision at creation.
use super::*;

#[tokio::test]
async fn model_override_decision_reuses_only_an_equal_id_and_resolves_every_mismatch() {
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let mut app = application(repository, Arc::new(RecordingEnvironmentSource::default()));
    let publication = awaken_session_configuration_contract::SessionModelPublication {
        primary: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "override-account",
                "override-upstream",
                "genai",
            ),
        ),
        candidates: Vec::new(),
    };
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_model_publication_resolver(Arc::new(RecordingModelPublication {
        publication: publication.clone(),
        requests: requests.clone(),
    }));

    let reused = app
        .creation_commands
        .resolve_session_model_override(
            "workspace",
            "published-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect("equal model id reuses the publication");
    assert!(reused.publication.is_none());
    assert!(requests.lock().expect("model requests").is_empty());

    let resolved = app
        .creation_commands
        .resolve_session_model_override(
            "workspace",
            "requested-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect("mismatched model id resolves a complete publication");
    assert_eq!(resolved.publication.as_deref(), Some(&publication));
    assert_eq!(
        requests.lock().expect("model requests").as_slice(),
        [("workspace".to_string(), "requested-model".to_string())]
    );

    let duplicate = publication.primary.clone();
    app.set_model_publication_resolver(Arc::new(FixedModelPublication {
        publication: awaken_session_configuration_contract::SessionModelPublication {
            primary: duplicate.clone(),
            candidates: vec![duplicate],
        },
    }));
    let error = app
        .creation_commands
        .resolve_session_model_override(
            "workspace",
            "malformed-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect_err("malformed resolver output must fail before persistence");
    assert_eq!(
        error.kind,
        awaken_runtime_contract::runs::RunErrorKind::BadRequest
    );
    assert!(error.message.contains("duplicate model candidate"));
}
