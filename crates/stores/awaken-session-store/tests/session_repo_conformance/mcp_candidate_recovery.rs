//! Root-derived hot MCP demand survives independent clients and reconstruction.
//! Both external and already-bound self-hosted placement run this table.
//! These are store/index tests, not physical MCP execution or crash receipts.
//!
//! | Rule | Retained root | Expected candidate / Worker demand |
//! |---|---|---|
//! | M0 | published v1, Idle, original Worker | absent / one |
//! | M1 | replacement v2 committed, Idle | exact root / one |
//! | M2 | same v2 pending, admitted activity Running | absent / one |
//! | M3 | final activity settled back to Idle | exact root / one |
//! | M4 | close and reopen independent clients | same M2/M3 root and lease |
//! | M5 | old index hides M3 | prove hidden, operational reopen restores it |
//! | M6 | PostgreSQL server opener sees M5 | explicit refusal, no index repair |
use super::*;
use awaken_session_application::SessionStore;
use awaken_session_contract::{
    PersistedSession, SessionActivityAdmissionPlan, SessionAggregate, SessionMutationResult,
    SessionRepository as _,
};
use awaken_session_types::SessionId;

const SCOPE: &str = "mcp-recovery-workspace";
const SESSION: &str = "mcp-recovery-session";
const STALE_INDEX: &str = "UPDATE managed_session_reconciliation_work SET preparation_candidate=0 WHERE session_id='mcp-recovery-session'";

fn mcp_draft(version: &str) -> awaken_session_configuration_contract::mcp::McpAttachmentDraft {
    awaken_session_configuration_contract::mcp::McpAttachmentDraft {
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(format!(
            "https://mcp.example/{version}"
        ))
        .unwrap(),
        credential: None,
        prompts_as_skills: false,
        origin: awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
    }
}

async fn seed<R: SessionStore>(repo: &R, self_hosted: bool) -> PersistedSession {
    let template = session(SESSION, "pending hot MCP");
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        template.baseline
    else {
        unreachable!()
    };
    let mut environment = baseline.environment;
    environment.self_hosted = self_hosted;
    // Compile fresh facts so placement and self-hosting participate in the
    // real baseline fingerprint, rather than mutating a frozen fingerprint.
    let baseline =
        awaken_session_configuration_contract::SessionBaseline::compile_with_execution_model_ref(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment,
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
                mcp_authoring: baseline.mcp_authoring,
                agent_id: baseline.agent_id,
                agent_revision: baseline.agent_revision,
                model: baseline.model,
                model_override: baseline.model_override,
                runtime: baseline.runtime,
                delegate_ids: baseline.delegate_ids,
                toolsets: baseline.toolsets,
                mounts: baseline.mounts,
                env: baseline.env,
                prompts: baseline.prompts,
                transcript_prefix: baseline.transcript_prefix,
            },
            baseline.execution_model_ref,
        );
    // The shared fixture deliberately decodes a historical missing environment
    // field. This scenario starts from a fresh canonical root: an operational
    // reopen must repair only the derived index, never migrate the fixture.
    let mut mcp =
        awaken_session_configuration_contract::mcp::SessionMcpAttachmentSet::from_initial(
            vec![mcp_draft("v1")],
            None,
        )
        .unwrap();
    // A separate canonical Requested root is the unbound-placement control.
    // Never manufacture it by erasing the lease from a published MCP owner.
    let unbound = PersistedSession::frozen_with_budget(
        "unbound-mcp-control",
        baseline.clone(),
        Default::default(),
        mcp.clone(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    assert!(unbound.realization.is_none() && unbound.mcp.needs_realization());
    assert_eq!(
        unbound.execution,
        awaken_session_contract::SessionExecutionState::Preparing
    );
    assert!(
        !unbound.is_run_free_preparation_candidate(),
        "a fresh Preparing root has not requested run-free preparation"
    );
    let mut initial = SessionAggregate::try_from(unbound).unwrap();
    assert!(initial.assignment().request_run_free_preparation().unwrap());
    assert_eq!(
        initial.snapshot().is_run_free_preparation_candidate(),
        !self_hosted,
        "explicit unbound initial preparation stays WorkQueue-owned when self-hosted"
    );
    let original = mcp.attachments[0].generation.clone();
    mcp.claim_realization(
        &original,
        awaken_session_configuration_contract::mcp::McpRealizationClaim {
            realization_id: "original-mcp".into(),
            runtime_incarnation: "original-worker:1:boot".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: u64::MAX,
            stage_idempotency_key: "original-mcp-stage".into(),
        },
    )
    .unwrap();
    mcp.activate(&original, "original-mcp").unwrap();
    mcp.acknowledge_publication(&original, "original-mcp")
        .unwrap();
    assert!(!mcp.needs_realization());
    let mut root = PersistedSession::frozen_with_budget(
        template.session_id,
        baseline,
        Default::default(),
        mcp,
        template.title,
        template.metadata,
        template.tools,
        template.budget,
    );
    root.execution = awaken_session_contract::SessionExecutionState::Idle;
    root.realization = Some(
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "original-worker".into(),
            runtime_incarnation: "original-worker:1:boot".into(),
            epoch: 3,
            expires_at_unix_ms: u64::MAX,
        },
    );
    assert!(!root.realization_progress().run_free_initial_preparation());
    assert!(!root.is_run_free_preparation_candidate());
    let initial = create_session(repo, SCOPE, root, Vec::new()).await;
    observe(repo, &initial, false).await;

    // Commit a real successor intent after the original generation was
    // published. An initially Requested attachment alone is not this case.
    let mut updated = initial.clone();
    let plan = updated
        .mcp
        .request_full_replacement(vec![mcp_draft("v2")], None)
        .unwrap();
    assert_eq!(plan.requested.len(), 1);
    assert_eq!(plan.requested[0].version(), 2);
    assert_eq!(updated.mcp.attachments[0], initial.mcp.attachments[0]);
    let pending = replace_session(repo, SCOPE, updated, "mcp-replace-v2", Vec::new()).await;
    assert_eq!(pending.realization, initial.realization);
    assert!(pending.is_run_free_preparation_candidate());
    pending
}

async fn observe<R: SessionStore>(repo: &R, expected: &PersistedSession, candidate: bool) {
    assert_eq!(
        repo.get(&SessionId::from(SESSION)).await.unwrap(),
        *expected,
        "the whole canonical root is retained"
    );
    assert_eq!(repo.owner(&SessionId::from(SESSION)).await.unwrap(), SCOPE);
    assert_eq!(
        repo.worker_demand().await.unwrap(),
        1,
        "the retained physical worker is not an extra replica"
    );
    let scan = repo.preparation_candidates(None).await.unwrap();
    assert!(scan.quarantined.is_empty() && scan.next_cursor.is_none());
    let roots: Vec<_> = scan
        .sessions
        .into_iter()
        .map(|scoped| {
            assert_eq!(scoped.workspace_id, SCOPE);
            scoped.session
        })
        .collect();
    assert_eq!(
        roots,
        if candidate {
            vec![expected.clone()]
        } else {
            vec![]
        },
        "root-derived pending MCP candidate projection"
    );
}

async fn activity<R: SessionStore>(repo: &R, busy: bool) -> PersistedSession {
    let before = repo.get(&SessionId::from(SESSION)).await.unwrap();
    let mut root = SessionAggregate::try_from(before.clone()).unwrap();
    if busy {
        assert_eq!(
            root.activity().open(None, false, false, 10, 0, None),
            SessionActivityAdmissionPlan::Open(1)
        );
    } else {
        assert_eq!(root.activity().settle_activity_epoch(1u64), Some(true));
        root.activity().finish_running(20).unwrap();
    }
    let key = if busy { "mcp-busy" } else { "mcp-idle" };
    let mutation = root.compile_mutation(
        SCOPE,
        record(
            key,
            &awaken_session_contract::SessionMutationPayload::Replace(root.snapshot().clone()),
        ),
        Vec::new(),
        if busy { 10 } else { 20 },
    );
    assert!(matches!(
        repo.commit(SCOPE, mutation).await.unwrap(),
        SessionMutationResult::Applied { .. }
    ));
    let after = repo.get(&SessionId::from(SESSION)).await.unwrap();
    assert_eq!(
        after.realization, before.realization,
        "logical activity never reassigns a physical worker"
    );
    assert_eq!(
        after.mcp, before.mcp,
        "no pending generation is forgotten or completed"
    );
    assert_eq!(after.revision.0, before.revision.0 + 1);
    after
}

#[tokio::test]
async fn sqlite_hot_mcp_candidates_survive_busy_idle_reopen_and_operational_index_repair() {
    for self_hosted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.db");
        let path = file.to_str().unwrap();
        let a = SqliteManagedSessionRepository::open(path).await.unwrap();
        let first = seed(&a, self_hosted).await;
        let b = SqliteManagedSessionRepository::open(path).await.unwrap();
        observe(&b, &first, true).await;
        let busy = activity(&a, true).await;
        observe(&b, &busy, false).await;
        a.close().await;
        b.close().await;
        drop((a, b));

        let a = SqliteManagedSessionRepository::open(path).await.unwrap();
        let b = SqliteManagedSessionRepository::open(path).await.unwrap();
        observe(&b, &busy, false).await;
        let idle = activity(&a, false).await;
        observe(&b, &idle, true).await;
        a.close().await;
        b.close().await;
        drop((a, b));

        // A genuine old projection says initial-preparation-only. The operational
        // opener must reconstruct the broader index without changing the root.
        let raw = raw_sqlite(&file).await;
        assert_eq!(
            sqlx::query(STALE_INDEX)
                .execute(&raw)
                .await
                .unwrap()
                .rows_affected(),
            1
        );
        let absent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM managed_session_reconciliation_work WHERE preparation_candidate=1",
    )
    .fetch_one(&raw)
    .await
    .unwrap();
        assert_eq!(
            absent, 0,
            "the stale-index control actually hides the pending work"
        );
        raw.close().await;
        let restored = SqliteManagedSessionRepository::open(path).await.unwrap();
        observe(&restored, &idle, true).await;
        restored.close().await;
    }
}

async fn second_pool(url: &str, schema: &str) -> sqlx::PgPool {
    let schema = schema.to_owned();
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _| {
            let schema = schema.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(schema)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .unwrap()
}

#[tokio::test]
async fn postgres_hot_mcp_candidates_survive_busy_idle_new_pools_and_operational_index_repair() {
    for self_hosted in [false, true] {
        let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
        let (pa, schema) = owned_postgres_pool(&url, "mcp_recovery").await;
        let pb = second_pool(&url, &schema).await;
        let a = awaken_session_store::PostgresManagedSessionRepository::with_pool(pa.clone())
            .await
            .unwrap();
        let first = seed(&a, self_hosted).await;
        let b =
            awaken_session_store::PostgresManagedSessionRepository::with_existing_pool(pb.clone())
                .await
                .unwrap();
        observe(&b, &first, true).await;
        let busy = activity(&a, true).await;
        observe(&b, &busy, false).await;
        drop((a, b));
        pa.close().await;
        pb.close().await;

        let pa = second_pool(&url, &schema).await;
        let pb = second_pool(&url, &schema).await;
        let a =
            awaken_session_store::PostgresManagedSessionRepository::with_existing_pool(pa.clone())
                .await
                .unwrap();
        let b =
            awaken_session_store::PostgresManagedSessionRepository::with_existing_pool(pb.clone())
                .await
                .unwrap();
        observe(&b, &busy, false).await;
        let idle = activity(&a, false).await;
        observe(&b, &idle, true).await;
        assert_eq!(
            sqlx::query(STALE_INDEX)
                .execute(&pa)
                .await
                .unwrap()
                .rows_affected(),
            1
        );
        assert!(
            b.preparation_candidates(None)
                .await
                .unwrap()
                .sessions
                .is_empty(),
            "the stale-index control actually hides pending work"
        );
        drop((a, b));
        let refused =
            awaken_session_store::PostgresManagedSessionRepository::with_existing_pool(pb.clone())
                .await;
        assert!(
            matches!(refused, Err(message) if message.contains("reconciliation index requires operational migration rebuild")),
            "server mode refuses this exact stale projection instead of repairing it"
        );
        let still_stale: i64 = sqlx::query_scalar("SELECT preparation_candidate::bigint FROM managed_session_reconciliation_work WHERE session_id=$1").bind(SESSION).fetch_one(&pa).await.unwrap();
        assert_eq!(
            still_stale, 0,
            "the refused read-only opener did not mutate it"
        );
        let repaired =
            awaken_session_store::PostgresManagedSessionRepository::with_pool(pa.clone())
                .await
                .unwrap();
        let verified =
            awaken_session_store::PostgresManagedSessionRepository::with_existing_pool(pb.clone())
                .await
                .unwrap();
        observe(&repaired, &idle, true).await;
        observe(&verified, &idle, true).await;
        drop((repaired, verified));
        pb.close().await;
        close_postgres_pool(pa, &schema).await;
    }
}
