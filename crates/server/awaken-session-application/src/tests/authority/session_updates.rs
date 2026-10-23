//! Session update CAS and its original realization retry are one authority.
use super::*;

#[derive(Default)]
struct FailOnceMcpRealizer {
    stage_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl McpAttachmentRealizer for FailOnceMcpRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        if self.stage_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(RunError::unavailable("injected MCP stage outage"));
        }
        McpAttachmentRealizer::stage_mcp_attachment(&NoopMcpRealizer, request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        McpAttachmentRealizer::publish_mcp_generation(&NoopMcpRealizer, generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        McpAttachmentRealizer::drain_mcp_generation(&NoopMcpRealizer, generation).await
    }
}

#[tokio::test]
async fn mixed_update_commits_one_receipted_root_before_realization_and_replays_the_effect() {
    // Cause/effect graph: C1 one command mixes title, metadata, budget, and MCP
    // Agent desired state; C2 its root CAS and durable command receipt commit;
    // C3 MCP realization succeeds or fails retryably; C4 the caller replays the
    // exact command after C3 failure. Effects: E1 every desired field and MCP
    // generation share the single next root revision named by the receipt; E2 a
    // C3 failure is ProjectionAfterCommit carrying that complete committed
    // outcome; E3 C4 keeps the original command revision and allocates exactly
    // the aggregate-owned N+1 recovery generation, then realizes durable truth
    // without re-authoring any public field.
    // Constraint: normalization is pure before C2; no title/metadata/budget or
    // MCP intent may commit through an independent command mutation.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | M1 | yes | commits | retryable failure | no | E1 + E2 |
    // | M2 | same command | already committed | succeeds | exact | E3 |
    let session_id = "mixed-update-one-root";
    let repo = authority_repository("mixed update repository").await;
    let mut session = persisted(session_id, false, "idle");
    session.budget = awaken_session_budget_contract::SessionBudgetState::Active {
        max_list_cost_minor: 10,
        consumed_numerator: 0,
        usage_cursor: Default::default(),
        snapshot: awaken_session_budget_contract::ManagedListPriceSnapshot {
            snapshot_id: "mixed-prices-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "mixed-prices-v1-fingerprint".into(),
        },
        reach_transitions: Vec::new(),
    };
    create(repo.as_ref(), session).await;
    let before = repo
        .get(&SessionId::from(session_id))
        .await
        .expect("initial mixed Session");
    let command_revision = awaken_session_types::SessionRevision(
        before
            .revision
            .0
            .checked_add(1)
            .expect("test command revision"),
    );
    let realizer = Arc::new(FailOnceMcpRealizer::default());
    let app = SessionApplicationBuilder::fixture(
        Arc::new(NoopRuntime),
        realizer.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .expect("valid fixture Session application");
    let idempotency_key = "mixed-update-key";
    let command = SessionUpdateCommand {
        title: Some(SessionFieldUpdate::Replace("one root".into())),
        metadata: Some(SessionMetadataUpdate::Patch(
            std::collections::BTreeMap::from([("owner".into(), Some("session".into()))]),
        )),
        budget: Some(SessionFieldUpdate::Replace(20)),
        tools: None,
        mcp_update: Some(SessionMcpUpdate::PublicReplacement(vec![
            McpAttachmentCandidate {
                name: "docs".into(),
                target: McpAttachmentCandidateTarget::HttpUrl(
                    "https://docs.example.test/mcp".into(),
                ),
                prompts_as_skills: false,
                published_credential: None,
                origin: crate::session_mcp::McpAttachmentOrigin::Session,
            },
        ])),
        idempotency_key: Some(idempotency_key.into()),
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(
            "mixed-update-one-root-v1",
            session_id,
        )),
        if_match: None,
    };

    let (committed, failure) = match app
        .update_commands
        .update_session(&SessionId::from(session_id), command.clone())
        .await
    {
        Err(SessionUpdateError::ProjectionAfterCommit { outcome, source }) => (*outcome, source),
        other => panic!("M1/E2 expected committed realization failure, got {other:?}"),
    };
    assert_eq!(
        failure.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "M1/E2"
    );
    assert_eq!(committed.command_revision, command_revision, "M1/E1");
    assert!(committed.command_applied, "M1/E1");
    assert_eq!(
        [
            committed.changes.title(),
            committed.changes.metadata(),
            committed.changes.tools(),
            committed.changes.mcp(),
            committed.changes.budget(),
        ],
        [true, true, false, true, true],
        "M1/E1 complete mixed command"
    );
    assert_eq!(
        committed.session.revision, command_revision,
        "M1/E1 one command CAS"
    );
    assert_eq!(
        committed.session.title.as_deref(),
        Some("one root"),
        "M1/E1"
    );
    assert_eq!(
        committed.session.metadata.get("owner").map(String::as_str),
        Some("session"),
        "M1/E1"
    );
    assert_eq!(
        committed.session.budget.max_list_cost_minor(),
        Some(20),
        "M1/E1"
    );
    assert_eq!(
        committed.session.mcp.desired_attachments().len(),
        1,
        "M1/E1"
    );

    let receipt_key = format!(
        "managed:update-command:{session_id}:{}",
        SessionUpdateCommands::update_operation_id(session_id, idempotency_key)
    );
    let receipt = repo
        .idempotency_receipt(&SessionId::from(session_id), &receipt_key)
        .await
        .expect("M1 receipt read")
        .expect("M1 durable command receipt");
    assert_eq!(receipt.committed_revision, command_revision, "M1/E1");
    let durable_after_failure = repo
        .get(&SessionId::from(session_id))
        .await
        .expect("M1 durable mixed truth");
    assert_eq!(
        durable_after_failure.title.as_deref(),
        Some("one root"),
        "M1/E2"
    );
    assert_eq!(
        durable_after_failure.budget.max_list_cost_minor(),
        Some(20),
        "M1/E2"
    );
    assert_eq!(realizer.stage_calls.load(Ordering::SeqCst), 1, "M1/E2");

    let replayed = app
        .update_commands
        .update_session(&SessionId::from(session_id), command)
        .await
        .expect("M2 exact replay repairs realization");
    assert!(!replayed.command_applied, "M2/E3");
    assert_eq!(replayed.command_revision, command_revision, "M2/E3");
    assert_eq!(replayed.changes, SessionUpdateChanges::default(), "M2/E3");
    assert_eq!(replayed.session.title.as_deref(), Some("one root"), "M2/E3");
    assert_eq!(
        replayed.session.budget.max_list_cost_minor(),
        Some(20),
        "M2/E3"
    );
    let desired = replayed.session.mcp.desired_attachments();
    assert_eq!(desired.len(), 1, "M2/E3");
    assert_eq!(
        replayed.session.mcp.attachments.len(),
        2,
        "M2/E3 one failed generation plus one exact recovery generation"
    );
    assert_eq!(
        desired[0].generation.version(),
        2,
        "M2/E3 Failed -> N+1 uses the canonical MCP retry rule"
    );
    assert_eq!(
        desired[0].state,
        crate::session_mcp::McpAttachmentState::Active,
        "M2/E3"
    );
    assert!(desired[0].publication_acknowledged, "M2/E3");
    assert_eq!(realizer.stage_calls.load(Ordering::SeqCst), 2, "M2/E3");
    assert_eq!(
        repo.idempotency_receipt(&SessionId::from(session_id), &receipt_key)
            .await
            .expect("M2 receipt read")
            .expect("M2 durable command receipt")
            .committed_revision,
        command_revision,
        "M2/E3 receipt remains the original command CAS"
    );
}
