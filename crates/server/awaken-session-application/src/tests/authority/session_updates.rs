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
async fn external_mcp_update_preserves_the_worker_lease_and_queues_the_same_root() {
    // Cause/effect graph: C1 an Idle, externally placed Session retains a live
    // registered Worker lease; C2 this Coordinator also has co-located roles;
    // C3 one MCP replacement commits; C4 its exact command is replayed.
    // E1 C1+C2+C3 persists the new desired generation without trying a local
    // realization or replacing the Worker's lease; E2 the SAME Session root
    // becomes Worker preparation work without creating a Run; E3 C4 retains
    // the command revision, generation and lease without another local effect.
    // Rules U1=C1+C2+C3=>E1+E2; U2=U1+C4=>E3. The existing mixed-update test
    // covers the co-located success/failure/replay rows over the same command.
    let id = SessionId::from("external-mcp-update");
    let repo = authority_repository("external MCP update repository").await;
    let mut root = persisted(id.as_str(), false, "idle");
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut root.baseline
    else {
        panic!("frozen fixture");
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "registered-worker".into(),
        runtime_incarnation: "registered-worker:1:boot".into(),
        epoch: 3,
        expires_at_unix_ms: u64::MAX,
    };
    root.realization = Some(lease.clone());
    create(repo.as_ref(), root).await;
    let local_effects = Arc::new(FailOnceMcpRealizer::default());
    let app = SessionApplicationBuilder::fixture(
        Arc::new(NoopRuntime),
        local_effects.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .expect("valid co-located process fixture");
    let command = SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: None,
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
        idempotency_key: Some("external-update-key".into()),
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(
            "external-mcp-update-v1",
            id.as_str(),
        )),
        if_match: None,
    };
    let first = app
        .update_commands
        .update_session(&id, command.clone())
        .await
        .expect("U1 external desired state awaits its registered Worker");
    assert!(first.command_applied, "U1/E1");
    assert_eq!(first.session.realization.as_ref(), Some(&lease), "U1/E1");
    assert_eq!(local_effects.stage_calls.load(Ordering::SeqCst), 0, "U1/E1");
    assert!(
        first.session.is_run_free_preparation_candidate(),
        "U1/E2 pending MCP is root-owned Worker preparation"
    );
    let replay = app
        .update_commands
        .update_session(&id, command)
        .await
        .expect("U2 exact replay retains pending preparation");
    assert!(!replay.command_applied, "U2/E3");
    assert_eq!(replay.command_revision, first.command_revision, "U2/E3");
    assert_eq!(replay.session.mcp, first.session.mcp, "U2/E3");
    assert_eq!(replay.session.realization.as_ref(), Some(&lease), "U2/E3");
    assert_eq!(local_effects.stage_calls.load(Ordering::SeqCst), 0, "U2/E3");
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
    // Refactoring coverage: preparing the MCP replacement borrows this same
    // aggregate; M1's single revision/receipt assertions prove preparation adds
    // no commit, and M2 proves it is not repeated after a committed command.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | M1 | yes | commits | retryable failure | no | E1 + E2 |
    // | M2 | same command | already committed | succeeds | exact | E3 |
    let session_id = "mixed-update-one-root";
    let repo = authority_repository("mixed update repository").await;
    let mut session = persisted(session_id, false, "idle");
    session.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 10,
        consumed_numerator: 0,
        usage_cursor: Default::default(),
        snapshot: awaken_session_contract::ManagedListPriceSnapshot {
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

const RENEWAL_SOURCE: &str = "unkeyed-renewal-source";

fn renewal_access(
    revision: u64,
    holder: &awaken_credential_contract::PlaintextHolder,
) -> awaken_credential_contract::CredentialAccess {
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy,
    };
    CredentialAccess::new(
        CredentialRef {
            id: RENEWAL_SOURCE.into(),
            revision,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
        CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
    )
}

struct RenewedMcpCredentialSource;

#[async_trait::async_trait]
impl awaken_credential_contract::SessionCredentialSource for RenewedMcpCredentialSource {
    async fn has_vault(&self, _: &str, _: &str) -> Result<bool, String> {
        unreachable!("the explicit credential pin does not discover Vaults")
    }

    async fn mcp_credential_source_for_url(
        &self,
        _: &str,
        _: &[String],
        _: &str,
    ) -> Result<Option<awaken_credential_contract::CredentialSourceId>, String> {
        unreachable!("the explicit pin must not fall back to URL discovery")
    }

    async fn mcp_access_for_source(
        &self,
        source: &awaken_credential_contract::CredentialSourceId,
        workspace: &str,
        target: &McpTarget,
        holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
        assert_eq!(source.0, RENEWAL_SOURCE);
        assert_eq!(target.http_url(), Some("https://docs.example.test/mcp"));
        let usage = awaken_credential_contract::CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        };
        assert_eq!(
            binding,
            &awaken_credential_contract::CredentialMaterialBinding::for_target(
                workspace, target, &usage,
            )
        );
        Ok(renewal_access(4, holder))
    }

    async fn credential_access_for_source(
        &self,
        _: &awaken_credential_contract::CredentialSourceId,
        _: &str,
        _: awaken_credential_contract::SessionCredentialAccessRequest,
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
        unreachable!("MCP renewal must use its existing MCP credential role")
    }
}

async fn assert_unkeyed_credential_renewal(legacy: bool) {
    // C1 fresh accepted versus genuinely absent legacy history; C2 only the
    // exact credential revision changes; C3 no idempotency key and no authoring
    // field change. Both rows must commit through the application before any
    // realization, retaining identical Agent/history values. Replacing the
    // mcp_changed commit predicate with changes.mcp() must fail these rows;
    // command_record.is_some() cannot conceal that regression here.
    let id = format!("unkeyed-credential-renewal-{legacy}");
    let repo = authority_repository("unkeyed renewal SQLite repository").await;
    let original = persisted(&id, false, "idle");
    let baseline = original.frozen_baseline().unwrap().clone();
    let holder = baseline
        .environment
        .credential_realization
        .mcp_holder
        .clone();
    let target = McpTarget::parse_http("https://docs.example.test/mcp").unwrap();
    let mcp = crate::session_mcp::SessionMcpAttachmentSet::from_initial(
        vec![
            awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                name: "docs".into(),
                target: target.clone(),
                prompts_as_skills: false,
                credential: Some(renewal_access(3, &holder)),
                origin: crate::session_mcp::McpAttachmentOrigin::Session,
            },
        ],
        Some(holder),
    )
    .unwrap();
    let mut root =
        awaken_session_contract::SessionAggregate::try_from(PersistedSession::frozen_with_budget(
            &id,
            baseline.clone(),
            original.resources,
            mcp,
            original.title,
            original.metadata,
            original.tools,
            original.budget,
        ))
        .unwrap();
    let mut configuration = crate::agent_configuration::agent_configuration_from_profile(
        &baseline.agent_id,
        awaken_executable_agent_contract::ExecutableAgentSessionProfile {
            source_revision: baseline.agent_revision.unwrap_or(0),
            model: Some(baseline.model.clone()),
            ..Default::default()
        },
    );
    if let Some(selected) = &baseline.model_override {
        configuration.model.inference = selected.inference.clone();
        configuration.model.source =
            awaken_session_configuration_contract::SessionModelSource::Override;
    }
    root.record_initial_agent_configuration(Box::new(configuration))
        .unwrap();
    let mut session = root.into_snapshot();
    session.execution = SessionExecutionState::Idle;
    if legacy {
        let mut wire = serde_json::to_value(session).unwrap();
        for field in ["initial_agent_configuration", "update_history"] {
            wire.as_object_mut().unwrap().remove(field);
        }
        session = serde_json::from_value(wire).unwrap();
    }
    create(repo.as_ref(), session).await;
    let session_id = SessionId::from(id.as_str());
    let before = repo.get(&session_id).await.unwrap();
    assert_eq!(
        awaken_session_contract::SessionUpdateHistory::agent_configuration(&before).is_some(),
        !legacy,
    );
    let generation_before = before.mcp.desired_attachments()[0].generation.version();
    let mut app = SessionApplicationBuilder::fixture(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .unwrap();
    app.set_credential_source(Arc::new(RenewedMcpCredentialSource));
    let outcome = app
        .update_commands
        .update_session(
            &session_id,
            SessionUpdateCommand {
                title: None,
                metadata: None,
                budget: None,
                tools: None,
                mcp_update: Some(SessionMcpUpdate::CredentialLifecycle {
                    source_id: RENEWAL_SOURCE.into(),
                    revoked: false,
                    candidates: vec![McpAttachmentCandidate {
                        name: "docs".into(),
                        target: McpAttachmentCandidateTarget::Normalized(target),
                        prompts_as_skills: false,
                        published_credential: Some((RENEWAL_SOURCE.into(), 4)),
                        origin: crate::session_mcp::McpAttachmentOrigin::Session,
                    }],
                }),
                idempotency_key: None,
                request_fingerprint: "unkeyed-credential-revision-4".into(),
                if_match: Some(before.revision),
            },
        )
        .await
        .expect("credential-only update commits through its application owner");
    assert_eq!(outcome.changes, SessionUpdateChanges::default());
    assert!(outcome.command_applied);
    assert_eq!(
        outcome.command_revision.0,
        before.revision.0 + 1,
        "credential-only command must commit before realization"
    );
    let after = repo.get(&session_id).await.unwrap();
    let desired = after.mcp.desired_attachments();
    assert_eq!(desired.len(), 1);
    let pin = desired[0].credential.as_ref().unwrap();
    assert_eq!(pin.credential.id, RENEWAL_SOURCE);
    assert_eq!(
        pin.credential.revision, 4,
        "actual durable credential advance"
    );
    assert!(desired[0].generation.version() > generation_before);
    assert_ne!(
        after.mcp.desired_fingerprint,
        before.mcp.desired_fingerprint
    );
    assert_eq!(
        after.configured_mcp_servers(),
        before.configured_mcp_servers()
    );
    assert_eq!(
        awaken_session_contract::SessionUpdateHistory::for_session(&after),
        awaken_session_contract::SessionUpdateHistory::for_session(&before),
    );
    assert_eq!(
        awaken_session_contract::SessionUpdateHistory::agent_configuration(&after),
        awaken_session_contract::SessionUpdateHistory::agent_configuration(&before),
    );
}

#[tokio::test]
async fn credential_renewal_without_idempotency_commits_operational_mcp_without_agent_fact() {
    assert_unkeyed_credential_renewal(false).await;
}

#[tokio::test]
async fn legacy_credential_renewal_without_idempotency_commits_operational_mcp_without_agent_fact()
{
    assert_unkeyed_credential_renewal(true).await;
}
