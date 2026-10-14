use super::*;
use awaken_session_configuration_contract::resource as session_resource;

struct FileFenceProbe {
    accepted: Option<crate::ResourceOperationFence>,
    seen: std::sync::Mutex<Vec<Option<crate::ResourceOperationFence>>>,
}

impl FileFenceProbe {
    fn new(accepted: Option<crate::ResourceOperationFence>) -> Self {
        Self {
            accepted,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<Option<crate::ResourceOperationFence>> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl crate::FileContentSource<crate::ResourceOperationFence> for FileFenceProbe {
    async fn read(
        &self,
        _workspace: &str,
        file_id: &str,
        _purpose: &awaken_resource_contract::FileReadPurpose,
        fence: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        Option<awaken_resource_contract::ResolvedFileContent>,
        awaken_resource_contract::FileContentSourceError,
    > {
        self.seen.lock().unwrap().push(fence.cloned());
        if fence != self.accepted.as_ref() {
            return Err(awaken_resource_contract::FileContentSourceError::new(
                "File fence did not match the current authority",
            ));
        }
        let bytes = b"one exact File".to_vec();
        Ok(Some(awaken_resource_contract::ResolvedFileContent {
            file_id: file_id.into(),
            content_id: awaken_resource_contract::content_id(&bytes),
            filename: "input.txt".into(),
            media_type: "text/plain".into(),
            bytes,
        }))
    }
}

#[tokio::test]
async fn file_staging_passes_one_exact_prepared_fence_to_one_source_read() {
    // Causes: C1 role={store-owning,database-less Worker}; C2 Run claim={absent,current,stale};
    // C3 realization={absent,exact,foreign,zero revision}. The source accepts
    // only the current complete fence. R1 local without authority reads once
    // with None; R2 local/remote Run reads once with the same claim; R3 remote
    // realization supersedes a claim and reads once with the exact Session,
    // revision, input, owner and epoch; R4 stale claim or foreign Session reads
    // once, rejects, and returns no mount; R5 zero revision rejects before any
    // source read. No second File-specific fence may be constructed downstream.
    use crate::ResourceOperationFence as Fence;
    use awaken_dispatch_contract::RunClaim;
    use awaken_session_realization_contract::SessionResourceMaterializationIntent as Intent;

    let input = session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("file-binding"),
        source: session_resource::ResolvedInputSource::File {
            file_id: awaken_resource_contract::FileId::from("file-a"),
        },
        mount_path: "/file-a".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    };
    let current = RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("run-a".into()),
        owner: "worker-a".into(),
        epoch: 2,
    };
    let stale = RunClaim {
        epoch: 1,
        ..current.clone()
    };
    let lease = session_resource::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX,
    };
    let run = Fence::Run(current.clone());
    let stale_run = Fence::Run(stale.clone());
    let session = Fence::SessionMaterialization(
        Intent::try_new("session-a", 9, input.clone(), lease.clone()).unwrap(),
    );
    let foreign = Fence::SessionMaterialization(
        Intent::try_new("session-foreign", 9, input.clone(), lease.clone()).unwrap(),
    );
    let rules = [
        ("R1", false, None, None, None, None, true, 1),
        (
            "R2-local",
            false,
            Some(current.clone()),
            None,
            Some(run.clone()),
            Some(run.clone()),
            true,
            1,
        ),
        (
            "R2-remote",
            true,
            Some(current.clone()),
            None,
            Some(run.clone()),
            Some(run.clone()),
            true,
            1,
        ),
        (
            "R2-store-owning",
            false,
            Some(current.clone()),
            Some(("session-a", 9)),
            Some(run.clone()),
            Some(run.clone()),
            true,
            1,
        ),
        (
            "R3",
            true,
            Some(stale.clone()),
            Some(("session-a", 9)),
            Some(session.clone()),
            Some(session.clone()),
            true,
            1,
        ),
        (
            "R4-stale",
            true,
            Some(stale),
            None,
            Some(run.clone()),
            Some(stale_run),
            false,
            1,
        ),
        (
            "R4-foreign",
            true,
            None,
            Some(("session-foreign", 9)),
            Some(session),
            Some(foreign),
            false,
            1,
        ),
        (
            "R5",
            true,
            None,
            Some(("session-a", 0)),
            None,
            None,
            false,
            0,
        ),
    ];
    for (rule, remote, claim, realization, accepted, sent, succeeds, reads) in rules {
        let source = Arc::new(FileFenceProbe::new(accepted));
        let mut dependencies = dependencies()
            .await
            .with_file_content_source(source.clone());
        if remote {
            dependencies = dependencies.with_authority(crate::UpstreamAuthority::default());
        }
        let result = (dependencies)
            .build()
            .expect("validated Host dependencies")
            .realizer
            .staging
            .stage_resolved_input(
                "workspace",
                &input,
                claim.as_ref(),
                realization.map(|(session_id, revision)| (session_id, revision, &lease)),
                None,
            )
            .await;
        match (result, succeeds) {
            (Ok(staged), true) => assert_eq!(staged.mounts.len(), 1, "{rule}"),
            (Err(error), false) if rule == "R5" => assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::BadRequest,
                "{rule} rejects at the outer intent admission"
            ),
            (Err(error), false) => assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::Internal,
                "{rule} fails at the exact File source",
            ),
            (Ok(_), false) => panic!("{rule} unexpectedly accepted a foreign fence"),
            (Err(error), true) => panic!("{rule} rejected its exact fence: {error}"),
        }
        let observed = source.seen();
        assert_eq!(observed.len(), reads, "{rule}: source read count");
        if reads == 1 {
            assert_eq!(observed, vec![sent], "{rule}: complete fence identity");
        }
    }
}

#[tokio::test]
async fn terminal_repository_guard_rejects_file_before_any_source_read() {
    // C1 a terminal publication command carries a File instead of Repository;
    // R6 the existing intent guard rejects before FileContentSource, transport,
    // or physical effects. This is the excluded terminal arm of the File fence
    // decision table above, not a second terminal publication path. R10 uses
    // the original contract intent validation, which owns the kind decision;
    // deleting the unreachable second Stage guard preserves this first error.
    let input = session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("terminal-file"),
        source: session_resource::ResolvedInputSource::File {
            file_id: awaken_resource_contract::FileId::from("file-a"),
        },
        mount_path: "/file-a".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    };
    let command = awaken_session_state_contract::SessionRepositoryPublicationCommand {
        session_id: awaken_session_types::SessionId::from("session-a"),
        effect_id: "terminal-file".into(),
        intent: awaken_session_state_contract::SessionRepositoryPublicationIntent {
            input,
            expectation: pc::RepositoryPublicationExpectation {
                branch: "main".into(),
                commit: "a".repeat(40),
                expected_prior_commit: None,
            },
        },
    };
    let source = Arc::new(FileFenceProbe::new(None));
    let runtime = crate::ManagedHost::new(
        (dependencies()
            .await
            .with_file_content_source(source.clone()))
        .build()
        .expect("validated Host dependencies"),
    );
    let error = match runtime
        .host
        .realizer
        .staging
        .stage_terminal_repository_publication_input(
            "workspace",
            &command,
            &terminal_cleanup_test_lease(),
        )
        .await
    {
        Ok(_) => panic!("R6 terminal File unexpectedly accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind, awaken_session_types::RunErrorKind::BadRequest);
    assert!(source.seen().is_empty(), "R6 no File source read");
}
