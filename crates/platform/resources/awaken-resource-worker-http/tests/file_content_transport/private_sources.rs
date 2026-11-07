//! A frozen File reference cannot widen its source Session's private owner.
use super::*;

async fn private_sessions(
    identity: &WorkerIdentity,
) -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    Arc<awaken_session_application::SessionQueries>,
) {
    let sessions = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    for (id, owner) in [("private-alice", "alice"), ("private-bob", "bob")] {
        let root = awaken_session_contract::SessionPrivateBinding::new(
            "org".into(),
            "desk".into(),
            owner.into(),
            "workspace-file".into(),
            1,
        )
        .unwrap()
        .bind_fresh(frozen_application_session(id, "file-application", identity))
        .unwrap();
        let key =
            awaken_session_contract::SessionMutationPayload::default_create_idempotency(&root);
        sessions
            .create("workspace-file", root, key, Vec::new())
            .await
            .unwrap();
    }
    let selected = awaken_session_execution_contract::SessionExecutionRoles::colocated(
        Arc::new(awaken_session_execution_contract::NeutralRuntime),
        Arc::new(awaken_session_execution_contract::NeutralRuntime),
    );
    let queries = Arc::new(awaken_session_application::SessionQueries::new(
        sessions.clone(),
        &selected,
        None,
    ));
    (sessions, queries)
}

#[tokio::test]
async fn a_live_claim_and_frozen_reference_cannot_read_another_private_sessions_file() {
    private_file_source_case(false).await;
}

#[tokio::test]
async fn a_live_claim_and_frozen_reference_cannot_adopt_another_persons_private_input() {
    private_file_source_case(true).await;
}

async fn private_file_source_case(personal_input: bool) {
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let digest = store.put(b"bob-private-output").await.unwrap();
    store
        .create_file(awaken_resource_contract::FileRecord {
            private_binding: personal_input.then(|| {
                awaken_resource_contract::PrivateContentBinding::new(
                    "org".into(),
                    "desk".into(),
                    "bob".into(),
                    "workspace-file".into(),
                    1,
                )
                .unwrap()
            }),
            id: "file-application".into(),
            workspace_id: "workspace-file".into(),
            blob_id: digest,
            filename: "private.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 18,
            created_at: "2026-11-05T00:00:00Z".into(),
            expires_at: None,
            downloadable: true,
            scope_id: (!personal_input).then(|| "private-bob".into()),
            logical_path: (!personal_input).then(|| "private.txt".into()),
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("private-file-worker").await;
    let (sessions, queries) = private_sessions(&identity).await;
    let dispatch = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let (claim, alice_thread) = private_claim(&dispatch, &identity, "private-alice").await;
    let (bob_claim, bob_thread) = private_claim(&dispatch, &identity, "private-bob").await;
    let lifecycle = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        store.clone(),
        store,
        lifecycle,
        awaken_time::system_clock(),
    ));
    let mut intents = Vec::new();
    for id in ["private-alice", "private-bob"] {
        let root = sessions.get(&id.into()).await.unwrap();
        intents.push(
            awaken_session_execution_contract::SessionResourceMaterializationIntent::try_new(
                id,
                1,
                resources("file-application").inputs()[0].clone(),
                root.realization.unwrap(),
            )
            .unwrap(),
        );
    }
    let control = Arc::new(ExactPrivateMaterialization {
        intents: intents.clone(),
    });
    let service = Arc::new(
        WorkerFileContentService::new(
            Arc::new(
                ApplicationFileContentSource::new(application).with_visibility(Arc::new(
                    awaken_session_application::SessionFileContentAccess::new(sessions.clone()),
                )),
            ),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            directory,
            awaken_time::system_clock(),
        )
        .with_session_file_authorization(queries)
        .with_session_control(control),
    );
    let address = support::serve(worker_file_content_router(service)).await;
    let source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );
    let result = source
        .read(
            "workspace-file",
            "file-application",
            &FileReadPurpose::SessionResource {
                reader_session_id: None,
            },
            Some(&ResourceOperationFence::Run(claim.clone())),
        )
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "a valid Alice claim and frozen reference cannot grant Bob's private output"
    );
    let forged = source
        .read(
            "workspace-file",
            "file-application",
            &FileReadPurpose::SessionResource {
                reader_session_id: Some("private-bob".into()),
            },
            Some(&ResourceOperationFence::Run(claim.clone())),
        )
        .await
        .unwrap_err();
    assert!(
        forged.to_string().contains("403"),
        "the held Alice claim must reject a forged Bob reader: {forged}"
    );
    let own = source
        .read(
            "workspace-file",
            "file-application",
            &FileReadPurpose::SessionResource {
                reader_session_id: None,
            },
            Some(&ResourceOperationFence::Run(bob_claim.clone())),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        own.bytes, b"bob-private-output",
        "the actual Bob claim still reads its own output"
    );
    for (reader, logical_thread, claim, allowed) in [
        ("private-alice", alice_thread, claim.clone(), false),
        ("private-bob", bob_thread, bob_claim.clone(), true),
    ] {
        let content = source
            .read(
                "workspace-file",
                "file-application",
                &FileReadPurpose::ModelContent {
                    reader_session_id: None,
                    thread_id: logical_thread.0.clone(),
                },
                Some(&ResourceOperationFence::Run(claim.clone())),
            )
            .await
            .unwrap();
        assert_eq!(
            content.is_some(),
            allowed,
            "a strong child model reference retains its original Session owner: {reader}"
        );
        let forged = source
            .read(
                "workspace-file",
                "file-application",
                &FileReadPurpose::ModelContent {
                    reader_session_id: Some("forged-session".into()),
                    thread_id: logical_thread.0,
                },
                Some(&ResourceOperationFence::Run(claim)),
            )
            .await
            .unwrap_err();
        assert!(
            forged.to_string().contains("403"),
            "model input cannot select a forged reader: {forged}"
        );
    }
    for (index, allowed) in [(0, false), (1, true)] {
        let fence = ResourceOperationFence::SessionMaterialization(intents[index].clone());
        let result = source
            .read(
                "workspace-file",
                "file-application",
                &FileReadPurpose::SessionResource {
                    reader_session_id: None,
                },
                Some(&fence),
            )
            .await
            .unwrap();
        assert_eq!(
            result.is_some(),
            allowed,
            "an exactly admitted materialization target retains its native private owner"
        );
        let forged = source
            .read(
                "workspace-file",
                "file-application",
                &FileReadPurpose::SessionResource {
                    reader_session_id: Some("forged-session".into()),
                },
                Some(&fence),
            )
            .await
            .unwrap_err();
        assert!(
            forged.to_string().contains("403"),
            "the intent's Session cannot be replaced by a wire reader: {forged}"
        );
    }
    sessions.close().await;
    let unavailable = source
        .read(
            "workspace-file",
            "file-application",
            &FileReadPurpose::SessionResource {
                reader_session_id: None,
            },
            Some(&ResourceOperationFence::Run(claim)),
        )
        .await
        .unwrap_err();
    assert!(
        unavailable.to_string().contains("503"),
        "unavailable native authority is not an ordinary fallback: {unavailable}"
    );
}

pub(super) async fn ordinary_visibility(
    identity: &WorkerIdentity,
    session_id: &str,
) -> Arc<dyn awaken_resource_contract::FileContentVisibility> {
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    insert_root(&repository, identity, session_id, "workspace-file", None).await;
    Arc::new(awaken_session_application::SessionFileContentAccess::new(
        repository,
    ))
}

async fn insert_root(
    repository: &awaken_session_store::SqliteManagedSessionRepository,
    identity: &WorkerIdentity,
    id: &str,
    workspace: &str,
    binding: Option<awaken_session_contract::SessionPrivateBinding>,
) {
    let root = frozen_application_session(id, "file-application", identity);
    let root = match binding {
        Some(binding) => binding.bind_fresh(root).unwrap(),
        None => root,
    };
    let key = awaken_session_contract::SessionMutationPayload::default_create_idempotency(&root);
    repository
        .create(workspace, root, key, Vec::new())
        .await
        .unwrap();
}

async fn file_record(
    store: &awaken_file_store::InMemoryFileStore,
    id: &str,
    scope: Option<&str>,
    missing_blob: bool,
) {
    let bytes = b"source-bytes";
    let digest = if missing_blob {
        awaken_resource_contract::content_id(b"not-stored")
    } else {
        store.put(bytes).await.unwrap()
    };
    store
        .create_file(awaken_resource_contract::FileRecord {
            private_binding: None,
            id: id.into(),
            workspace_id: "workspace-file".into(),
            blob_id: digest,
            filename: "input.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: bytes.len() as u64,
            created_at: "2026-11-05T00:00:00Z".into(),
            expires_at: None,
            downloadable: true,
            scope_id: scope.map(str::to_owned),
            logical_path: None,
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn in_process_files_require_exact_private_binding_and_known_native_provenance() {
    let (_, identity) = support::ready_worker("local-private-file-worker").await;
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    for (id, org, app, account, workspace, revision) in [
        ("alice", "org", "desk", "alice", "workspace-file", 1),
        ("alice-sibling", "org", "desk", "alice", "workspace-file", 1),
        ("other-org", "other", "desk", "alice", "workspace-file", 1),
        ("other-app", "org", "other", "alice", "workspace-file", 1),
        ("bob", "org", "desk", "bob", "workspace-file", 1),
        (
            "other-workspace",
            "org",
            "desk",
            "alice",
            "workspace-other",
            1,
        ),
        (
            "other-revision",
            "org",
            "desk",
            "alice",
            "workspace-file",
            2,
        ),
    ] {
        let binding = awaken_session_contract::SessionPrivateBinding::new(
            org.into(),
            app.into(),
            account.into(),
            workspace.into(),
            revision,
        )
        .unwrap();
        insert_root(&repository, &identity, id, workspace, Some(binding)).await;
    }
    insert_root(&repository, &identity, "ordinary", "workspace-file", None).await;
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    for scope in [
        "alice",
        "alice-sibling",
        "other-org",
        "other-app",
        "bob",
        "other-workspace",
        "other-revision",
        "ordinary",
        "unknown",
    ] {
        file_record(&store, scope, Some(scope), false).await;
    }
    file_record(&store, "unscoped", None, false).await;
    file_record(&store, "bob-missing-blob", Some("bob"), true).await;
    let lifecycle = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        store.clone(),
        store,
        lifecycle,
        awaken_time::system_clock(),
    ));
    let source = ApplicationFileContentSource::new(application.clone()).with_visibility(Arc::new(
        awaken_session_application::SessionFileContentAccess::new(repository.clone()),
    ));
    for (reader, allowed) in [
        (Some("alice"), vec!["alice", "alice-sibling"]),
        (Some("bob"), vec!["bob"]),
        (Some("ordinary"), vec!["ordinary", "unscoped"]),
        (None, vec!["ordinary", "unscoped"]),
        (Some("missing-reader"), Vec::new()),
        (Some("other-workspace"), Vec::new()),
    ] {
        for file in [
            "alice",
            "alice-sibling",
            "other-org",
            "other-app",
            "bob",
            "other-workspace",
            "other-revision",
            "ordinary",
            "unknown",
            "unscoped",
        ] {
            // Both staging and model inputs use the same native observation.
            for purpose in [
                FileReadPurpose::SessionResource {
                    reader_session_id: reader.map(str::to_owned),
                },
                FileReadPurpose::ModelContent {
                    thread_id: "logical-child".into(),
                    reader_session_id: reader.map(str::to_owned),
                },
            ] {
                let result =
                    <ApplicationFileContentSource as awaken_resource_contract::FileContentSource<
                        (),
                    >>::read(&source, "workspace-file", file, &purpose, None)
                    .await
                    .unwrap();
                assert_eq!(
                    result.is_some(),
                    allowed.contains(&file),
                    "reader={reader:?}, file={file}, purpose={purpose:?}"
                );
                if let Some(content) = result {
                    assert_eq!(content.bytes, b"source-bytes");
                }
            }
        }
    }
    let alice = FileReadPurpose::SessionResource {
        reader_session_id: Some("alice".into()),
    };
    let bob = FileReadPurpose::SessionResource {
        reader_session_id: Some("bob".into()),
    };
    assert!(
        <ApplicationFileContentSource as awaken_resource_contract::FileContentSource<()>>::read(
            &source,
            "workspace-file",
            "bob-missing-blob",
            &alice,
            None
        )
        .await
        .unwrap()
        .is_none(),
        "denial happens before a missing private blob can be opened"
    );
    assert!(
        <ApplicationFileContentSource as awaken_resource_contract::FileContentSource<()>>::read(
            &source,
            "workspace-file",
            "bob-missing-blob",
            &bob,
            None
        )
        .await
        .is_err(),
        "the owner reaches the actual blob error"
    );
    let unconfigured = ApplicationFileContentSource::new(application);
    for (file, purpose) in [
        ("alice", alice.clone()),
        (
            "ordinary",
            FileReadPurpose::SessionResource {
                reader_session_id: None,
            },
        ),
        ("unscoped", alice.clone()),
    ] {
        assert!(<ApplicationFileContentSource as awaken_resource_contract::FileContentSource<()>>::read(&unconfigured, "workspace-file", file, &purpose, None).await.is_err(), "missing authority cannot become an ordinary fallback");
    }
    repository.close().await;
    assert!(
        <ApplicationFileContentSource as awaken_resource_contract::FileContentSource<()>>::read(
            &source,
            "workspace-file",
            "alice",
            &alice,
            None
        )
        .await
        .is_err(),
        "a prior positive observation cannot survive unavailable native authority"
    );
}

async fn private_claim(
    dispatch: &MemoryDispatchStore,
    identity: &WorkerIdentity,
    session: &str,
) -> (RunClaim, ThreadId) {
    let mut activation = support::activation(&format!("child-{session}"));
    activation.input = vec![Message::new(
        MessageId(format!("message-{session}")),
        Role::User,
        vec![
            awaken_agent_contract::agent::content::ContentBlock::document_file("file-application"),
        ],
    )];
    let thread = activation.thread_id.clone();
    dispatch
        .enqueue(
            RunDispatch::new(activation)
                .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
                    awaken_agent_contract::ScopeId::from("workspace-file"),
                ))
                .for_session(ThreadId(session.into()))
                .with_session_resources(awaken_dispatch::SessionResourceEnvelope::new(
                    "workspace-file",
                    serde_json::to_string(&resources("file-application")).unwrap(),
                )),
        )
        .await
        .unwrap();
    let claimed = dispatch
        .claim(&identity.lease_owner(), 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    (RunClaim::from(&claimed.lease), thread)
}

// This fixture admits only the exact two prepared targets. The application
// owns the separate native current-lease matrix; this table tests that the
// HTTP edge derives its reader from that admitted intent, not the wire field.
struct ExactPrivateMaterialization {
    intents: Vec<awaken_session_execution_contract::SessionResourceMaterializationIntent>,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionResourceMaterializationControl
    for ExactPrivateMaterialization
{
    async fn authorize_resource_materialization_intent(
        &self,
        intent: &awaken_session_execution_contract::SessionResourceMaterializationIntent,
    ) -> Result<String, awaken_session_execution_contract::SessionRealizationControlFailure> {
        if self.intents.contains(intent) {
            Ok("workspace-file".into())
        } else {
            Err(awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership)
        }
    }
}
