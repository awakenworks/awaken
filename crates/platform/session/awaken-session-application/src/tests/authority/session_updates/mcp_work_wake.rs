//! Accepted MCP intent wakes the same Work identity after its root commit.
use super::*;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WorkWakeState {
    #[default]
    Stopped,
    Queued,
}

#[derive(Clone, Copy, Debug)]
enum WakeReply {
    Accepted,
    UnavailableBeforeWake,
    LostAcknowledgement,
}

/// Record the assignment port's effects and both sides of an ambiguous ACK.
/// PostgreSQL and the existing real-HTTP owner cover the actual queue backend.
#[derive(Default)]
struct McpWakeAssignment {
    state: Mutex<WorkWakeState>,
    calls: Mutex<Vec<(String, String)>>,
    replies: Mutex<VecDeque<WakeReply>>,
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerAssignment for McpWakeAssignment {
    async fn enqueue_session(&self, _: &str, _: &SessionId) -> Result<String, RunError> {
        panic!("MCP update recovery must use its accepted-intent wake role")
    }

    async fn wake_session(
        &self,
        environment: &str,
        session: &SessionId,
    ) -> Result<String, RunError> {
        self.calls
            .lock()
            .unwrap()
            .push((environment.into(), session.to_string()));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(WakeReply::Accepted);
        if matches!(reply, WakeReply::UnavailableBeforeWake) {
            return Err(RunError::unavailable("injected Work wake outage"));
        }
        *self.state.lock().unwrap() = WorkWakeState::Queued;
        if matches!(reply, WakeReply::LostAcknowledgement) {
            return Err(RunError::unavailable(
                "Work wake committed before its acknowledgement was lost",
            ));
        }
        Ok(format!("work:{session}"))
    }

    async fn retire_session(&self, _: &str, _: &SessionId) -> Result<(), RunError> {
        panic!("MCP update recovery must not retire Work")
    }
}

fn wake_root(id: &str) -> PersistedSession {
    let mut root = persisted(id, true, "idle");
    root.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: "registered-worker".into(),
        runtime_incarnation: "registered-worker:1:boot".into(),
        epoch: 3,
        expires_at_unix_ms: u64::MAX,
    });
    root
}

fn wake_draft(url: &str) -> awaken_session_configuration_contract::mcp::McpAttachmentDraft {
    awaken_session_configuration_contract::mcp::McpAttachmentDraft {
        name: "docs".into(),
        target: McpTarget::parse_http(url).unwrap(),
        prompts_as_skills: false,
        credential: None,
        origin: crate::session_mcp::McpAttachmentOrigin::Session,
    }
}

fn wake_command(key: &str, url: &str) -> SessionUpdateCommand {
    SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: None,
        tools: None,
        mcp_update: Some(SessionMcpUpdate::PublicReplacement(vec![
            McpAttachmentCandidate {
                name: "docs".into(),
                target: McpAttachmentCandidateTarget::HttpUrl(url.into()),
                prompts_as_skills: false,
                published_credential: None,
                origin: crate::session_mcp::McpAttachmentOrigin::Session,
            },
        ])),
        idempotency_key: Some(key.into()),
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(
            "mcp-work-wake",
            key,
            url,
        )),
        if_match: None,
    }
}

async fn wake_fixture(
    root: PersistedSession,
    assignments: Arc<McpWakeAssignment>,
) -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    crate::SessionApplication,
) {
    let repo = authority_repository("MCP Work wake SQLite repository").await;
    create(repo.as_ref(), root).await;
    let environment = Arc::new(RecordingEnvironmentSource::default());
    let probe = Arc::new(crate::ExecutionObservationProbe::default());
    let app = SessionApplicationBuilder::fixture(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        crate::SessionEnvironments {
            projection: environment.clone(),
            assignment: assignments,
            leases: environment,
        },
        crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .unwrap();
    (repo, app)
}

fn command_receipt_key(id: &SessionId, key: &str) -> String {
    format!(
        "managed:update-command:{id}:{}",
        crate::SessionUpdateCommands::update_operation_id(id.as_str(), key)
    )
}

#[tokio::test]
async fn accepted_mcp_update_wakes_stopped_work_and_exact_replay_reconciles_current_intent() {
    // W1 root CAS precedes wake; W2 duplicate delivery retains its root receipt;
    // W3 an old receipt reconciles current desired truth without reauthoring it.
    let id = SessionId::from("accepted-mcp-work-wake");
    let assignments = Arc::new(McpWakeAssignment::default());
    let (repo, app) = wake_fixture(wake_root(id.as_str()), assignments.clone()).await;
    assert_eq!(*assignments.state.lock().unwrap(), WorkWakeState::Stopped);
    let command = wake_command("wake-first", "https://first.example.test/mcp");
    let first = app
        .update_commands
        .update_session(&id, command.clone())
        .await
        .unwrap();
    assert!(first.command_applied);
    assert!(first.changes.mcp());
    assert!(first.session.is_run_free_preparation_candidate());
    assert_eq!(*assignments.state.lock().unwrap(), WorkWakeState::Queued);
    assert_eq!(
        assignments.calls.lock().unwrap().as_slice(),
        &[("env-worker".into(), id.to_string())]
    );
    let receipt = repo
        .idempotency_receipt(&id, &command_receipt_key(&id, "wake-first"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.committed_revision, first.command_revision);
    let replay = app
        .update_commands
        .update_session(&id, command.clone())
        .await
        .unwrap();
    assert!(!replay.command_applied);
    assert_eq!(replay.command_revision, first.command_revision);
    assert_eq!(replay.session, first.session);
    assert_eq!(assignments.calls.lock().unwrap().len(), 2);

    let second = app
        .update_commands
        .update_session(
            &id,
            wake_command("wake-second", "https://second.example.test/mcp"),
        )
        .await
        .unwrap();
    assert_ne!(
        second.session.mcp.desired_fingerprint,
        first.session.mcp.desired_fingerprint
    );
    let current = repo.get(&id).await.unwrap();
    let older_receipt = app
        .update_commands
        .update_session(&id, command)
        .await
        .unwrap();
    assert!(!older_receipt.command_applied);
    assert_eq!(older_receipt.command_revision, first.command_revision);
    assert_eq!(older_receipt.session, current);
    assert_eq!(repo.get(&id).await.unwrap(), current);
    assert_eq!(assignments.calls.lock().unwrap().len(), 4);
    assert_eq!(
        repo.idempotency_receipt(&id, &command_receipt_key(&id, "wake-first"))
            .await
            .unwrap()
            .unwrap(),
        receipt
    );
}

#[tokio::test]
async fn mcp_work_wake_failure_and_unknown_ack_replay_the_original_committed_command() {
    // F1 failure precedes the Work effect; F2 the effect succeeds but its ACK
    // fails. Both preserve the complete accepted root and repair through replay.
    for reply in [
        WakeReply::UnavailableBeforeWake,
        WakeReply::LostAcknowledgement,
    ] {
        let id = SessionId::from(format!("mcp-work-wake-{reply:?}"));
        let assignments = Arc::new(McpWakeAssignment::default());
        assignments.replies.lock().unwrap().push_back(reply);
        let (repo, app) = wake_fixture(wake_root(id.as_str()), assignments.clone()).await;
        let command = wake_command("wake-retry", "https://docs.example.test/mcp");
        let error = app
            .update_commands
            .update_session(&id, command.clone())
            .await
            .unwrap_err();
        let SessionUpdateError::ProjectionAfterCommit { outcome, source } = error else {
            panic!("{reply:?} must preserve its accepted command outcome: {error:?}");
        };
        assert_eq!(
            source.kind,
            awaken_runtime_contract::runs::RunErrorKind::Unavailable
        );
        assert!(outcome.command_applied);
        assert!(outcome.changes.mcp());
        assert!(outcome.session.is_run_free_preparation_candidate());
        assert_eq!(repo.get(&id).await.unwrap(), outcome.session);
        let receipt = repo
            .idempotency_receipt(&id, &command_receipt_key(&id, "wake-retry"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.committed_revision, outcome.command_revision);
        let expected_state = match reply {
            WakeReply::UnavailableBeforeWake => WorkWakeState::Stopped,
            WakeReply::LostAcknowledgement => WorkWakeState::Queued,
            WakeReply::Accepted => unreachable!(),
        };
        assert_eq!(*assignments.state.lock().unwrap(), expected_state);
        let replay = app
            .update_commands
            .update_session(&id, command)
            .await
            .unwrap();
        assert!(!replay.command_applied);
        assert_eq!(replay.command_revision, outcome.command_revision);
        assert_eq!(replay.session, outcome.session);
        assert_eq!(*assignments.state.lock().unwrap(), WorkWakeState::Queued);
        assert_eq!(assignments.calls.lock().unwrap().len(), 2);
        assert_eq!(
            repo.idempotency_receipt(&id, &command_receipt_key(&id, "wake-retry"))
                .await
                .unwrap()
                .unwrap(),
            receipt
        );
    }
}

fn claim_wake_generation(root: &mut PersistedSession) -> String {
    let generation = root.mcp.desired_attachments()[0].generation.clone();
    let lease = root.realization.as_ref().unwrap();
    let realization_id = format!("mcp-wake-realization:{}", root.session_id);
    root.mcp
        .claim_realization(
            &generation,
            crate::session_mcp::McpRealizationClaim {
                realization_id: realization_id.clone(),
                runtime_incarnation: lease.runtime_incarnation.clone(),
                lease_epoch: lease.epoch,
                lease_expires_at_unix_ms: lease.expires_at_unix_ms,
                stage_idempotency_key: format!("mcp-wake-stage:{}", root.session_id),
            },
        )
        .unwrap();
    realization_id
}

#[derive(Clone, Copy, Debug)]
enum WakeRootChange {
    DifferentDesired,
    RetryGeneration,
    Terminal,
    RealizationComplete,
    NonSelfHosted,
    ObservedAhead,
    SameHeadRealizing,
    SameHeadMetadata,
}

#[tokio::test]
async fn mcp_work_wake_rechecks_current_root_and_exact_desired_heads_before_every_effect() {
    // R1 stale desired or retry-generation snapshots cannot wake newer intent;
    // R2 terminal/completed/non-Work roots and regressed reads cannot wake;
    // R3 same-head phase or unrelated root advancement remains replayable.
    for change in [
        WakeRootChange::DifferentDesired,
        WakeRootChange::RetryGeneration,
        WakeRootChange::Terminal,
        WakeRootChange::RealizationComplete,
        WakeRootChange::NonSelfHosted,
        WakeRootChange::ObservedAhead,
        WakeRootChange::SameHeadRealizing,
        WakeRootChange::SameHeadMetadata,
    ] {
        let id = SessionId::from(format!("mcp-work-current-{change:?}"));
        let mut root = wake_root(id.as_str());
        if matches!(change, WakeRootChange::NonSelfHosted) {
            let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
                &mut root.baseline
            else {
                panic!("frozen fixture");
            };
            baseline.environment.self_hosted = false;
        }
        let holder = root
            .frozen_baseline()
            .unwrap()
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        let draft = wake_draft("https://docs.example.test/mcp");
        let assignments = Arc::new(McpWakeAssignment::default());
        let (repo, app) = wake_fixture(root, assignments.clone()).await;
        let accepted = app
            .update_commands
            .update_session(
                &id,
                wake_command("current-root-original", "https://docs.example.test/mcp"),
            )
            .await
            .unwrap();
        let mut observed = accepted.session;
        assert!(observed.is_run_free_preparation_candidate());
        let mut current = observed.clone();
        match change {
            WakeRootChange::DifferentDesired => {
                current = app
                    .update_commands
                    .update_session(
                        &id,
                        wake_command(
                            "current-root-successor",
                            "https://successor.example.test/mcp",
                        ),
                    )
                    .await
                    .unwrap()
                    .session;
            }
            WakeRootChange::RetryGeneration => {
                let generation = current.mcp.desired_attachments()[0].generation.clone();
                let realization = claim_wake_generation(&mut current);
                current
                    .mcp
                    .fail_realization(&generation, &realization, "fixture failed generation")
                    .unwrap();
                current
                    .mcp
                    .request_full_replacement(vec![draft], Some(holder))
                    .unwrap();
                assert_eq!(
                    current.mcp.desired_fingerprint,
                    observed.mcp.desired_fingerprint
                );
                assert_ne!(current.mcp.desired_attachments()[0].generation, generation);
            }
            WakeRootChange::Terminal => current.execution = SessionExecutionState::Terminated,
            WakeRootChange::RealizationComplete => {
                let generation = current.mcp.desired_attachments()[0].generation.clone();
                let realization = claim_wake_generation(&mut current);
                current.mcp.activate(&generation, &realization).unwrap();
                current
                    .mcp
                    .acknowledge_publication(&generation, &realization)
                    .unwrap();
                assert!(!current.mcp.needs_realization());
            }
            WakeRootChange::NonSelfHosted => {}
            WakeRootChange::ObservedAhead => {
                observed.revision = awaken_session_types::SessionRevision(current.revision.0 + 2);
            }
            WakeRootChange::SameHeadRealizing => {
                claim_wake_generation(&mut current);
            }
            WakeRootChange::SameHeadMetadata => {
                let mut command =
                    wake_command("current-root-metadata", "https://docs.example.test/mcp");
                command.mcp_update = None;
                command.metadata = Some(SessionMetadataUpdate::Patch(
                    [("fixture".into(), Some("advanced".into()))]
                        .into_iter()
                        .collect(),
                ));
                current = app
                    .update_commands
                    .update_session(&id, command)
                    .await
                    .unwrap()
                    .session;
            }
        }
        let committed = if matches!(
            change,
            WakeRootChange::DifferentDesired
                | WakeRootChange::SameHeadMetadata
                | WakeRootChange::NonSelfHosted
        ) {
            current
        } else {
            let owner = app.mutations.owner(&id).await.unwrap();
            app.mutations
                .commit_session_snapshot_with_outcome(
                    &owner,
                    awaken_session_contract::SessionAggregate::try_from(current).unwrap(),
                    "mcp-work-wake-fixture-current-root",
                    Vec::new(),
                )
                .await
                .unwrap()
                .into_session()
        };
        let calls_before = assignments.calls.lock().unwrap().len();
        let state_before = *assignments.state.lock().unwrap();
        let permits = matches!(
            change,
            WakeRootChange::SameHeadRealizing | WakeRootChange::SameHeadMetadata
        );
        app.work_assignments
            .wake_for_mcp_update(&observed)
            .await
            .unwrap();
        app.work_assignments
            .wake_for_mcp_update(&observed)
            .await
            .unwrap();
        assert_eq!(
            assignments.calls.lock().unwrap().len(),
            calls_before + if permits { 2 } else { 0 },
            "{change:?}"
        );
        assert_eq!(
            *assignments.state.lock().unwrap(),
            if permits {
                WorkWakeState::Queued
            } else {
                state_before
            },
            "{change:?}"
        );
        assert_eq!(
            repo.get(&id).await.unwrap(),
            committed,
            "{change:?} wake never mutates the Session root"
        );
    }
}
