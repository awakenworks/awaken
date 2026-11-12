//! The production-selected PostgreSQL writer and real Dispatch transitions.

use super::*;
use awaken_agent_contract::agent::message::{Id as MessageId, Role};
use awaken_agent_contract::agent::run::EndCause;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::lifecycle::RunLifecycleEventKind;
use awaken_agent_contract::thread::read::transcript::TranscriptView;
use awaken_dispatch_contract::{
    DispatchClaims, DispatchEnqueue, DispatchExecution, DispatchSettlement, RunDispatch,
    SettleOutcome,
};
use awaken_dispatch_erasure::DispatchBinding;
use awaken_runtime_contract::runs::RunErrorKind;
use awaken_worker_contract::RunClaim;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

async fn fixture() -> Option<(String, Arc<DurableRuntimeAuthority>, PgPool)> {
    let url = awaken_reliability_testkit::postgres_fixture_url()?;
    // A single connection distinguishes one transaction from an outer claim
    // lock that waits for a second checkout from the same process pool.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .unwrap();
    let mut storage = tests::test_storage();
    storage.store = StoreKind::Postgres;
    storage.dispatch_backend = DispatchBackend::Postgres;
    let authority = DurableRuntimeAuthority::open_with_postgres_pools(
        &storage,
        SchemaAccess::Migrate,
        Some(pool.clone()),
        Some(
            awaken_sql_storage::postgres_pool_options()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(5))
                .connect(&url)
                .await
                .unwrap(),
        ),
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    Some((url, authority, pool))
}

async fn initial_claim(authority: &DurableRuntimeAuthority) -> ClaimedCommitCommand {
    claim_dispatch(
        authority,
        RunDispatch::new(awaken_dispatch_testkit::worker_http::activation(
            "atomic-claim",
        )),
    )
    .await
}

#[tokio::test]
async fn postgres_session_replay_keeps_a_child_before_the_parent_receipt_across_takeover() {
    use awaken_agent_contract::{ExecutionScopeRef, ScopeId};
    use awaken_session_configuration_contract::{SessionBaseline, SessionBaselineInputs};
    use awaken_session_contract::{PersistedSession, SessionMutationPayload, SessionRepository};
    use awaken_session_execution_contract::{
        coordinated_activation_input, coordinated_run_id, coordinated_thread_id,
    };
    use awaken_session_types::SessionId;
    use awaken_tool_contract::tool::{ToolCall, ToolOutput};
    use awaken_tool_contract::tool_batch::ToolBatch;

    let Some((url, authority, pool)) = fixture().await else {
        return;
    };
    let store = awaken_session_store::PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .unwrap();
    let scope = "workspace-replay";
    let session = SessionId::from("session-replay");
    let root = ThreadId(session.to_string());
    let baseline = SessionBaseline::compile(SessionBaselineInputs {
        environment: awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "replay-environment".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "replay".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        },
        runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
        mcp_authoring: Default::default(),
        agent_id: "agent-parent".into(),
        agent_revision: None,
        model_override: None,
        toolsets: Vec::new(),
        model: "test".into(),
        runtime: None,
        delegate_ids: Vec::new(),
        mounts: Vec::new(),
        env: Vec::new(),
        prompts: Vec::new(),
        transcript_prefix: None,
    });
    let state = PersistedSession::frozen_with_budget(
        session.clone(),
        baseline,
        Default::default(),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let idempotency = SessionMutationPayload::default_create_idempotency(&state);
    store
        .create(scope, state, idempotency, Vec::new())
        .await
        .unwrap();

    let mut parent = awaken_dispatch_testkit::worker_http::activation("parent");
    parent.thread_id = root.clone();
    let parent_run = parent.run_id.clone();
    let parent = claim_dispatch(
        &authority,
        RunDispatch::new(parent)
            .for_session(root.clone())
            .with_session_activity_epoch(1)
            .with_execution_scope(ExecutionScopeRef(ScopeId::from(scope))),
    )
    .await;
    let mut batch = ToolBatch::for_step(
        parent_run.clone(),
        0,
        [(
            ToolCall {
                call_id: "send".into(),
                tool_id: "send_message".into(),
                arguments: serde_json::json!({"agent_id": "agent-child", "message": "investigate"}),
            },
            Default::default(),
        )],
    )
    .unwrap();
    batch.mark_executing("send").unwrap();
    let operation_id = batch.operation_id("send");
    let child_thread = coordinated_thread_id(&session, &parent_run, &operation_id);
    let child_run = coordinated_run_id(&session, &child_thread, &parent_run, &operation_id);
    let writer = authority.clone().run_commits();
    writer
        .commit_operation(replay_batch_commit(&parent.claim, &root, 0, 0, &batch))
        .await
        .unwrap();

    let service = crate::session_replay::PostgresSessionReplay::new(pool.clone());
    let before_child = service.capture_for(scope, &session).await.unwrap();
    let before = service
        .load_for(scope, &session, &before_child.id)
        .await
        .unwrap();
    assert_eq!(before.threads.len(), 1);
    assert_eq!(before.messages[0].target_first_commit, None);

    let mut child = awaken_dispatch_testkit::worker_http::activation("child");
    child.thread_id = child_thread.clone();
    child.run_id = child_run.clone();
    child.input = coordinated_activation_input(
        session.as_str(),
        &child_thread,
        &child_run,
        &operation_id,
        "investigate".into(),
    );
    child.delegation_origin = Some(awaken_dispatch_execution::coordinated_run_origin(
        session.as_str(),
        &operation_id,
        parent_run.clone(),
        "send".into(),
        "agent-parent".into(),
    ));
    let child = claim_dispatch(
        &authority,
        RunDispatch::new(child)
            .for_session(root.clone())
            .with_session_activity_epoch(2)
            .with_execution_scope(ExecutionScopeRef(ScopeId::from(scope))),
    )
    .await;
    use crate::session_replay::timeline::{run_evidence_at, timeline};
    let page_query = awaken_session_application::replay::TimelineQuery {
        from_ms: None,
        to_ms: None,
        thread_id: None,
        run_id: None,
        operation_id: None,
        source: None,
        limit: 1,
    };
    let queued = timeline(&service, scope, &session, &page_query, None)
        .await
        .unwrap();
    let child_evidence = run_evidence_at(&service, scope, &session, &child_run, &queued.snapshot)
        .await
        .unwrap();
    assert!(child_evidence.admission.is_some());
    assert!(child_evidence.activation.is_none());
    assert!(child_evidence.runtime.is_none());
    assert!(child_evidence.dispatch.is_some());
    assert_eq!(child_evidence.parent_run_id.as_ref(), Some(&parent_run));
    assert_eq!(child_evidence.messages.len(), 1);

    let mut child = child;
    child.operation.commit.messages.clear();
    child.operation.payload_hash = commit_payload_hash(&child.operation.commit).unwrap();
    sqlx::query("ALTER TABLE runtime_commit_receipt ADD CONSTRAINT reject_child_replay CHECK (false) NOT VALID")
        .execute(&pool).await.unwrap();
    assert!(
        writer
            .commit_operation(child.clone())
            .await
            .unwrap_err()
            .message
            .contains("reject_child_replay")
    );
    let uncommitted = service.capture_for(scope, &session).await.unwrap();
    let uncommitted = service
        .load_for(scope, &session, &uncommitted.id)
        .await
        .unwrap();
    assert_eq!(uncommitted.threads.len(), 1);
    assert_eq!(uncommitted.activations.len(), 1);
    sqlx::query("ALTER TABLE runtime_commit_receipt DROP CONSTRAINT reject_child_replay")
        .execute(&pool)
        .await
        .unwrap();
    let first = writer.commit_operation(child.clone()).await.unwrap();

    let gap = service.capture_for(scope, &session).await.unwrap();
    let frozen = service.load_for(scope, &session, &gap.id).await.unwrap();
    assert_eq!(frozen.threads.len(), 2);
    assert_eq!(frozen.messages.len(), 1);
    assert_eq!(
        frozen.messages[0].target_first_commit,
        Some(first.commit_sequence)
    );
    assert_eq!(frozen.messages[0].target_input_commit, None);
    assert!(matches!(
        frozen.messages[0].source,
        awaken_session_application::replay::ReplayAgentMessageSource::Send {
            receipt_at: None,
            ..
        }
    ));

    let dispatch = authority.dispatch_store();
    assert_eq!(
        dispatch.relinquish_claim(&child.claim).await.unwrap(),
        SettleOutcome::Applied
    );
    let successor = dispatch
        .claim("replay-successor", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let successor = RunClaim::from(&successor.lease);
    assert_eq!(
        writer
            .commit_operation(child.clone())
            .await
            .unwrap_err()
            .code,
        awaken_dispatch_contract::COMMIT_FENCED
    );
    let duplicate = writer
        .commit_operation(ClaimedCommitCommand {
            claim: successor.clone(),
            operation: child.operation,
        })
        .await
        .unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.commit_sequence, first.commit_sequence);

    // A terminal child keeps the original activation after Dispatch deletes
    // the active row. Its input and the root report use distinct commits.
    let mut child_input = coordinated_activation_input(
        session.as_str(),
        &child_thread,
        &child_run,
        &operation_id,
        "investigate".into(),
    );
    child_input.push(Message::text(
        MessageId::assistant(&child_run, 1),
        Role::Assistant,
        "done",
    ));
    let child_terminal = writer
        .commit_operation(replay_terminal_commit(
            &successor,
            &child_thread,
            1,
            1,
            child_input,
        ))
        .await
        .unwrap();
    assert_eq!(
        dispatch
            .settle(
                &child_run,
                successor.epoch,
                awaken_dispatch_contract::DispatchOutcome::Done,
                &[]
            )
            .await
            .unwrap(),
        SettleOutcome::Applied
    );
    use awaken_dispatch_contract::{SessionRunAdmissionObservation, SessionRunAdmissionReads};
    let SessionRunAdmissionObservation::Completed { completion, .. } = dispatch
        .observe_session_run_admission(&child_run)
        .await
        .unwrap()
    else {
        panic!("settled child retains completion evidence")
    };
    let activation = authority
        .archived_run_activation(session.as_str(), &child_thread, &child_run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        completion.request_fingerprint,
        Some(activation.admission_fingerprint)
    );
    let after_child = service.capture_for(scope, &session).await.unwrap();
    let before_receipt = service
        .load_for(scope, &session, &after_child.id)
        .await
        .unwrap();
    assert!(matches!(
        before_receipt.messages[0].source,
        awaken_session_application::replay::ReplayAgentMessageSource::Send {
            receipt_at: None,
            ..
        }
    ));
    assert_eq!(
        before_receipt.messages[0].target_input_commit,
        Some(child_terminal.commit_sequence)
    );

    let diagnostic_before = timeline(&service, scope, &session, &page_query, None)
        .await
        .unwrap();
    let old_child = run_evidence_at(
        &service,
        scope,
        &session,
        &child_run,
        &diagnostic_before.snapshot,
    )
    .await
    .unwrap();
    assert!(old_child.runtime.is_some());
    assert!(old_child.activation.is_some());
    assert_eq!(old_child.messages, before_receipt.messages);

    let receipt = serde_json::to_string(&awaken_ext_builtin_tools::AgentMessageReceipt {
        accepted: true,
        session_thread_id: child_thread.0.clone(),
    })
    .unwrap();
    batch.complete(ToolOutput::ok("send", receipt)).unwrap();
    let parent_receipt = writer
        .commit_operation(replay_batch_commit(&parent.claim, &root, 1, 1, &batch))
        .await
        .unwrap();
    let after_receipt = service.capture_for(scope, &session).await.unwrap();
    let reopened =
        crate::session_replay::PostgresSessionReplay::new(PgPool::connect(&url).await.unwrap());
    let mut diagnostic_page = diagnostic_before.clone();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        assert_eq!(diagnostic_page.snapshot, diagnostic_before.snapshot);
        for row in &diagnostic_page.entries {
            assert!(seen.insert((format!("{:?}", row.evidence.source), row.evidence.position)));
            assert!(
                !(row.evidence.source
                    == awaken_session_application::replay::EvidenceSource::Runtime
                    && row.evidence.position
                        == i64::try_from(parent_receipt.commit_sequence).unwrap())
            );
        }
        let Some(cursor) = diagnostic_page.next_cursor else {
            break;
        };
        diagnostic_page = timeline(&reopened, scope, &session, &page_query, Some(&cursor))
            .await
            .unwrap();
    }
    assert!(seen.len() > 3);
    assert_eq!(
        run_evidence_at(
            &reopened,
            scope,
            &session,
            &child_run,
            &diagnostic_before.snapshot
        )
        .await
        .unwrap(),
        old_child,
    );
    let diagnostic_after = timeline(&reopened, scope, &session, &page_query, None)
        .await
        .unwrap();
    let new_child = run_evidence_at(
        &reopened,
        scope,
        &session,
        &child_run,
        &diagnostic_after.snapshot,
    )
    .await
    .unwrap();
    assert!(matches!(
        new_child.messages[0].source,
        awaken_session_application::replay::ReplayAgentMessageSource::Send { receipt_at: Some(sequence), .. }
            if sequence == parent_receipt.commit_sequence
    ));
    assert!(matches!(
        run_evidence_at(
            &reopened,
            "foreign",
            &session,
            &child_run,
            &diagnostic_after.snapshot
        )
        .await,
        Err(awaken_session_application::replay::SessionReplayError::NotFound)
    ));
    let latest = reopened
        .load_for(scope, &session, &after_receipt.id)
        .await
        .unwrap();
    assert_eq!(latest.messages.len(), 1);
    assert!(
        matches!(latest.messages[0].source, awaken_session_application::replay::ReplayAgentMessageSource::Send { receipt_at: Some(sequence), .. } if sequence == parent_receipt.commit_sequence)
    );
    assert_eq!(latest.activations.len(), 2);

    writer
        .commit_operation(replay_terminal_commit(
            &parent.claim,
            &root,
            2,
            2,
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(
        dispatch
            .settle(
                &parent_run,
                parent.claim.epoch,
                awaken_dispatch_contract::DispatchOutcome::Done,
                &[]
            )
            .await
            .unwrap(),
        SettleOutcome::Applied
    );
    let mut report = awaken_dispatch_testkit::worker_http::activation("report");
    report.thread_id = root.clone();
    report.run_id = awaken_session_execution_contract::agent_report_run_id(&session, &child_run);
    let report_input = Message::text(
        MessageId::agent_thread_report(&child_run),
        Role::User,
        format!(
            "Message from agent child (thread {}):\ndone",
            child_thread.0
        ),
    );
    report.input = vec![report_input.clone()];
    let mut report = claim_dispatch(
        &authority,
        RunDispatch::new(report)
            .for_session(root.clone())
            .with_session_activity_epoch(2)
            .with_execution_scope(ExecutionScopeRef(ScopeId::from(scope))),
    )
    .await;
    report.operation.expected_thread_version = 3;
    report.operation.commit.messages.clear();
    report.operation.payload_hash = commit_payload_hash(&report.operation.commit).unwrap();
    let report_started = writer.commit_operation(report.clone()).await.unwrap();
    let before_report_input = timeline(&service, scope, &session, &page_query, None)
        .await
        .unwrap();
    let parent_evidence = run_evidence_at(
        &service,
        scope,
        &session,
        &parent_run,
        &before_report_input.snapshot,
    )
    .await
    .unwrap();
    assert_eq!(parent_evidence.messages.len(), 2);
    assert_eq!(
        parent_evidence.messages[1].target_first_commit,
        Some(report_started.commit_sequence)
    );
    assert_eq!(parent_evidence.messages[1].target_input_commit, None);
    let report_commit = writer
        .commit_operation(replay_terminal_commit(
            &report.claim,
            &root,
            1,
            4,
            vec![report_input],
        ))
        .await
        .unwrap();
    let with_report = service.capture_for(scope, &session).await.unwrap();
    let with_report = service
        .load_for(scope, &session, &with_report.id)
        .await
        .unwrap();
    assert_eq!(with_report.messages.len(), 2);
    assert_eq!(
        with_report.messages[0].target_input_commit,
        Some(child_terminal.commit_sequence)
    );
    assert_eq!(
        with_report.messages[1].target_input_commit,
        Some(report_commit.commit_sequence)
    );
    assert_eq!(with_report.messages[1].source_run, child_run);
    assert_eq!(
        reopened.load_for(scope, &session, &gap.id).await.unwrap(),
        frozen
    );
    assert_eq!(
        reopened
            .load_for(scope, &session, &after_child.id)
            .await
            .unwrap(),
        before_receipt
    );
    assert_eq!(
        reopened
            .load_for(scope, &session, &before_child.id)
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        reopened
            .load_for("foreign", &session, &gap.id)
            .await
            .unwrap_err(),
        awaken_session_application::replay::SessionReplayError::NotFound
    );
}

fn replay_terminal_commit(
    claim: &RunClaim,
    thread: &ThreadId,
    ordinal: u64,
    version: u64,
    messages: Vec<Message>,
) -> ClaimedCommitCommand {
    let commit = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::ended(claim.run_id.clone(), EndCause::NaturalEnd),
        true,
        messages,
        Vec::new(),
        Vec::new(),
    );
    ClaimedCommitCommand {
        claim: claim.clone(),
        operation: CommitOperation {
            operation_id: CommitOperationId {
                run_id: claim.run_id.clone(),
                ordinal,
            },
            expected_thread_version: version,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    }
}

fn replay_batch_commit(
    claim: &RunClaim,
    thread: &ThreadId,
    ordinal: u64,
    version: u64,
    batch: &awaken_tool_contract::tool_batch::ToolBatch,
) -> ClaimedCommitCommand {
    use awaken_agent_contract::agent::state::StateKey as _;
    let commit = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::running(claim.run_id.clone()),
        ordinal == 0,
        Vec::new(),
        vec![awaken_tool_contract::tool_batch::ActiveToolBatch::write(
            &Some(batch.clone()),
        )],
        Vec::new(),
    );
    ClaimedCommitCommand {
        claim: claim.clone(),
        operation: CommitOperation {
            operation_id: CommitOperationId {
                run_id: claim.run_id.clone(),
                ordinal,
            },
            expected_thread_version: version,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    }
}

async fn claim_dispatch(
    authority: &DurableRuntimeAuthority,
    request: RunDispatch,
) -> ClaimedCommitCommand {
    let run_id = request.run_id().clone();
    let commit = ThreadCommit::assemble(
        request.thread_id().clone(),
        RunDisposition::running(run_id.clone()),
        true,
        vec![Message::text(
            MessageId("atomic-message".into()),
            Role::Assistant,
            "committed",
        )],
        vec![StateCommand::set(
            awaken_agent_contract::agent::state::Scope::Run,
            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
            "step",
            serde_json::json!(1),
        )],
        Vec::new(),
    );
    let dispatch = authority.dispatch_store();
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim("previous-worker", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    ClaimedCommitCommand {
        claim: RunClaim::from(&claimed.lease),
        operation: CommitOperation {
            operation_id: CommitOperationId { run_id, ordinal: 0 },
            expected_thread_version: 0,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    }
}

async fn assert_commit_rows(pool: &PgPool, expected: i64) {
    for (table, statement) in [
        ("runtime_commit", "SELECT COUNT(*) FROM runtime_commit"),
        ("runtime_message", "SELECT COUNT(*) FROM runtime_message"),
        (
            "runtime_state_command",
            "SELECT COUNT(*) FROM runtime_state_command",
        ),
        ("runtime_event", "SELECT COUNT(*) FROM runtime_event"),
        (
            "runtime_run_record",
            "SELECT COUNT(*) FROM runtime_run_record",
        ),
        (
            "runtime_thread_version",
            "SELECT COUNT(*) FROM runtime_thread_version",
        ),
        (
            "runtime_commit_receipt",
            "SELECT COUNT(*) FROM runtime_commit_receipt",
        ),
    ] {
        let count: i64 = sqlx::query_scalar(statement).fetch_one(pool).await.unwrap();
        let expected = if table == "runtime_event" {
            expected * 2
        } else {
            expected
        };
        assert_eq!(count, expected, "{table}");
    }
}

#[cfg(test)]
#[tokio::test]
async fn selected_postgres_writer_rolls_back_late_failure_and_replays_on_one_connection() {
    let Some((_, authority, pool)) = fixture().await else {
        return;
    };
    let command = initial_claim(&authority).await;
    let writer = authority.clone().run_commits();

    // This is the last INSERT, after the commit, message, event and Run state.
    sqlx::query("ALTER TABLE runtime_commit_receipt ADD CONSTRAINT reject_receipt CHECK (false)")
        .execute(&pool)
        .await
        .unwrap();
    let error = writer.commit_operation(command.clone()).await.unwrap_err();
    assert!(error.message.contains("reject_receipt"), "{error}");
    assert_commit_rows(&pool, 0).await;
    let thread = &command.operation.commit.thread_id;
    let run = &command.claim.run_id;
    let recovery = authority.shared_recovery_source().unwrap();
    let empty = recovery.recovery_snapshot(thread, run).await.unwrap();
    assert!(empty.messages.is_empty());
    assert_eq!(empty.thread_version, 0);

    sqlx::query("ALTER TABLE runtime_commit_receipt DROP CONSTRAINT reject_receipt")
        .execute(&pool)
        .await
        .unwrap();
    let first = writer.commit_operation(command.clone()).await.unwrap();
    assert_eq!(first.thread_version, 1);
    let replay = writer.commit_operation(command.clone()).await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.commit_sequence, first.commit_sequence);
    assert_eq!(replay.thread_version, first.thread_version);
    let snapshot = recovery.recovery_snapshot(thread, run).await.unwrap();
    assert_eq!(snapshot.messages, command.operation.commit.messages);
    assert_eq!(snapshot.state, command.operation.commit.state);
    let page = authority
        .shared_lifecycle_feed()
        .unwrap()
        .events_after(RunLifecycleCursor(0), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    // The process-wide reader must not accumulate every Worker's transcript
    // in the synchronous projection reserved for local execution.
    assert_eq!(
        authority.postgres_commit.as_ref().unwrap().commit_count(),
        0
    );
    assert_commit_rows(&pool, 1).await;
}

async fn wait_for_blocker(pool: &PgPool, waiting: i32, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $2 = ANY(pg_blocking_pids($1))")
                .bind(waiting)
                .bind(blocker)
                .fetch_one(pool)
                .await
                .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected PostgreSQL lock ordering was not observed");
}

#[cfg(test)]
async fn pause_before_receipt(
    reader: &PgPool,
) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut blocker = reader.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(i64::from(blocker_pid))
        .execute(&mut *blocker)
        .await
        .unwrap();
    // blocker_pid is the database-returned integer PID; no request value enters DDL.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION pause_receipt() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN PERFORM pg_advisory_xact_lock({blocker_pid}); RETURN NEW; END $$"
    )))
    .execute(reader)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER pause_receipt BEFORE INSERT ON runtime_commit_receipt \
         FOR EACH ROW EXECUTE FUNCTION pause_receipt()",
    )
    .execute(reader)
    .await
    .unwrap();

    (blocker, blocker_pid)
}

#[tokio::test]
async fn selected_postgres_writer_serializes_commit_and_takeover() {
    let Some((url, authority, pool)) = fixture().await else {
        return;
    };
    let reader = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let contender_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let contender = awaken_dispatch::with_existing_postgres_dispatch_pool(contender_pool.clone())
        .await
        .unwrap();
    let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let contender_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&contender_pool)
        .await
        .unwrap();
    let command = initial_claim(&authority).await;
    let old_claim = command.claim.clone();
    let binding = DispatchBinding::coordinator(authority.clone());
    let writer = binding.local_run_commits().unwrap().clone();

    // Pause at the receipt INSERT, after the transaction has written the
    // result. Observe both actual PostgreSQL wait edges before unblocking it.
    let (blocker, blocker_pid) = pause_before_receipt(&reader).await;

    let applying = tokio::spawn({
        let writer = writer.clone();
        let command = command.clone();
        async move { writer.commit_operation(command).await }
    });
    wait_for_blocker(&reader, writer_pid, blocker_pid).await;
    let taking_over = tokio::spawn(async move {
        assert_eq!(
            contender.relinquish_claim(&old_claim).await.unwrap(),
            SettleOutcome::Applied
        );
        contender
            .claim("successor-worker", 60_000, &Default::default())
            .await
            .unwrap()
            .unwrap()
    });
    wait_for_blocker(&reader, contender_pid, writer_pid).await;
    assert_commit_rows(&reader, 0).await;
    blocker.commit().await.unwrap();
    let first = applying.await.unwrap().unwrap();
    let successor = RunClaim::from(&taking_over.await.unwrap().lease);
    assert!(successor.epoch > command.claim.epoch);

    // Once takeover wins, even an exact duplicate from the previous epoch is
    // fenced. The current claim can recover the original receipt.
    let error = writer.commit_operation(command.clone()).await.unwrap_err();
    assert_eq!(error.kind, RunErrorKind::BadRequest);
    assert_eq!(error.message, "run claim is stale");
    assert_eq!(error.code, awaken_dispatch_contract::COMMIT_FENCED);
    let replay = writer
        .commit_operation(ClaimedCommitCommand {
            claim: successor,
            operation: command.operation,
        })
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.commit_sequence, first.commit_sequence);
    assert_commit_rows(&reader, 1).await;
}

#[tokio::test]
async fn selected_postgres_cold_reads_freeze_pages_and_resume_lifecycle_after_takeover() {
    let Some((url, authority, _)) = fixture().await else {
        return;
    };
    let mut command = initial_claim(&authority).await;
    command.operation.commit.messages.push(Message::text(
        MessageId("second".into()),
        Role::Assistant,
        "frozen second",
    ));
    command.operation.payload_hash = commit_payload_hash(&command.operation.commit).unwrap();
    let thread = command.operation.commit.thread_id.clone();
    let run = command.claim.run_id.clone();
    let writer = authority.clone().run_commits();
    let first = writer.commit_operation(command.clone()).await.unwrap();

    let mut stale_version = command.clone();
    stale_version.operation.operation_id.ordinal += 1;
    let failure = writer.commit_operation(stale_version).await.unwrap_err();
    assert_eq!(failure.kind, RunErrorKind::Conflict);
    assert_eq!(failure.code, awaken_dispatch_contract::COMMIT_CONFLICT);

    let mut reused_identity = command.clone();
    reused_identity
        .operation
        .commit
        .messages
        .push(Message::text(
            MessageId("changed".into()),
            Role::Assistant,
            "different payload",
        ));
    reused_identity.operation.payload_hash =
        commit_payload_hash(&reused_identity.operation.commit).unwrap();
    let failure = writer.commit_operation(reused_identity).await.unwrap_err();
    assert_eq!(
        failure.code,
        awaken_dispatch_contract::COMMIT_IDENTITY_MISMATCH
    );

    let mut request = TranscriptPageRequest {
        thread_id: thread.clone(),
        view: TranscriptView::RawCommitted,
        snapshot: None,
        start: 0,
        limit: std::num::NonZeroUsize::MIN,
    };
    let page = authority
        .read_transcript_page(&thread.0, &request)
        .await
        .unwrap();
    assert_eq!(page.messages, command.operation.commit.messages[..1]);
    assert_eq!(page.snapshot.end_seq, 2);
    request.start = page.next_start().unwrap();
    request.snapshot = Some(page.snapshot.clone());

    let lifecycle = authority.shared_lifecycle_feed().unwrap();
    let running = lifecycle
        .events_after(RunLifecycleCursor(0), 1)
        .await
        .unwrap();
    assert_eq!(running.events.len(), 1);
    assert_eq!(running.events[0].kind, RunLifecycleEventKind::Running);
    assert_eq!(
        running.events[0].source_commit_cursor,
        first.commit_sequence
    );

    let dispatch = authority.dispatch_store();
    assert_eq!(
        dispatch.relinquish_claim(&command.claim).await.unwrap(),
        SettleOutcome::Applied
    );
    let successor = dispatch
        .claim("replay-successor", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&successor.lease);
    assert!(claim.epoch > command.claim.epoch);
    assert!(writer.commit_operation(command.clone()).await.is_err());

    command.claim = claim.clone();
    let duplicate = writer.commit_operation(command.clone()).await.unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.commit_sequence, first.commit_sequence);
    let empty = lifecycle
        .events_after(running.next_cursor, 1)
        .await
        .unwrap();
    assert!(empty.events.is_empty());
    assert_eq!(empty.next_cursor, running.next_cursor);

    let commit = ThreadCommit::assemble(
        thread,
        RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
        true,
        vec![Message::text(
            MessageId("third".into()),
            Role::Assistant,
            "new tail",
        )],
        Vec::new(),
        Vec::new(),
    );
    let terminal = ClaimedCommitCommand {
        claim,
        operation: CommitOperation {
            operation_id: CommitOperationId::new(run, 1),
            expected_thread_version: first.thread_version,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    };
    let applied = writer.commit_operation(terminal).await.unwrap();

    let peer = PostgresCommitCoordinator::connect_existing(&url, 1)
        .await
        .unwrap();
    let frozen = peer.authoritative_transcript_page(&request).await.unwrap();
    assert_eq!(frozen.snapshot, page.snapshot);
    assert_eq!(frozen.messages, command.operation.commit.messages[1..]);
    assert_eq!(frozen.next_start(), None);
    request.snapshot = None;
    request.start = 2;
    let latest = peer.authoritative_transcript_page(&request).await.unwrap();
    assert_eq!(latest.snapshot.end_seq, 3);
    assert_eq!(latest.messages[0].text_content(), "new tail");
    request.snapshot = Some(frozen.snapshot);
    request.view = TranscriptView::ContextEffective;
    assert!(peer.authoritative_transcript_page(&request).await.is_err());

    let replay = peer.events_after(running.next_cursor, 1).await.unwrap();
    assert_eq!(replay.events.len(), 1);
    assert_eq!(replay.events[0].kind, RunLifecycleEventKind::Completed);
    assert_eq!(
        replay.events[0].source_commit_cursor,
        applied.commit_sequence
    );
    let end = peer.events_after(replay.next_cursor, 1).await.unwrap();
    assert!(end.events.is_empty());
    assert_eq!(end.next_cursor, replay.next_cursor);
}

#[cfg(test)]
#[tokio::test]
async fn selected_postgres_writer_fences_after_waiting_for_a_new_epoch() {
    let Some((url, authority, pool)) = fixture().await else {
        return;
    };
    let command = initial_claim(&authority).await;
    let writer = authority.clone().run_commits();
    let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let reader = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let mut takeover = reader.begin().await.unwrap();
    let takeover_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *takeover)
        .await
        .unwrap();
    // Freeze an exact successor's durable row update before it commits, so the
    // old request observes the new epoch only after waiting for that transaction.
    assert_eq!(sqlx::query(
        "UPDATE runtime_dispatch SET lease_epoch = lease_epoch + 1, lease_owner = $1 WHERE run_id = $2"
    ).bind("successor-worker").bind(&command.claim.run_id.0)
        .execute(&mut *takeover).await.unwrap().rows_affected(), 1);
    let applying = tokio::spawn(async move { writer.commit_operation(command).await });
    wait_for_blocker(&reader, writer_pid, takeover_pid).await;
    takeover.commit().await.unwrap();
    let error = applying.await.unwrap().unwrap_err();
    assert_eq!(error.kind, RunErrorKind::BadRequest);
    assert_eq!(error.message, "run claim is stale");
    assert_eq!(error.code, awaken_dispatch_contract::COMMIT_FENCED);
    assert_commit_rows(&reader, 0).await;
}

#[tokio::test]
async fn selected_postgres_writer_rolls_back_when_the_commit_connection_dies() {
    let Some((url, authority, pool)) = fixture().await else {
        return;
    };
    let reader = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let command = initial_claim(&authority).await;
    let writer = authority.run_commits();
    let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (blocker, blocker_pid) = pause_before_receipt(&reader).await;
    let applying = tokio::spawn({
        let writer = writer.clone();
        let command = command.clone();
        async move { writer.commit_operation(command).await }
    });
    wait_for_blocker(&reader, writer_pid, blocker_pid).await;

    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(writer_pid)
        .fetch_one(&reader)
        .await
        .unwrap();
    assert!(terminated);
    assert!(
        tokio::time::timeout(Duration::from_secs(8), applying)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    blocker.commit().await.unwrap();
    assert_commit_rows(&reader, 0).await;
    let applied = writer.commit_operation(command).await.unwrap();
    assert!(!applied.duplicate);
    assert_commit_rows(&reader, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_local_writers_commit_child_history_in_the_parent_session() {
    for kind in [StoreKind::Fs, StoreKind::Sqlite] {
        let root = tempfile::tempdir().unwrap();
        let mut storage = tests::test_storage();
        storage.store = kind;
        storage.storage_dir = Some(root.path().to_path_buf());
        let authority = DurableRuntimeAuthority::open(
            &storage,
            SchemaAccess::Migrate,
            awaken_time::system_clock(),
        )
        .await
        .unwrap();
        let session = ThreadId("parent-session".into());
        let request = RunDispatch::new(awaken_dispatch_testkit::worker_http::activation("child"))
            .for_session(session.clone());
        let command = claim_dispatch(&authority, request).await;
        let thread = &command.operation.commit.thread_id;
        let opened = authority.open_commit(&session.0).await.unwrap();
        let writer = authority.clone().run_commits();

        let mut wrong_thread = command.clone();
        wrong_thread.operation.commit.thread_id = session;
        wrong_thread.operation.payload_hash =
            commit_payload_hash(&wrong_thread.operation.commit).unwrap();
        let error = writer.commit_operation(wrong_thread).await.unwrap_err();
        assert_eq!(error.kind, RunErrorKind::BadRequest);
        assert!(
            error
                .message
                .contains("does not match claimed dispatch thread")
        );

        let receipt = writer.commit_operation(command.clone()).await.unwrap();
        assert_eq!(receipt.thread_version, 1);
        let replay = writer.commit_operation(command.clone()).await.unwrap();
        assert!(replay.duplicate);
        assert_eq!(receipt.commit_sequence, replay.commit_sequence);
        let snapshot = opened
            .recovery_snapshot(thread, &command.claim.run_id)
            .await
            .unwrap();
        assert_eq!(snapshot.messages, command.operation.commit.messages);
        assert_eq!(
            opened
                .events_after(RunLifecycleCursor(0), 10)
                .await
                .unwrap()
                .events
                .len(),
            1
        );
        assert_eq!(snapshot.state, command.operation.commit.state);
        assert!(!authority.durable_thread_exists(&thread.0).await.unwrap());
        assert!(authority.shared_lifecycle_feed().is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_process_dependency_reads_keep_the_original_claim_authority() {
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_dispatch_contract::{DispatchClaimOwnership as _, DispatchCommitAuthority as _};
    use awaken_time::WallClockExt as _;
    use sqlx::Executor as _;
    // P1/P2 normal migrate and verify startup share a bounded dependency-read
    // pool. Every authority slot is occupied by an actual CommitEpochGuard,
    // yet Registry roster/exact reads and cold Runtime recovery/projection
    // complete. P3 malformed committed input fails recovery while the original
    // guard still owns its slot; explicit release permits the next exact-claim
    // operation. SELECT-only recovery cannot wait on its own Dispatch row lock.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let admin = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
    admin
        .execute("DROP SCHEMA IF EXISTS t_coordinator_dependency_reads CASCADE")
        .await
        .unwrap();
    admin
        .execute("CREATE SCHEMA t_coordinator_dependency_reads")
        .await
        .unwrap();
    let separator = if url.contains('?') { '&' } else { '?' };
    let url = format!("{url}{separator}options=-c%20search_path%3Dt_coordinator_dependency_reads");
    for (capacity, verify) in [(1, false), (2, true)] {
        let mut storage = tests::test_storage();
        storage.store = StoreKind::Postgres;
        storage.dispatch_backend = DispatchBackend::Postgres;
        storage.database_url = Some(url.clone());
        storage.postgres_max_connections = std::num::NonZeroU32::new(capacity).unwrap();
        let clock = awaken_time::system_clock();
        let persistence = if verify {
            crate::coordinator_persistence::open_existing(&storage, clock.clone()).await
        } else {
            crate::coordinator_persistence::open(&storage, clock.clone()).await
        }
        .unwrap();
        let pool = persistence.postgres_pool.as_ref().unwrap();
        let seed = PostgresCommitCoordinator::with_existing_pool(pool.clone())
            .await
            .unwrap();
        let directory = &persistence.worker_directory;
        let registered = directory
            .register(
                awaken_worker_contract::WorkerRegistration {
                    worker_id: format!("pool-worker-{capacity}"),
                    incarnation_id: "boot".into(),
                    manifest: awaken_dispatch_testkit::worker_http::current_dispatch_codec_manifest(
                    ),
                    create_ledger: None,
                },
                clock.now_ms(),
                60_000,
            )
            .await
            .unwrap();
        let dispatch = persistence.runtime_authority.dispatch_store();
        let mut claims = Vec::new();
        let mut threads = Vec::new();
        let mut guards = Vec::new();
        for ordinal in 0..capacity {
            let request = RunDispatch::new(awaken_dispatch_testkit::worker_http::activation(
                &format!("pool-{capacity}-{ordinal}"),
            ));
            let thread = request.thread_id().clone();
            seed.commit(ThreadCommit::assemble(
                thread.clone(),
                RunDisposition::ended(
                    RunId(format!("history-{capacity}-{ordinal}")),
                    EndCause::NaturalEnd,
                ),
                true,
                vec![Message::text(
                    MessageId("pool-message".into()),
                    Role::Assistant,
                    "prefix",
                )],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
            let run = request.run_id().clone();
            dispatch.enqueue(request).await.unwrap();
            let claimed = dispatch
                .claim_run(
                    &run,
                    &registered.snapshot.identity.lease_owner(),
                    60_000,
                    &Default::default(),
                )
                .await
                .unwrap()
                .unwrap();
            claims.push(RunClaim::from(&claimed.lease));
            threads.push(thread);
        }
        for claim in &claims {
            guards.push(dispatch.lock_commit_epoch(claim).await.unwrap().unwrap());
        }
        assert_eq!(
            pool.num_idle(),
            0,
            "P1 every original authority slot is guarded"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            assert!(
                directory.list().await.unwrap().contains(&registered),
                "P1 roster"
            );
            assert_eq!(
                directory
                    .current(&registered.snapshot.identity.worker_id)
                    .await
                    .unwrap(),
                Some(registered.clone()),
                "P1 current"
            );
            assert_eq!(
                directory
                    .incarnation(&registered.snapshot.identity)
                    .await
                    .unwrap(),
                Some(registered.clone()),
                "P1 exact incarnation"
            );
            for (thread, claim) in threads.iter().zip(&claims) {
                let recovery = persistence
                    .run_recovery
                    .as_ref()
                    .unwrap()
                    .recovery_snapshot(thread, &claim.run_id)
                    .await
                    .unwrap();
                assert_eq!(recovery.messages.len(), 1, "P2 cold prefix truth");
                let view = persistence
                    .runtime_authority
                    .open_commit(&thread.0)
                    .await
                    .unwrap();
                assert_eq!(
                    view.authoritative_committed_messages(thread).await.unwrap(),
                    recovery.messages,
                    "P2 derived projection read"
                );
                assert!(
                    persistence
                        .runtime_authority
                        .durable_thread_exists(&thread.0)
                        .await
                        .unwrap(),
                    "P2 authoritative collision read"
                );
            }
        })
        .await
        .expect("P1/P2 actual row guards cannot block their dependency observations");
        sqlx::query("UPDATE t_coordinator_dependency_reads.runtime_message SET data = 'null'::jsonb WHERE thread_id = $1")
            .bind(&threads[0].0).execute(&admin).await.unwrap();
        assert!(
            persistence
                .run_recovery
                .as_ref()
                .unwrap()
                .recovery_snapshot(&threads[0], &claims[0].run_id)
                .await
                .is_err(),
            "P3 malformed prefix fails closed"
        );
        for guard in guards {
            if verify {
                drop(guard);
            } else {
                guard.release().await.unwrap();
            }
        }
        for claim in &claims {
            assert!(
                tokio::time::timeout(Duration::from_secs(5), dispatch.claim_is_current(claim))
                    .await
                    .expect("P3 original authority slot released")
                    .unwrap()
            );
        }
    }
    admin
        .execute("DROP SCHEMA t_coordinator_dependency_reads CASCADE")
        .await
        .unwrap();
    admin.close().await;
}
