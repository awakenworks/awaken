// @formal evidence webhook.committed_outbox_row_is_recoverable
// @formal evidence webhook.lifecycle_commit_and_outbox_are_atomic
use awaken_session_repository_contract::LifecycleOutbox as _;
use std::collections::BTreeMap;
use std::time::Duration;

use awaken_reliability_testkit::CrashProcess;
use awaken_session_repository_contract::SessionRepository;
use awaken_session_state_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionExecutionState,
    SessionMutationPayload,
};
use awaken_session_store::SqliteManagedSessionRepository;

const CHILD_MODE: &str = "AWAKEN_SESSION_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_SESSION_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_SESSION_CRASH_MARKER";

fn session() -> PersistedSession {
    let holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Worker,
        "awaken.worker",
    );
    let environment = awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "env".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "env-1".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization: awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: holder.clone(),
            mcp_holder: holder.clone(),
            resource_holder: holder,
        },
    };
    PersistedSession {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        session_id: "sesn_process_crash".into(),
        revision: Default::default(),
        baseline: awaken_session_configuration_contract::SessionBaselineState::Frozen(
            awaken_session_configuration_contract::SessionBaseline::compile(
                awaken_session_configuration_contract::SessionBaselineInputs {
                    environment,
                    runtime_placement:
                        awaken_session_configuration_contract::SessionRuntimePlacement::Local,
                    mcp_authoring: Default::default(),
                    agent_id: "assistant".into(),
                    agent_revision: None,
                    model_override: None,
                    model: "model".into(),
                    runtime: None,
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                },
            ),
        ),
        title: None,
        metadata: BTreeMap::new(),
        tools: Default::default(),
        budget: Default::default(),
        event_batches: Vec::new(),
        activity_epoch: 0,
        active_activity_epochs: Default::default(),
        running_interval: None,
        closed_runtime_intervals: Vec::new(),
        runtime_active_millis: 0,
        usage_cursor: Default::default(),
        environment: Default::default(),
        mcp: Default::default(),
        resources: Default::default(),
        realization: None,
        realization_progress: Default::default(),
        execution: SessionExecutionState::Idle,
        disposition: Default::default(),
        terminal_cleanup: Default::default(),
    }
}

fn fact() -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: "session:sesn_process_crash:created".into(),
        object_id: "sesn_process_crash".into(),
        workspace_id: Some("ws_a".into()),
        event_type: "session.status_idled".into(),
        timestamp: 1_700_000_000,
        runtime_interval: None,
    }
}

#[tokio::test]
async fn session_commit_survives_process_kill_before_notification() {
    // Cause/effect decision table: C1=create returns after the aggregate and
    // lifecycle fact commit; C2=the process is killed before notification;
    // C3=a fresh repository instance reopens the same database; C4=completion
    // is retried. R1=C1+C2+C3 -> E1 exact revision-one aggregate and exact
    // pending fact are both visible; R2=R1+C4 -> E2 the first completion
    // retires the fact and the replay is idempotent. The child marker is only
    // the kill boundary; database durability is observed exclusively through
    // the fresh production adapter after the child process has terminated.
    if std::env::var_os(CHILD_MODE).is_some() {
        let db = std::env::var(DB_PATH).unwrap();
        let marker = std::env::var(MARKER_PATH).unwrap();
        let repo = SqliteManagedSessionRepository::open(&db).unwrap();
        let value = session();
        let payload = SessionMutationPayload::Replace(value.clone());
        repo.create(
            "ws_a",
            value,
            IdempotencyRecord {
                key: "test:process-crash:create".into(),
                payload_hash: payload.stable_hash(),
            },
            vec![fact()],
        )
        .await
        .unwrap();
        std::fs::write(marker, b"committed").unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        panic!("parent failed to kill crash child");
    }

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sessions.db");
    let marker = dir.path().join("committed.marker");
    let status = CrashProcess::new(
        "session_commit_survives_process_kill_before_notification",
        &marker,
    )
    .env(CHILD_MODE, "1")
    .env(DB_PATH, &db)
    .env(MARKER_PATH, &marker)
    .run()
    .expect("child reaches the post-commit boundary and is killed");
    assert!(!status.success());

    let repo = SqliteManagedSessionRepository::open(db.to_str().unwrap()).unwrap();
    let mut expected = session();
    expected.revision = awaken_session_types::SessionRevision(1);
    assert_eq!(
        repo.get(&awaken_session_types::SessionId::from("sesn_process_crash"))
            .await,
        Ok(expected)
    );
    assert_eq!(repo.pending_lifecycle().await.unwrap(), vec![fact()]);
    repo.complete_lifecycle(&fact().id).await.unwrap();
    repo.complete_lifecycle(&fact().id).await.unwrap();
    assert!(repo.pending_lifecycle().await.unwrap().is_empty());
}
